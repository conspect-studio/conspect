// Per-consumer ABR tests.
//
// The selector under test is the PINNED @moq/watch Video.Source, driven exactly as
// production drives it: an AbrSampler writes `Player.in.target` (here the same signal the
// real `Video.Source` reads) and the assertions read `source.out.track` / `out.available` /
// `out.config`. The network is modelled the way a live MoQ consumer experiences it —
// DEMAND-LIMITED delivery: a viewer subscribing to one rendition can receive at most
// `min(link capacity, that rendition's bitrate)`, so measured arrival alone never reveals
// spare capacity and an upshift requires the policy's bounded probe. Counters, frame counts
// and loss are the ones the browser and the pinned library actually expose.
import { describe, expect, it, vi } from 'vitest';
import { Video } from '@moq/watch';
import { Signal } from '@moq/signals';
import type { Connection } from '@moq/net';
import type * as Catalog from '@moq/hang/catalog';
import {
  AbrSampler,
  AbrController,
  authorizeRenditions,
  buildLadder,
  rateBps,
  abrSnapshot,
  type AbrAuthorizedVideo,
  type AbrPersistedState,
  type AbrPolicy,
  type AbrState,
} from './moqAbr';

interface RenditionSpec {
  readonly codec?: string;
  readonly width?: number | undefined;
  readonly height?: number | undefined;
  readonly bitrate?: number | undefined;
  readonly framerate?: number | undefined;
  readonly stalled?: boolean | undefined;
  readonly description?: string | undefined;
}

function rendition(spec: RenditionSpec): Catalog.VideoConfig {
  return {
    codec: spec.codec ?? 'vp8',
    container: { kind: 'legacy' },
    ...(spec.width !== undefined ? { codedWidth: spec.width } : {}),
    ...(spec.height !== undefined ? { codedHeight: spec.height } : {}),
    ...(spec.bitrate !== undefined ? { bitrate: spec.bitrate } : {}),
    ...(spec.framerate !== undefined ? { framerate: spec.framerate } : {}),
    ...(spec.stalled !== undefined ? { stalled: spec.stalled } : {}),
    ...(spec.description !== undefined ? { description: spec.description } : {}),
  } as unknown as Catalog.VideoConfig;
}

/** The media owner's canonical ladder (t38): 360p/540p VP8 + one H.264 1080p tier. */
const CANONICAL: Record<string, Catalog.VideoConfig> = {
  'video-360p': rendition({ width: 640, height: 360, bitrate: 800_000, framerate: 30 }),
  'video-540p': rendition({ width: 960, height: 540, bitrate: 2_000_000, framerate: 30 }),
  'video-1080p': rendition({ codec: 'avc1.42c01e', width: 1920, height: 1080, bitrate: 6_000_000, framerate: 30 }),
};

/** The same three tiers in one codec: what a single-codec lease can actually reach. */
const SINGLE_CODEC: Record<string, Catalog.VideoConfig> = {
  'video-360p': at(CANONICAL, 'video-360p'),
  'video-540p': at(CANONICAL, 'video-540p'),
  'video-1080p': rendition({ width: 1920, height: 1080, bitrate: 6_000_000, framerate: 30 }),
};

const AUTHORIZED: AbrAuthorizedVideo = { codec: 'vp8', codedWidth: 1920, codedHeight: 1080 };

/** A present fixture entry; the ladder tests would be meaningless with a missing one. */
function at(renditions: Record<string, Catalog.VideoConfig>, name: string): Catalog.VideoConfig {
  const config = renditions[name];
  if (config === undefined) throw new Error(`missing fixture rendition ${name}`);
  return config;
}

function root(renditions: Record<string, Catalog.VideoConfig>, audio = false): Catalog.Root {
  const video = { renditions } as unknown as Catalog.Video;
  return (audio ? { video, audio } : { video }) as unknown as Catalog.Root;
}

/** Let the pinned selector's effects (support probe, selection) settle. */
async function flush(): Promise<void> {
  for (let i = 0; i < 8; i += 1) await Promise.resolve();
  await new Promise((resolve) => { setTimeout(resolve, 0); });
  for (let i = 0; i < 8; i += 1) await Promise.resolve();
}

interface RigOptions {
  readonly authorized?: AbrAuthorizedVideo | undefined;
  readonly policy?: Partial<AbrPolicy> | undefined;
  /** Peer-reported PROBE value. moq-transport (Cloudflare draft16/18) reports none. */
  readonly probe?: Connection.Probe | undefined;
  readonly linkBps?: number | undefined;
  /** Shared sample clock, so a refresh can continue the same timeline. */
  readonly clock?: { at: number } | undefined;
  /** Adaptation carried from the session this rig replaces. */
  readonly resume?: AbrPersistedState | undefined;
  /** Model a transport that never serves the new subscription (old data keeps arriving). */
  readonly honourSwitches?: boolean | undefined;
}

/** Test policy: the same ratios, short enough to observe several switches in one test. */
const FAST: Partial<AbrPolicy> = {
  upshiftCooldownMs: 2000,
  upshiftHoldSamples: 2,
  settleGraceSamples: 1,
  probeVerifySamples: 1,
};

interface StepInput {
  /** Overrides the demand-limited model (used to test the measured-headroom path). */
  readonly injectedBps?: number | undefined;
  readonly elapsedMs?: number | undefined;
  /** Decoded-output frames per second observed on the render hook. */
  readonly presentedPerSecond?: number | undefined;
  readonly presentedFramesDelta?: number | undefined;
  readonly noDecodeHook?: boolean | undefined;
  readonly stalled?: boolean | undefined;
  readonly statsFailure?: boolean | undefined;
  readonly noVideoStats?: boolean | undefined;
  readonly lossGrowth?: boolean | undefined;
  readonly bufferedMs?: number | undefined;
}

/**
 * One independent viewer: its own real selector, its own target signal, its own counters.
 * A step accrues the bytes the demand-limited model allows and returns the resulting state.
 */
function rig(options: RigOptions = {}) {
  const catalog = new Signal<Catalog.Root | undefined>(undefined);
  const target = new Signal<Video.Target | undefined>(undefined);
  const probe = new Signal<Connection.Probe | undefined>(options.probe);
  const selector = new Video.Source({
    broadcast: new Signal({ out: { catalog } } as unknown as never),
    target,
    supported: () => Promise.resolve(true),
    probe,
  });
  const clock = options.clock ?? { at: 0 };
  let link = options.linkBps ?? 0;
  let transportBytes = 0;
  let videoBytes = 0;
  let lostBytes = 0;
  let stalled = false;
  let statsFailure = false;
  let noVideoStats = false;
  let noDecodeHook = false;
  let presentedFrames = 0;
  let bufferedMs: number | undefined = 500;
  let floor = 800_000;
  let sticky: { track: string | undefined; config: Catalog.VideoConfig | undefined } | undefined;
  /**
   * What the transport reports as the running rendition. With `honourSwitches: false` the
   * first selection sticks: exactly the observed failure where a SUBSCRIBE is never answered
   * and the old rendition's data keeps flowing.
   */
  const reported = (): { track: string | undefined; config: Catalog.VideoConfig | undefined } => {
    const track = selector.out.track.peek();
    const config = selector.out.config.peek();
    if (options.honourSwitches !== false) return { track, config };
    // Capture only after the ABR has pinned something: the selector's own pre-target default
    // is not "the rendition the transport is stuck on".
    if (sticky === undefined && track !== undefined && writes.length > 0) sticky = { track, config };
    return sticky ?? { track: undefined, config: undefined };
  };
  const writes: (Video.Target | undefined)[] = [];
  target.subscribe((value) => { writes.push(value); });
  const sampler = new AbrSampler({
    target,
    ...(options.authorized !== undefined ? { authorized: options.authorized } : {}),
    policy: { ...FAST, ...options.policy },
    now: () => clock.at,
    ...(options.resume !== undefined ? { resume: options.resume } : {}),
    source: {
      stats: () => {
        if (statsFailure) return Promise.reject(new Error('getStats unavailable'));
        return Promise.resolve({ bytesReceived: transportBytes, bytesLost: lostBytes });
      },
      probe: () => probe.peek(),
      catalog: () => catalog.peek()?.video,
      available: () => selector.out.available.peek(),
      unsupported: () => selector.out.error.peek() === 'unsupported',
      videoBytes: () => (noVideoStats ? undefined : videoBytes),
      presentedFrames: () => (noDecodeHook ? undefined : presentedFrames),
      presentedFrame: () => {
        const config = reported().config;
        return config?.codedWidth === undefined || config.codedHeight === undefined
          ? undefined
          : { width: config.codedWidth, height: config.codedHeight };
      },
      stalled: () => stalled,
      bufferedMs: () => bufferedMs,
      selection: reported,
    },
  });
  /** The rendition the viewer is currently subscribed to, so demand is its rate. */
  const demandedBps = (): number => reported().config?.bitrate ?? floor;
  async function step(input: StepInput = {}): Promise<AbrState> {
    const elapsed = input.elapsedMs ?? 1000;
    clock.at += elapsed;
    const seconds = elapsed / 1000;
    const demand = input.injectedBps ?? Math.min(link, demandedBps());
    videoBytes += Math.round((demand * seconds) / 8);
    transportBytes += Math.round((demand * seconds) / 8);
    if (input.presentedFramesDelta !== undefined) presentedFrames += input.presentedFramesDelta;
    else presentedFrames += Math.round((input.presentedPerSecond ?? 30) * seconds);
    if (input.lossGrowth === true) lostBytes += 4096;
    stalled = input.stalled ?? false;
    statsFailure = input.statsFailure ?? false;
    noVideoStats = input.noVideoStats ?? false;
    noDecodeHook = input.noDecodeHook ?? false;
    if (input.bufferedMs !== undefined) bufferedMs = input.bufferedMs;
    await sampler.sample();
    await flush();
    return sampler.controller.state;
  }
  return {
    sampler,
    selector,
    target,
    writes,
    step,
    state: (): AbrState => sampler.controller.state,
    effective: (): string | undefined => selector.out.track.peek(),
    reported: (): string | undefined => reported().track,
    setLink: (bps: number): void => { link = bps; },
    setCatalog: (renditions: Record<string, Catalog.VideoConfig>, audio = false): void => {
      floor = Math.min(...Object.values(renditions).map((r) => r.bitrate ?? Number.POSITIVE_INFINITY));
      catalog.set(root(renditions, audio));
    },
  };
}

describe('ABR ladder from the broadcast catalog', () => {
  it('offers only authorized, unstalled, bitrate-declared, decodable renditions', () => {
    const supported = {
      ...SINGLE_CODEC,
      'video-avc': rendition({ codec: 'avc1.42c01e', width: 640, height: 360, bitrate: 900_000 }),
      'video-unauthorized': rendition({ width: 3840, height: 2160, bitrate: 12_000_000 }),
      'video-stalled': rendition({ width: 960, height: 540, bitrate: 2_100_000, stalled: true }),
      'video-nobitrate': rendition({ width: 960, height: 540 }),
      'video-unknown-dims': rendition({ bitrate: 500_000 }),
    };
    const ladder = buildLadder(root({
      ...supported,
      'video-unsupported': rendition({ codec: 'vp09.00.10.08', width: 1920, height: 1080, bitrate: 7_000_000 }),
    }).video, { available: supported, authorized: AUTHORIZED });
    expect(ladder.renditions.map((r) => `${r.name}:${r.bitrate.toFixed(0)}`)).toEqual([
      'video-360p:800000', 'video-540p:2000000', 'video-1080p:6000000',
    ]);
    expect(ladder.excluded).toEqual([
      { name: 'video-avc', cause: 'not-authorized-codec' },
      { name: 'video-unauthorized', cause: 'not-authorized-dimensions' },
      { name: 'video-stalled', cause: 'stalled' },
      { name: 'video-nobitrate', cause: 'unknown-bitrate' },
      { name: 'video-unknown-dims', cause: 'not-authorized-dimensions' },
      { name: 'video-unsupported', cause: 'unsupported' },
    ]);
  });

  it('collapses a mixed-codec ladder to the single authorized codec, and reaches it when authorized', () => {
    // The lease today names one codec, so the canonical ladder's H.264 1080p tier is not
    // selectable for a VP8 lease: two tiers, and the policy says what it excluded instead of
    // synthesizing a quality.
    const collapsed = buildLadder(root(CANONICAL).video, { authorized: AUTHORIZED });
    expect(collapsed.renditions.map((r) => r.name)).toEqual(['video-360p', 'video-540p']);
    expect(collapsed.excluded).toEqual([{ name: 'video-1080p', cause: 'not-authorized-codec' }]);
    // Independently typed alternatives (the form a multi-codec ladder needs) reach all
    // three, each under its own dimension ceiling.
    const granted = buildLadder(root(CANONICAL).video, {
      authorized: {
        alternatives: [
          { codec: 'vp8', codedWidth: 960, codedHeight: 540 },
          { codec: 'avc1.42c01e', codedWidth: 1920, codedHeight: 1080 },
        ],
      },
    });
    expect(granted.renditions.map((r) => r.name)).toEqual(['video-360p', 'video-540p', 'video-1080p']);
    // An alternative whose own cap is smaller refuses the larger rendition of that codec.
    expect(buildLadder(root(CANONICAL).video, {
      authorized: { alternatives: [{ codec: 'avc1.42c01e', codedWidth: 640, codedHeight: 360 }] },
    }).excluded).toEqual([
      { name: 'video-360p', cause: 'not-authorized-codec' },
      { name: 'video-540p', cause: 'not-authorized-codec' },
      { name: 'video-1080p', cause: 'not-authorized-dimensions' },
    ]);
    // Init data the grant pinned is part of the grant: a rendition that does not carry it is
    // refused rather than handed to a decoder configured for other bytes.
    expect(buildLadder(root(CANONICAL).video, {
      authorized: {
        alternatives: [
          { codec: 'vp8', codedWidth: 960, codedHeight: 540 },
          { codec: 'avc1.42c01e', codedWidth: 1920, codedHeight: 1080, description: 'AAAA' },
        ],
      },
    }).excluded).toEqual([{ name: 'video-1080p', cause: 'not-authorized-dimensions' }]);
  });

  it('refuses a whole broadcast as unauthorized when the codec differs, and reports empty when nothing decodes', () => {
    expect(buildLadder(root({ only: rendition({ codec: 'avc1.42c01e', width: 320, height: 180, bitrate: 400_000 }) }).video, {
      authorized: AUTHORIZED,
    })).toEqual({ renditions: [], excluded: [{ name: 'only', cause: 'not-authorized-codec' }] });
    expect(buildLadder(root(SINGLE_CODEC).video, { available: {}, unsupported: true }))
      .toEqual({ renditions: [], excluded: Object.keys(SINGLE_CODEC).map((name) => ({ name, cause: 'unsupported' })) });
  });

  it('orders by ascending cost, keeping the larger rendition on a bitrate tie', () => {
    const ladder = buildLadder(root({
      small: rendition({ width: 160, height: 90, bitrate: 2_000_000 }),
      'video-540p': at(SINGLE_CODEC, 'video-540p'),
      'video-360p': at(SINGLE_CODEC, 'video-360p'),
    }).video);
    expect(ladder.renditions.map((r) => r.name)).toEqual(['video-360p', 'video-540p']);
  });

  it('does not apply the support filter until the probe has settled', () => {
    expect(buildLadder(root(SINGLE_CODEC).video, { available: {}, unsupported: false }).renditions).toHaveLength(3);
  });
});

describe('ABR measured delivery', () => {
  it('derives bits per second from cumulative counters and refuses noise', () => {
    expect(rateBps(0, 1000, 1000)).toBe(8000);
    expect(rateBps(1_000_000, 1_500_000, 500)).toBe(8_000_000);
    expect(rateBps(1000, 900, 1000)).toBeUndefined(); // counter reset
    expect(rateBps(0, 1000, 0)).toBeUndefined(); // no interval
    expect(rateBps(0, Number.NaN, 1000)).toBeUndefined();
  });

  it('measures the video track, so a heavy independent audio share cannot buy quality', async () => {
    // Same 3 Mbps of transport traffic, 900 kbps of it video: the viewer that budgets on the
    // whole-connection counter upshifts on audio bytes, the one that measures the video track
    // does not. Audio is a single non-per-rendition track, so it must not select video.
    const videoMeasured = rig({ authorized: AUTHORIZED, linkBps: 900_000 });
    const transportMeasured = rig({ authorized: AUTHORIZED, linkBps: 900_000 });
    videoMeasured.setCatalog(SINGLE_CODEC, true);
    transportMeasured.setCatalog(SINGLE_CODEC, true);
    await flush();
    for (let i = 0; i < 14; i += 1) {
      await videoMeasured.step({});
      await transportMeasured.step({ noVideoStats: true });
    }
    expect(videoMeasured.state().measurement).toBe('video-track-bytes');
    expect(videoMeasured.state().ladder.map((r) => r.name)).toEqual(['video-360p', 'video-540p', 'video-1080p']);
    expect(videoMeasured.effective()).toBe('video-360p');
    expect(transportMeasured.state().measurement).toBe('transport-received-bytes');
  });

  it('falls back to the transport counter when the player publishes no video stats', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 12_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 14; i += 1) await viewer.step({ noVideoStats: true });
    expect(viewer.state().measurement).toBe('transport-received-bytes');
    expect(viewer.effective()).toBe('video-1080p');
  });
});

describe('ABR per-consumer selection with the pinned selector', () => {
  it('holds, probes once and settles differently for three independent viewers', async () => {
    const slow = rig({ authorized: AUTHORIZED, linkBps: 300_000 });
    const mid = rig({ authorized: AUTHORIZED, linkBps: 4_000_000, policy: { probeBackoffMs: 30000 } });
    const fast = rig({ authorized: AUTHORIZED, linkBps: 12_000_000 });
    for (const viewer of [slow, mid, fast]) viewer.setCatalog(SINGLE_CODEC);
    await flush();
    expect(slow.target.peek()).toBeUndefined();

    const midReasons = new Set<string>();
    for (let i = 0; i < 25; i += 1) {
      await slow.step({});
      midReasons.add((await mid.step({})).reason);
      await fast.step({});
    }
    // The pinned selector's own verdict, one independent Source per viewer.
    expect(slow.effective()).toBe('video-360p');
    expect(mid.effective()).toBe('video-540p');
    expect(fast.effective()).toBe('video-1080p');
    expect(slow.selector.out.config.peek()?.bitrate).toBe(800_000);
    expect(mid.selector.out.config.peek()?.bitrate).toBe(2_000_000);
    expect(fast.selector.out.config.peek()?.bitrate).toBe(6_000_000);
    // The 300 kbps viewer is starving the lowest tier and would go lower if it could.
    expect(slow.state().reason).toBe('downshift-bandwidth');
    expect(slow.state().arrivalBps).toBeLessThan(800_000);
    // The 4 Mbps viewer reached 540p, tried 1080p, measured that it could not hold it, and
    // backed off instead of thrashing.
    expect([...midReasons]).toEqual(expect.arrayContaining(['probe-upshift', 'probe-accepted', 'probe-failed']));
    expect(mid.state().probeFailures).toBe(1);
    expect(mid.state().probeBackoffUntilMs).toBeDefined();
    // The 12 Mbps viewer holds the top tier with nothing left to ask for.
    expect(fast.state().reason).toBe('steady');
    expect(fast.state().requested?.name).toBe('video-1080p');
  });

  it('downshifts for a decoder that cannot keep up, and never probes upward from there', async () => {
    const healthy = rig({ authorized: AUTHORIZED, linkBps: 4_000_000 });
    const overloaded = rig({ authorized: AUTHORIZED, linkBps: 12_000_000 });
    healthy.setCatalog(SINGLE_CODEC);
    overloaded.setCatalog(SINGLE_CODEC);
    await flush();
    const reasons = new Set<string>();
    for (let i = 0; i < 18; i += 1) {
      await healthy.step({});
      reasons.add((await overloaded.step({ presentedPerSecond: 3 })).reason);
    }
    expect(healthy.effective()).toBe('video-540p');
    expect(overloaded.effective()).toBe('video-360p');
    expect([...reasons]).toContain('downshift-decoder');
  });

  it('treats a stalled renderer as backpressure and recovers', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 12_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 12; i += 1) await viewer.step({});
    expect(viewer.effective()).toBe('video-1080p');
    await viewer.step({ stalled: true, presentedFramesDelta: 0 });
    const stalled = await viewer.step({ stalled: true, presentedFramesDelta: 0 });
    expect(stalled.reason).toBe('downshift-stalled');
    expect(viewer.effective()).toBe('video-540p');
    for (let i = 0; i < 20; i += 1) await viewer.step({});
    expect(viewer.effective()).toBe('video-1080p');
  });

  it('moves off a rendition the publisher marks stalled', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 12_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 12; i += 1) await viewer.step({});
    expect(viewer.effective()).toBe('video-1080p');
    viewer.setCatalog({
      ...SINGLE_CODEC,
      'video-1080p': rendition({ width: 1920, height: 1080, bitrate: 6_000_000, framerate: 30, stalled: true }),
    });
    await flush();
    const state = await viewer.step({});
    expect(state.reason).toBe('effective-unavailable');
    expect(viewer.effective()).toBe('video-540p');
    expect(state.excluded).toEqual([{ name: 'video-1080p', cause: 'stalled' }]);
  });
});

describe('ABR probe discipline and hysteresis', () => {
  it('does not downshift a saturated rung on delivery jitter around its own advertised bitrate', async () => {
    // The death-spiral case: at a link equal to the rung's own bitrate the smoothed arrival
    // hovers at the bar, so a jittery interval that dips below the old 0.9x bar must not cost
    // the viewer a rung. The interval rate decides, and the bar is a clear 0.8x.
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 2_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 10; i += 1) {
      const state = await viewer.step({});
      if (state.probing !== undefined) break;
    }
    for (let i = 0; i < 6; i += 1) await viewer.step({});
    const settled = viewer.effective();
    const switches = viewer.state().switches;
    // 0.875x of the 2 Mbps rung: below the old bar, above the current one.
    const jitter = await viewer.step({ injectedBps: 1_750_000 });
    expect(jitter.reason).not.toBe('downshift-bandwidth');
    expect(viewer.effective()).toBe(settled);
    expect(viewer.state().switches).toBe(switches);
  });

  it('judges decoder health on the interval it measured, not on the configured period', async () => {
    // 3 decoded frames in a 100 ms interval is 30 fps and healthy; 30 decoded frames in a 5 s
    // interval is 6 fps and is not. A policy that assumed its own 1 s period would call the
    // first one a deficit and the second one healthy.
    const fastSample = rig({ authorized: AUTHORIZED, linkBps: 4_000_000, policy: { intervalMs: 100 } });
    const slowSample = rig({ authorized: AUTHORIZED, linkBps: 4_000_000 });
    fastSample.setCatalog(SINGLE_CODEC);
    slowSample.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 30; i += 1) {
      await fastSample.step({ elapsedMs: 100, presentedFramesDelta: 3 });
      await slowSample.step({ elapsedMs: 5000, presentedFramesDelta: 30 });
    }
    // Both links are the same; only the measured decode rate differs from the catalog cadence.
    expect(fastSample.state().reason).not.toBe('downshift-decoder');
    expect(slowSample.state().reason).toBe('downshift-decoder');
  });

  it('allows a GOP join stall longer than 500 ms but fails at the bounded deadline', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 4_000_000,
      policy: { joinTimeoutMs: 5000 } });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 10 && viewer.state().probing === undefined; i += 1) await viewer.step({});
    expect(viewer.state().probing).toBeDefined();
    for (let i = 0; i < 4; i += 1) {
      const joining = await viewer.step({ elapsedMs: 1000, stalled: true, presentedFramesDelta: 0 });
      expect(joining.reason).toBe('hold-probe');
      expect(joining.probing).toBeDefined();
    }
    expect((await viewer.step({ stalled: true, presentedFramesDelta: 0 })).reason).toBe('join-timeout');
  });

  it('downshifts when encoded frames arrive but the decoder renders none', async () => {
    // The received-payload counter in video.out.stats cannot stand in for decode health: here
    // the transport delivers a full 4 Mbps and the viewer still renders nothing.
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 4_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 12; i += 1) await viewer.step({});
    expect(viewer.effective()).toBe('video-540p');
    const reasons: string[] = [];
    for (let i = 0; i < 6; i += 1) reasons.push((await viewer.step({ presentedFramesDelta: 0 })).reason);
    expect(reasons).toContain('downshift-decoder');
    expect(viewer.effective()).toBe('video-360p');
  });

  it('does not treat a received-frame count as decode health', async () => {
    // The mirror image: little payload per interval, but the decoder is keeping up with the
    // catalog cadence, so the viewer must not be pushed down.
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 4_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    const reasons: string[] = [];
    for (let i = 0; i < 12; i += 1) reasons.push((await viewer.step({ presentedFramesDelta: 30 })).reason);
    expect(reasons).not.toContain('downshift-decoder');
    expect(viewer.effective()).toBe('video-540p');
  });

  it('probes once on a link that cannot sustain the next tier, then stays quiet', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 1_200_000,
      policy: { probeBackoffMs: 30000, probeBackoffMaxMs: 300000 } });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    const reasons: string[] = [];
    for (let i = 0; i < 60; i += 1) reasons.push((await viewer.step({})).reason);
    const attempts: number[] = [];
    reasons.forEach((reason, index) => { if (reason === 'probe-upshift') attempts.push(index); });
    expect(reasons).toContain('probe-failed');
    // Bounded, exponentially quieted probing: two attempts in a minute, the second only
    // after the first 30 s quiet period expired - not a retry every sample.
    expect(attempts).toHaveLength(2);
    const [firstAttempt, secondAttempt] = attempts;
    expect(secondAttempt !== undefined && firstAttempt !== undefined ? secondAttempt - firstAttempt : 0)
      .toBeGreaterThanOrEqual(30);
    expect(reasons.slice(40).every((reason) => reason === 'probe-backoff')).toBe(true);
    expect(viewer.state().switches).toBe(5); // initial, up, back, up, back
    expect(viewer.state().probeFailures).toBe(2);
    expect(viewer.effective()).toBe('video-360p');
    // One write per switch: the setter is never called for an unchanged request.
    expect(viewer.writes.filter((w) => w !== undefined).length).toBe(viewer.state().switches);
  });

  it('honours the hold window and the switch cooldown on measured headroom, with no write until they expire', async () => {
    // Arrival injected above the current tier (a burst, or the tail of a higher rendition):
    // this is the measured-headroom path, and it must still wait for the hold window.
    const upshiftHoldSamples = 3;
    const viewer = rig({ authorized: AUTHORIZED, policy: { upshiftHoldSamples, upshiftCooldownMs: 5000 } });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    const seen: string[] = [];
    const wroteAt: number[] = [];
    let writes = 0;
    for (let i = 0; i < 12; i += 1) {
      seen.push((await viewer.step({ injectedBps: 12_000_000 })).reason);
      if (viewer.writes.length > writes) {
        writes = viewer.writes.length;
        wroteAt.push(i);
      }
    }
    expect(seen[0]).toBe('initial');
    expect(seen).toContain('hold-settle');
    const upIndex = seen.indexOf('upshift-headroom');
    expect(upIndex).toBeGreaterThan(0);
    // Exactly two writes: the initial pin and the upshift. Nothing was written while the
    // cooldown and the hold window were still running, even though the headroom was there.
    expect(wroteAt).toHaveLength(2);
    expect(seen[0]).toBe('initial');
    expect(seen[upIndex]).toBe('upshift-headroom');
    expect(seen.slice(0, upIndex)).toContain('hold-cooldown');
    // The Nth eligible sample is the one that switches, so N-1 samples hold first.
    expect(seen.slice(0, upIndex).filter((reason) => reason === 'hold-headroom').length).toBe(upshiftHoldSamples - 1);
    expect(viewer.writes.length).toBe(2);
    expect(viewer.effective()).toBe('video-540p');
    // The cooldown for the switch just made then blocks the next tier.
    expect(seen.slice(upIndex + 1)).toContain('hold-cooldown');
  });

  it('recovers to the top tier from the floor on measured capability alone', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 12_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 12; i += 1) await viewer.step({});
    expect(viewer.effective()).toBe('video-1080p');
    viewer.setLink(300_000);
    const dip = new Set<string>();
    for (let i = 0; i < 6; i += 1) dip.add((await viewer.step({})).reason);
    expect(viewer.effective()).toBe('video-360p'); // fast downshift, two tiers
    expect([...dip]).toContain('downshift-bandwidth');
    expect(viewer.state().ladder.map((r) => r.name)).toEqual(['video-360p', 'video-540p', 'video-1080p']);
    viewer.setLink(12_000_000);
    const recovery = new Set<string>();
    for (let i = 0; i < 25; i += 1) recovery.add((await viewer.step({})).reason);
    expect(viewer.effective()).toBe('video-1080p');
    expect([...recovery].some((reason) => reason === 'probe-upshift' || reason === 'upshift-headroom')).toBe(true);
    expect(viewer.state().requested?.name).toBe('video-1080p');
  });

  it('reports an unacknowledged switch, quiets the upshift path and stops asking', async () => {
    // The transport never answers the new subscription (the observed Cloudflare failure):
    // the old rendition keeps delivering. The policy must reach an explicit compatibility
    // outcome with a bounded number of requests, not hold a timeout forever or re-ask.
    const viewer = rig({
      authorized: AUTHORIZED, linkBps: 4_000_000, honourSwitches: false,
      policy: { settleTimeoutMs: 2000, joinTimeoutMs: 2000, probeBackoffMs: 3000 },
    });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    const reasons: string[] = [];
    for (let i = 0; i < 25; i += 1) reasons.push((await viewer.step({})).reason);
    // The old rendition keeps rendering, so the requested one never joins: reported, bounded.
    expect(reasons).toContain('join-timeout');
    expect(reasons).toContain('switch-unacknowledged');
    expect(viewer.state().compat).toBe('switch-unacknowledged');
    expect(viewer.state().unacknowledgedSwitches).toBeGreaterThanOrEqual(2);
    expect(reasons.slice(-8).every((reason) => reason === 'probe-backoff')).toBe(true);
    // One bounded attempt, then the compatibility verdict quiets the probe path.
    expect(viewer.state().switches).toBeLessThanOrEqual(5);
    expect(viewer.reported()).toBe('video-360p');
    // After the verdict the upshift path is quiet: the tail is backoff, not retries.
    expect(reasons.slice(-10).every((reason) => reason === 'probe-backoff')).toBe(true);
  });

  it('keeps the target, the failed probe and its quiet period across a session refresh', async () => {
    const clock = { at: 0 };
    const first = rig({ authorized: AUTHORIZED, linkBps: 1_200_000, clock, policy: { probeBackoffMs: 30000 } });
    first.setCatalog(SINGLE_CODEC);
    await flush();
    const before: string[] = [];
    for (let i = 0; i < 12; i += 1) before.push((await first.step({})).reason);
    expect(before).toContain('probe-failed');
    const saved = abrSnapshot(first.state());
    expect(saved.requested?.name).toBe('video-360p');
    expect(saved.probeFailures).toBe(1);
    expect(saved.probeBackoffUntilMs).toBeDefined();

    // A refresh: new player, new counters, same viewer. The carried state must not restart
    // adaptation at the floor, forget the failed probe, or churn a request.
    const next = rig({ authorized: AUTHORIZED, linkBps: 1_200_000, clock, resume: saved, policy: { probeBackoffMs: 30000 } });
    next.setCatalog(SINGLE_CODEC);
    await flush();
    const after: string[] = [];
    for (let i = 0; i < 6; i += 1) after.push((await next.step({})).reason);
    expect(next.state().requested?.name).toBe('video-360p');
    expect(next.state().probeFailures).toBe(1);
    expect(next.state().probeBackoffUntilMs).toBe(saved.probeBackoffUntilMs);
    expect(next.state().switches).toBe(saved.switches);
    expect(after).toContain('probe-backoff');
    expect(next.reported()).toBe('video-360p');
  });

  it('adopts the effective rendition when a requested one disappears', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 12_000_000, policy: { settleTimeoutMs: 3000 } });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 12; i += 1) await viewer.step({});
    expect(viewer.effective()).toBe('video-1080p');
    const writesBefore = viewer.writes.length;
    // The broadcast stops advertising video-1080p: the pinned selector falls back inside the
    // still-explicit bitrate target, and the ABR must adopt that instead of re-requesting a
    // rendition that no longer exists.
    viewer.setCatalog({
      'video-360p': at(SINGLE_CODEC, 'video-360p'),
      'video-540p': at(SINGLE_CODEC, 'video-540p'),
    });
    await flush();
    const reasons = new Set<string>();
    for (let i = 0; i < 8; i += 1) reasons.add((await viewer.step({})).reason);
    expect(viewer.effective()).toBe('video-540p');
    expect([...reasons]).toContain('switch-timeout');
    expect(viewer.writes.length).toBeGreaterThan(writesBefore);
    expect(viewer.state().requested?.name).toBe('video-540p');
  });
});

describe('ABR without a usable probe (Cloudflare moq-transport)', () => {
  it('adapts on the measured counters while the peer estimate is exposed but ignored', async () => {
    // moq-transport has no PROBE stream: the pinned probe getter stays empty. When a peer
    // estimate does exist it describes OUR uplink, so it must never select a rendition.
    const slow = rig({ authorized: AUTHORIZED, linkBps: 300_000, probe: { estimatedRecvRate: 40_000_000 } });
    const fast = rig({ authorized: AUTHORIZED, linkBps: 12_000_000, probe: {} });
    slow.setCatalog(SINGLE_CODEC);
    fast.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 16; i += 1) {
      await slow.step({});
      await fast.step({});
    }
    expect(slow.effective()).toBe('video-360p');
    expect(fast.effective()).toBe('video-1080p');
    expect(slow.state().peerEstimatedRecvRateBps).toBe(40_000_000);
    expect(slow.state().measurement).toBe('video-track-bytes');
  });

  it('leaves the library auto-selection (which reads the peer estimate) unable to pick a rendition', async () => {
    // Evidence for the pinning rule: with no target the pinned selector picks by the peer's
    // estimate of our own uplink — here the LARGEST rendition — while an explicit target
    // cannot be overridden by it. This is why AbrSampler always pins one.
    const catalog = new Signal<Catalog.Root | undefined>(root(SINGLE_CODEC));
    const probe = new Signal<{ estimatedRecvRate?: number } | undefined>({ estimatedRecvRate: 40_000_000 });
    const target = new Signal<Video.Target | undefined>(undefined);
    const selector = new Video.Source({
      broadcast: new Signal({ out: { catalog } } as unknown as never),
      target,
      supported: () => Promise.resolve(true),
      probe,
    });
    await flush();
    expect(selector.out.track.peek()).toBe('video-1080p');
    target.set({ name: 'video-360p', bitrate: 800_000 });
    await flush();
    expect(selector.out.track.peek()).toBe('video-360p');
    target.set(undefined);
    await flush();
    expect(selector.out.track.peek()).toBe('video-1080p');
  });

  it('fails a probe whose delivery is declared lossy while the rate still looks fine', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 12_000_000, policy: { probeVerifySamples: 3 } });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    const reasons: string[] = [];
    for (let i = 0; i < 12; i += 1) {
      const state = await viewer.step({ lossGrowth: true });
      reasons.push(state.reason);
      if (state.reason === 'probe-failed') break;
    }
    expect(reasons).toContain('probe-failed');
    expect(viewer.state().probeFailures).toBe(1);
    expect(viewer.state().probeBackoffUntilMs).toBeDefined();
    expect(viewer.state().lostBytes).toBeGreaterThan(0);
    expect(viewer.effective()).toBe('video-360p');
  });

  it('treats missing counters as unknown, never as zero or as healthy', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 4_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 12; i += 1) await viewer.step({});
    expect(viewer.effective()).toBe('video-540p');
    const measured = viewer.state().arrivalBps;
    const writesBefore = viewer.writes.length;
    // One unmeasured sample: the recent measurement is still inside the staleness window.
    const fresh = await viewer.step({ statsFailure: true, noVideoStats: true });
    expect(fresh.arrivalBps).toBe(measured);
    // Past the window it is unavailable, and the policy says so instead of reusing it.
    for (let i = 0; i < 4; i += 1) await viewer.step({ statsFailure: true, noVideoStats: true });
    const stale = viewer.state();
    expect(stale.arrivalBps).toBeUndefined();
    expect(stale.measurement).toBe('unavailable');
    expect(stale.reason).toBe('measurement-unavailable');
    expect(viewer.effective()).toBe('video-540p');
    expect(viewer.writes.length).toBe(writesBefore);
  });

  it('abandons an unverifiable probe rather than accepting it without evidence', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 4_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 12; i += 1) {
      const state = await viewer.step({});
      if (state.probing !== undefined) break;
    }
    expect(viewer.state().probing).toBeDefined();
    // The sample right after the switch still mixes both renditions; the next unmeasured one
    // reaches the verdict and cannot credit the probe as healthy.
    await viewer.step({ statsFailure: true, noVideoStats: true });
    const unknown = await viewer.step({ statsFailure: true, noVideoStats: true });
    expect(unknown.reason).toBe('measurement-unavailable');
    expect(unknown.probing).toBeDefined();
    // Past the staleness window the probe is unverifiable and is abandoned, never accepted.
    for (let i = 0; i < 4; i += 1) await viewer.step({ statsFailure: true, noVideoStats: true });
    expect(viewer.state().probing).toBeUndefined();
    expect(viewer.state().reason).toBe('measurement-unavailable');
    expect(viewer.effective()).toBe('video-540p');
  });

  it('resolves rather than rejects on a failing stats call, so playback cannot be failed here', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 4_000_000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    await viewer.step({});
    await expect(viewer.sampler.sample()).resolves.toBeDefined();
  });

  it('says so when the broadcast has no selectable ladder', async () => {
    const viewer = rig({ authorized: AUTHORIZED });
    viewer.setCatalog({});
    await flush();
    const state = await viewer.step({});
    expect(state.reason).toBe('ladder-unavailable');
    expect(state.requested).toBeUndefined();
    expect(viewer.target.peek()).toBeUndefined();
  });

  it('validates every admitted rendition exactly as the control plane grants it', () => {
    // The grant is per rung and typed: exact track, codec, coded size, init data (base64 on the
    // HTTP wire, hex in the catalog) and the admitted bitrate/cadence as facts.
    const description = '000000016764001fac';
    const bytes = Uint8Array.from(description.match(/../g)?.map((pair) => Number.parseInt(pair, 16)) ?? []);
    let binary = '';
    for (const byte of bytes) binary += String.fromCharCode(byte);
    const catalog = root({
      'video-360p': rendition({ codec: 'avc1.42c01e', width: 640, height: 360, bitrate: 800_000, description }),
      'video-540p': rendition({ codec: 'avc1.42c01e', width: 960, height: 540, bitrate: 2_000_000, description }),
      'video-1080p': rendition({ codec: 'vp8', width: 1920, height: 1080, bitrate: 6_000_000 }),
    });
    const granted = authorizeRenditions([
      { track: 'video-360p', codec: 'avc1.42c01e', codedWidth: 640, codedHeight: 360,
        cadenceNumerator: 30, cadenceDenominator: 1, bitrateBps: 800_000, description: btoa(binary) },
      { track: 'video-1080p', codec: 'vp8', codedWidth: 1920, codedHeight: 1080, bitrateBps: 6_000_000 },
    ]);
    // The base64 init data must be compared in the catalog's hex encoding.
    expect(buildLadder(catalog.video, { authorized: granted }).renditions.map((r) => r.name))
      .toEqual(['video-360p', 'video-1080p']);
    // A rung the grant does not name is refused as such, not silently dropped.
    expect(buildLadder(catalog.video, { authorized: granted }).excluded)
      .toEqual([{ name: 'video-540p', cause: 'not-authorized-track' }]);
    // Init data that differs from the grant is refused rather than handed to the decoder.
    const other = root({
      'video-360p': rendition({ codec: 'avc1.42c01e', width: 640, height: 360, bitrate: 800_000, description: btoa('other') }),
    });
    expect(buildLadder(other.video, { authorized: granted }).excluded)
      .toEqual([{ name: 'video-360p', cause: 'not-authorized-dimensions' }]);
    // A catalog that advertises no bitrate uses the admitted one; nothing is estimated.
    const noBitrate = root({ 'video-360p': rendition({ codec: 'avc1.42c01e', width: 640, height: 360, description }) });
    expect(buildLadder(noBitrate.video, { authorized: granted }).renditions.map((r) => `${r.name}:${r.bitrate.toString()}`))
      .toEqual(['video-360p:800000']);
  });

  it('pins the single authorized rendition without claiming adaptivity', async () => {
    const only = { 'video-540p': at(SINGLE_CODEC, 'video-540p') };
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 12_000_000 });
    viewer.setCatalog(only);
    await flush();
    const state = await viewer.step({});
    expect(state.reason).toBe('single-rendition');
    expect(state.requested?.name).toBe('video-540p');
    expect(viewer.effective()).toBe('video-540p');
  });
});

describe('ABR live congestion recovery', () => {
  it.each([0, 1, 2])('keeps healthy gapped VBR unbiased for omission phase %s', (phase) => {
    const controller = new AbrController({ settleGraceSamples: 0, upshiftHoldSamples: 100 });
    const ladder = buildLadder(root(SINGLE_CODEC).video);
    const mid = { name: 'video-540p', bitrate: 2000000, framerate: 30 };
    const picture = { width: 960, height: 540 };
    controller.observe({ at: 0, ladder, effective: mid, presentedFrames: 0, presentedFrame: picture, videoBytes: 0, stalled: false });
    controller.restore({ ...abrSnapshot(controller.state), requested: mid });
    let bytes = 0;
    for (let second = 1; second <= 24; second += 1) {
      bytes += [2800000, 1500000, 1800000][(second - 1) % 3]! / 8;
      controller.observe({ at: second * 1000, ladder, effective: mid, presentedFrames: second * 30,
        presentedFrame: picture, videoBytes: second % 3 === phase ? undefined : bytes, stalled: false });
      expect(controller.state.requested?.name).toBe('video-540p');
    }
  });

  it('retains measured delivery debt through intermittent unavailable counters', () => {
    const controller = new AbrController({ settleGraceSamples: 0, upshiftHoldSamples: 100 });
    const ladder = buildLadder(root(SINGLE_CODEC).video);
    const mid = { name: 'video-540p', bitrate: 2000000, framerate: 30 };
    const picture = { width: 960, height: 540 };
    controller.observe({ at: 0, ladder, effective: mid, presentedFrames: 0, presentedFrame: picture, videoBytes: 0, stalled: false });
    controller.restore({ ...abrSnapshot(controller.state), requested: mid });
    for (let second = 1; second <= 12; second += 1) {
      controller.observe({ at: second * 1000, ladder, effective: mid, presentedFrames: second * 30,
        presentedFrame: picture, videoBytes: second % 3 === 0 ? undefined : second * 62500, stalled: false });
      if (controller.state.requested?.name === 'video-360p') break;
    }
    expect(controller.state.requested?.name).toBe('video-360p');
    expect(controller.state.reason).toBe('downshift-bandwidth');
  });
  it('holds steady healthy VBR output through a small GOP while sustained collapse still escapes', () => {
    const controller = new AbrController({ settleGraceSamples: 0, upshiftHoldSamples: 100 });
    const ladder = buildLadder(root(SINGLE_CODEC).video);
    const mid = { name: 'video-540p', bitrate: 2000000, framerate: 30 };
    const picture = { width: 960, height: 540 };
    controller.observe({ at: 0, ladder, effective: mid, presentedFrames: 0, presentedFrame: picture, videoBytes: 0, stalled: false });
    controller.restore({ ...abrSnapshot(controller.state), requested: mid });
    let bytes = 0;
    for (let second = 1; second <= 8; second += 1) {
      // 2.8/1.5/1.8 Mbps groups average above the rendition's sustainable delivery bar.
      bytes += [2800000, 1500000, 1800000][(second - 1) % 3]! / 8;
      controller.observe({ at: second * 1000, ladder, effective: mid, presentedFrames: second * 30,
        presentedFrame: picture, videoBytes: bytes, stalled: false });
      expect(controller.state.requested?.name).toBe('video-540p');
    }
    bytes += 250000 / 8;
    controller.observe({ at: 9000, ladder, effective: mid, presentedFrames: 240,
      presentedFrame: picture, videoBytes: bytes, stalled: true });
    expect(controller.state.requested?.name).toBe('video-360p');
  });
  it('escalates an obsolete pending downshift to the floor on a fresh capacity collapse', () => {
    const controller = new AbrController({ settleGraceSamples: 0 });
    const ladder = buildLadder(root(SINGLE_CODEC).video);
    const high = { name: 'video-1080p', bitrate: 6000000, framerate: 30 };
    const picture = { width: 1920, height: 1080 };
    controller.observe({ at: 0, ladder, effective: high, presentedFrames: 1, presentedFrame: picture, videoBytes: 0, stalled: false });
    // Restore a previously healthy high request without inventing a new media session counter.
    controller.restore({ ...abrSnapshot(controller.state), requested: high });
    controller.observe({ at: 1000, ladder, effective: high, presentedFrames: 2, presentedFrame: picture, videoBytes: 0, stalled: false });
    controller.observe({ at: 2000, ladder, effective: high, presentedFrames: 2, presentedFrame: picture, videoBytes: 350000, stalled: true });
    expect(controller.state.requested?.name).toBe('video-540p');
    controller.observe({ at: 3000, ladder, effective: high, presentedFrames: 2, presentedFrame: picture, videoBytes: 375000, stalled: true });
    expect(controller.state.requested?.name).toBe('video-360p');
    expect(controller.state.reason).toBe('downshift-bandwidth');
  });

  it('retains healthy VBR presentation across a short low-bitrate probe interval', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 4000000 });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    for (let i = 0; i < 10 && viewer.state().probing === undefined; i += 1) await viewer.step({});
    expect(viewer.state().probing).toBe('video-540p');
    await viewer.step({ injectedBps: 3500000, presentedFramesDelta: 30 });
    const state = await viewer.step({ injectedBps: 400000, presentedFramesDelta: 30 });
    expect(state.reason).not.toBe('probe-failed');
    expect(viewer.state().requested?.name).toBe('video-540p');
  });

  it('bounds capacity probe rate and rechecks a recovered link within 25 seconds', async () => {
    const viewer = rig({ authorized: AUTHORIZED, linkBps: 1200000, policy: { upshiftHoldSamples: 3 } });
    viewer.setCatalog(SINGLE_CODEC);
    await flush();
    const attempts: number[] = [];
    for (let i = 0; i < 60; i += 1) {
      if ((await viewer.step({})).reason === 'probe-upshift') attempts.push(i);
    }
    expect(attempts.length).toBeLessThanOrEqual(5);
    expect(viewer.effective()).toBe('video-360p');
    for (let i = 1; i < attempts.length; i += 1) expect((attempts[i] ?? 0) - (attempts[i - 1] ?? 0)).toBeGreaterThanOrEqual(8);
    viewer.setLink(4000000);
    for (let i = 0; i < 25 && viewer.effective() !== 'video-540p'; i += 1) await viewer.step({});
    expect(viewer.effective()).toBe('video-540p');
  });
});

describe('ABR presented rendition evidence', () => {
  it('does not report a selected label as the effective rung, or accept ambiguous dimensions', () => {
    const controller = new AbrController();
    const ladder = buildLadder(root(SINGLE_CODEC).video);
    controller.observe({ at: 0, ladder, effective: { name: 'video-1080p', bitrate: 6000000, framerate: 30 },
      presentedFrames: 1, presentedFrame: { width: 640, height: 360 }, stalled: false });
    expect(controller.state.effective.name).toBe('video-360p');
    controller.observe({ at: 1000, ladder, effective: { name: 'video-1080p', bitrate: 6000000, framerate: 30 },
      presentedFrames: 1, stalled: false });
    expect(controller.state.effective.name).toBeUndefined();
    const ambiguous = buildLadder(root({
      low: rendition({ width: 640, height: 360, bitrate: 800000 }),
      high: rendition({ width: 640, height: 360, bitrate: 2000000 }),
    }).video);
    controller.observe({ at: 2000, ladder: ambiguous, effective: { name: 'high', bitrate: 2000000, framerate: 30 },
      presentedFrames: 2, presentedFrame: { width: 640, height: 360 }, stalled: false });
    expect(controller.state.effective.name).toBeUndefined();
  });
});

describe('ABR sampler lifecycle', () => {
  it('samples once per interval, is idempotent and stops on close', async () => {
    vi.useFakeTimers();
    try {
      const viewer = rig({ authorized: AUTHORIZED, linkBps: 4_000_000 });
      viewer.setCatalog(SINGLE_CODEC);
      const sampled = vi.spyOn(viewer.sampler, 'sample');
      const stop = viewer.sampler.start();
      await vi.advanceTimersByTimeAsync(0);
      expect(sampled.mock.calls.length).toBe(1); // the immediate first sample
      viewer.sampler.start(); // idempotent: no second timer, no second immediate sample
      expect(sampled.mock.calls.length).toBe(1);
      await vi.advanceTimersByTimeAsync(3000);
      expect(sampled.mock.calls.length).toBe(4);
      stop();
      await vi.advanceTimersByTimeAsync(3000);
      expect(sampled.mock.calls.length).toBe(4);
    } finally {
      vi.useRealTimers();
    }
  });
});
