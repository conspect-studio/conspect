// Per-consumer MoQ adaptive rendition selection (ABR) for the live preview.
//
// Division of labour, from the pinned APIs (@moq/net 0.4.7, @moq/watch 0.6.6):
//   - This layer owns the POLICY: measure the viewer's own inbound delivery, pick a
//     rendition, and expose requested/effective/reason. It never subscribes to or reorders
//     media itself.
//   - `@moq/watch` `Video.Source` owns the MECHANISM: it filters the catalog by the
//     WebCodecs support probe, honours `Player.in.target` (name and/or bitrate), and
//     `Video.Decoder` promotes a replacement rendition only after `caughtUp()` — a new
//     track subscription starts at the latest group boundary, so a switch is a fresh
//     keyframe-aligned subscription rather than a mid-group splice.
//
// What is measured, and what is deliberately NOT trusted:
//   - `probe.estimatedRecvRate` is measured by the PEER and reports the bitrate the peer
//     receives FROM US: for a viewer that is our negligible uplink, not downstream
//     capacity. On moq-transport (Cloudflare draft16/18) there is no PROBE stream at all
//     and `probe` stays `{}`. It is exposed in the state for diagnosis and never selects.
//   - `stats().estimatedSendRate` is the local congestion controller's SEND estimate; a
//     consumer sends almost nothing, so it is not delivered capacity either.
//   - Arrival is measured from monotonic counters over a measured wall-clock interval:
//     `Video.Decoder.out.stats.bytesReceived` (video only, preferred) else
//     `Connection.stats().bytesReceived` (whole session). A missing or stale counter is
//     reported as unavailable and holds the rendition — never as zero, and never as an
//     invented rate.
//   - Arrival while a rendition is playing is bounded by that rendition's own demand, so it
//     proves capacity only when it exceeds the current tier. Ascending therefore uses a
//     BOUNDED ACTIVE PROBE: request the next tier, verify delivery (frame age via the
//     library's stalled flag, presented-frame throughput, measured arrival, declared loss) for
//     a bounded window, keep it when delivery holds and back off — immediately, then
//     exponentially — when it does not. Without the probe a viewer parked on the lowest tier
//     of a fast link could never discover the headroom.
//
// Audio is independent: the ladder is built from `catalog.video.renditions` only, the
// measured rate prefers the video track's own byte counter over the whole-connection
// counter, and no audio input is ever written (the publisher serves one audio track, so a
// video rendition switch does not re-subscribe audio). Congestion/ABR here is QUIC delivery
// and WebCodecs backpressure only — there is no WebRTC path and no transport fallback.
import type * as Catalog from '@moq/hang/catalog';
import type { Video } from '@moq/watch';

/** Where a measured arrival rate came from, or why there is none. */
export type AbrMeasurement = 'video-track-bytes' | 'transport-received-bytes' | 'unavailable';

/** Why a catalog rendition is not offered to the policy. */
export type AbrExclusionCause =
  | 'unsupported'
  | 'not-authorized-track'
  | 'not-authorized-codec'
  | 'not-authorized-dimensions'
  | 'stalled'
  | 'unknown-bitrate';

/** A compatibility outcome the viewer established about its transport. */
export type AbrCompat = 'switch-unacknowledged';

/** The decision the policy reached on one sample. */
export type AbrReason =
  | 'initial'
  | 'steady'
  | 'hold-settle'
  | 'hold-probe'
  | 'hold-headroom'
  | 'hold-cooldown'
  | 'hold-deficit'
  | 'hold-pending'
  | 'probe-backoff'
  | 'downshift-bandwidth'
  | 'downshift-decoder'
  | 'downshift-stalled'
  | 'downshift-unmeasured'
  | 'upshift-headroom'
  | 'probe-upshift'
  | 'probe-accepted'
  | 'probe-failed'
  | 'join-timeout'
  | 'switch-timeout'
  | 'switch-unacknowledged'
  | 'effective-unavailable'
  | 'ladder-unavailable'
  | 'single-rendition'
  | 'measurement-unavailable';

/** One selectable rendition: catalog metadata plus the advertised encoded bitrate. */
export interface AbrRendition {
  readonly name: string;
  /** Advertised average encoded bitrate in bits per second; always positive. */
  readonly bitrate: number;
  readonly width: number | undefined;
  readonly height: number | undefined;
  readonly framerate: number | undefined;
}

/** A rendition the policy will not offer, with the first cause that removed it. */
export interface AbrExclusion {
  readonly name: string;
  readonly cause: AbrExclusionCause;
}

/** The selectable ladder plus the exclusions that produced it. */
export interface AbrLadder {
  /** Ascending by bitrate. */
  readonly renditions: readonly AbrRendition[];
  readonly excluded: readonly AbrExclusion[];
}

/** One authorized decoder configuration: codec plus the shape the grant covers. */
export interface AbrAuthorizedVideoConfig {
  /** Exact rendition name this grant covers, when the grant names one. */
  readonly track?: string | undefined;
  readonly codec: string;
  readonly codedWidth?: number | undefined;
  readonly codedHeight?: number | undefined;
  /** Hex decoder initialization data the grant pinned for this codec, when required. */
  readonly description?: string | undefined;
  /** Admitted average bitrate, used only where the catalog advertises none. */
  readonly bitrate?: number | undefined;
  /** Admitted cadence in frames per second, from the published rational. */
  readonly cadence?: number | undefined;
}

/**
 * Per-viewer capability/authorization ceiling, from the MoQ lease metadata.
 *
 * `codec`/`codedWidth`/`codedHeight` are the single negotiated config the lease carries
 * today. A broadcast whose ladder mixes codecs (the media owner's canonical ladder is VP8 up
 * to 540p and H.264 at 1080p) collapses to that one codec's tiers under that form, which is
 * correct but not adaptive across codecs. `alternatives` is the form a multi-codec ladder
 * needs: independently typed configs, each granting one codec at its own dimension ceiling
 * and (when the codec needs initialization data) its own `description` — not a bare codec
 * string set, which cannot express that H.264 1080p needs different dimensions and init data
 * than VP8 540p.
 *
 * The ceiling is per granted config: a rendition is authorized when its codec matches one
 * alternative, its coded size is at or below that alternative's cap, and, where the
 * alternative pins `description`, the rendition's decoder initialization data matches it.
 * Dimensions absent from a cap impose nothing; a rendition whose dimensions are unknown
 * cannot be shown to be inside a wired cap and is refused rather than assumed small.
 */
export interface AbrAuthorizedVideo {
  readonly codec?: string | undefined;
  readonly codedWidth?: number | undefined;
  readonly codedHeight?: number | undefined;
  /** Exact typed alternatives; when present they replace the single-config form. */
  readonly alternatives?: readonly AbrAuthorizedVideoConfig[] | undefined;
}

/** Tunables. Every default is a deliberate policy choice, not a library knob. */
export interface AbrPolicy {
  /** Sampling period; one decision per sample, never faster. */
  readonly intervalMs: number;
  /** Downshift when measured arrival falls below the effective rendition by this ratio. */
  readonly downshiftRatio: number;
  /**
   * Consecutive samples a smoothed-only deficit must persist before it downshifts. A deficit
   * in the last interval acts at once; one visible only in the smoothed rate waits, so a
   * rendition whose delivery merely jitters around its own advertised bitrate is not mistaken
   * for a link that cannot hold it (the death-spiral case).
   */
  readonly downshiftSamples: number;
  /** Downshift target: the highest rendition within this fraction of measured arrival. */
  readonly downshiftMargin: number;
  /** Fraction of the measured arrival treated as usable for an upshift. */
  readonly upshiftBudgetRatio: number;
  /** Upshift headroom: the candidate must fit the usable budget divided by this. */
  readonly upshiftHeadroom: number;
  /** Consecutive eligible samples required before any upshift (measured or probed). */
  readonly upshiftHoldSamples: number;
  /** Minimum time between switch requests, both directions. */
  readonly upshiftCooldownMs: number;
  /** How long a requested rendition may stay unselected before the request is abandoned. */
  readonly settleTimeoutMs: number;
  /**
   * Unacknowledged requests before the peer is treated as unable to switch renditions. The
   * request is not retried on every sample: reaching the limit quiets the upshift path for
   * `probeBackoffMs` and reports `switch-unacknowledged`, so a transport that cannot serve a
   * new subscription shows up as an explicit compatibility outcome instead of churn.
   */
  readonly unacknowledgedLimit: number;
  /** Presented-frame deficit (against catalog framerate) that counts as decoder backpressure. */
  readonly decodeDeficitRatio: number;
  /** Samples to hold after a switch before the next policy evaluation. */
  readonly settleGraceSamples: number;
  /**
   * Bound on a switch or probe that has not produced a presented frame. The new subscription
   * must render within this long of the request; nothing here waits for a fixed stall window,
   * so a transport that accepts a request and renders nothing is reported, not waited on.
   * Where the publisher admits a keyframe/GOP cadence, size this above that cadence.
   */
  readonly joinTimeoutMs: number;
  /** Consecutive stalled samples that count as severe congestion rather than a join. */
  readonly stallDownshiftSamples: number;
  /** A measured arrival older than this is treated as unavailable rather than reused. */
  readonly measurementStaleMs: number;
  /** Healthy post-switch samples required before a probed rendition is accepted. */
  readonly probeVerifySamples: number;
  /** Probe failure: measured arrival below the probed rendition by this ratio. */
  readonly probeFailureRatio: number;
  /** How saturating the current rendition must be before measuring cannot reveal headroom. */
  readonly probeSaturationRatio: number;
  /** Quiet period after a failed probe, doubling per consecutive failure. */
  readonly probeBackoffMs: number;
  /** Cap on the doubling probe backoff. */
  readonly probeBackoffMaxMs: number;
  /** Samples whose declared loss grew before a probe is judged failed. */
  readonly lossProbeFailSamples: number;
}

/**
 * Fast downshift, cautious upshift, one switch per decision.
 *
 * The asymmetric pair is what makes this hysteretic: a downshift needs delivery below
 * `1/downshiftRatio` of the effective bitrate (≈1.25× of it), while a *measured* upshift
 * needs arrival above `upshiftHeadroom / upshiftBudgetRatio` of the candidate (≈1.59×).
 * Between the two the measured path does nothing; the probe path is what moves a viewer
 * whose own delivery is demand-limited, and it is bounded by `probeVerifySamples`, the
 * switch cooldown and the exponential `probeBackoffMs`.
 */
export const DEFAULT_ABR_POLICY: AbrPolicy = {
  intervalMs: 1000,
  downshiftRatio: 0.8,
  downshiftSamples: 2,
  downshiftMargin: 0.8,
  upshiftBudgetRatio: 0.85,
  upshiftHeadroom: 1.35,
  upshiftHoldSamples: 3,
  upshiftCooldownMs: 5000,
  settleTimeoutMs: 10000,
  unacknowledgedLimit: 2,
  decodeDeficitRatio: 0.8,
  settleGraceSamples: 2,
  joinTimeoutMs: 5000,
  stallDownshiftSamples: 2,
  measurementStaleMs: 3000,
  probeVerifySamples: 2,
  probeFailureRatio: 0.8,
  probeSaturationRatio: 0.9,
  // Capacity may return quickly on a live link. Recheck within 16 s plus join/verification,
  // never every sample. Transport incompatibility has its own longer quiet budget below.
  probeBackoffMs: 8_000,
  probeBackoffMaxMs: 16_000,
  lossProbeFailSamples: 2,
};

/** What the policy is asked on every sample. All counters are cumulative. */
export interface AbrObservation {
  /** Monotonic sample time in milliseconds. */
  readonly at: number;
  readonly ladder: AbrLadder;
  /** The rendition the player currently renders, read back from the pinned selector. */
  readonly effective: {
    readonly name: string | undefined;
    readonly bitrate: number | undefined;
    readonly framerate: number | undefined;
  };
  /** `Video.Decoder.out.stalled`: no rendered frame for the library's buffering window. */
  readonly stalled: boolean;
  /** Cumulative encoded video bytes from `Video.Decoder.out.stats`. */
  readonly videoBytes?: number | undefined;
  /** Cumulative `Connection.stats().bytesReceived` for the whole session. */
  readonly transportBytes?: number | undefined;
  /**
   * Cumulative advancing presented-output frames observed on the viewer's own render hook
   * (`Video.Decoder.out.frame`). A reset or discontinuity means no reading, never a healthy one.
   */
  readonly presentedFrames?: number | undefined;
  /**
   * Shape of the newest presented frame. A join is evidenced by a picture at the probed
   * rendition's size, so a transport that keeps rendering the old rendition cannot pass for
   * having switched, whatever the reported track label says.
   */
  readonly presentedFrame?: { readonly width: number; readonly height: number } | undefined;
  /** Declared-lost bytes from `Connection.stats().bytesLost` (a gauge, not a total). */
  readonly lostBytes?: number | undefined;
  /** Media buffered ahead, from `Video.Decoder.out.buffered`; diagnosis only. */
  readonly bufferedMs?: number | undefined;
  /** Peer-reported PROBE estimates. Read for diagnostics only, never for selection. */
  readonly probe?: { readonly rtt?: number | undefined; readonly estimatedRecvRate?: number | undefined } | undefined;
}

/** The request to apply, present only when it changed. */
export interface AbrDecision {
  readonly target?: { readonly name: string; readonly bitrate: number } | undefined;
  readonly reason: AbrReason;
}

/** Observable ABR state: requested vs effective quality and why. */
export interface AbrState {
  readonly reason: AbrReason;
  readonly measurement: AbrMeasurement;
  readonly arrivalBps: number | undefined;
  /** The last interval's own rate, unsmoothed: what the probe verdict is judged on. */
  readonly instantBps: number | undefined;
  readonly requested: { readonly name: string; readonly bitrate: number } | undefined;
  readonly effective: { readonly name: string | undefined; readonly bitrate: number | undefined };
  readonly ladder: readonly AbrRendition[];
  readonly excluded: readonly AbrExclusion[];
  /** The rendition currently being verified, if a probe is in flight. */
  readonly probing: string | undefined;
  readonly probeFailures: number;
  readonly probeBackoffUntilMs: number | undefined;
  /** Consecutive requests the peer never honoured. */
  readonly unacknowledgedSwitches: number;
  /** Set when the peer has been shown unable to switch renditions, with the cause. */
  readonly compat: AbrCompat | undefined;
  /** When the carried arrival was measured, so a stale carry is not treated as fresh. */
  readonly measuredAtMs: number | undefined;
  readonly queuedAheadMs: number | undefined;
  readonly lostBytes: number | undefined;
  readonly probeRttMs: number | undefined;
  readonly peerEstimatedRecvRateBps: number | undefined;
  /** Target changes issued. Bounded by the policy: this is the hot-loop evidence. */
  readonly switches: number;
  readonly samples: number;
  readonly lastSwitchAtMs: number | undefined;
}

/**
 * Bits per second between two cumulative counter readings.
 *
 * Returns undefined for a non-finite reading, a counter that moved backwards (reset or
 * a different counter), or a non-positive interval: an unusable measurement is reported
 * as unusable rather than clamped into a number.
 */
export function rateBps(previous: number, current: number, elapsedMs: number): number | undefined {
  if (!Number.isFinite(previous) || !Number.isFinite(current) || !Number.isFinite(elapsedMs)) return undefined;
  if (previous < 0 || current < previous || elapsedMs <= 0) return undefined;
  return ((current - previous) * 8 * 1000) / elapsedMs;
}

/**
 * Build the selectable ladder from the broadcast catalog.
 *
 * `available` (from `Video.Source.out.available`) is the pinned selector's own WebCodecs
 * support verdict. While it is empty and `unsupported` is false the probe has not settled,
 * so the support filter is not applied yet — the authorization, stall and bitrate filters
 * still are, and the pinned selector keeps applying its own support filter to whatever we
 * request. `unsupported` true with an empty `available` means nothing in this broadcast can
 * be decoded, which leaves the ladder empty on purpose.
 *
 * A rendition the publisher marks `stalled` is excluded: that is the same avoid-this
 * reading the pinned selector applies, and it is what moves a viewer off a rendition whose
 * publisher says it is not being produced.
 */
export function buildLadder(
  video: Catalog.Video | undefined,
  options: {
    available?: Record<string, Catalog.VideoConfig> | undefined;
    unsupported?: boolean | undefined;
    authorized?: AbrAuthorizedVideo | undefined;
  } = {},
): AbrLadder {
  const renditions: AbrRendition[] = [];
  const excluded: AbrExclusion[] = [];
  const available = options.available;
  const probed = available !== undefined && Object.keys(available).length > 0;
  const authorized = options.authorized;
  for (const [name, config] of Object.entries(video?.renditions ?? {})) {
    let cause: AbrExclusionCause | undefined;
    const verdict = authorize(config, name, authorized);
    if (options.unsupported === true && !probed) cause = 'unsupported';
    else if (probed && !(name in available)) cause = 'unsupported';
    else if (verdict !== 'ok') cause = verdict;
    else if (config.stalled === true) cause = 'stalled';
    else if (!Number.isFinite(config.bitrate) && grantedBitrate(name, config.codec, authorized) === undefined) cause = 'unknown-bitrate';
    else if ((config.bitrate ?? 0) <= 0 && grantedBitrate(name, config.codec, authorized) === undefined) cause = 'unknown-bitrate';
    else {
      // A catalog that advertises no bitrate for a rung uses the admitted one; nothing is
      // estimated here, and the source is reported in the ladder entry.
      const granted = grantedBitrate(name, config.codec, authorized);
      const advertised = Number.isFinite(config.bitrate) && (config.bitrate ?? 0) > 0 ? config.bitrate : undefined;
      renditions.push({
        name,
        bitrate: advertised ?? granted ?? 0,
        width: config.codedWidth,
        height: config.codedHeight,
        framerate: authorized?.alternatives?.find((grant) =>
          grant.track === name && grant.codec === config.codec)?.cadence ?? config.framerate,
      });
    }
    if (cause !== undefined) excluded.push({ name, cause });
  }
  renditions.sort((a, b) => a.bitrate - b.bitrate || area(b) - area(a) || (a.name < b.name ? -1 : 1));
  // Two renditions at the same advertised bitrate cannot be told apart by a bitrate
  // request; keep the larger one so the ladder stays deterministic and ordered.
  const deduped: AbrRendition[] = [];
  for (const rendition of renditions) {
    if (deduped[deduped.length - 1]?.bitrate === rendition.bitrate) continue;
    deduped.push(rendition);
  }
  return { renditions: deduped, excluded };
}

function area(rendition: AbrRendition): number {
  return (rendition.width ?? 0) * (rendition.height ?? 0);
}

/** The authorization verdict for one rendition, with the cause when it is refused. */
type Authorization = 'ok' | 'not-authorized-track' | 'not-authorized-codec' | 'not-authorized-dimensions';

/**
 * Whether a rendition is inside the per-viewer grant, and why not when it is not.
 *
 * A grant that names nothing authorizes any codec (the dimensions cap, if any, still
 * applies). With `alternatives` the codec must match one alternative, and that
 * alternative's own dimensions and pinned init data must cover the rendition.
 */
function authorize(config: Catalog.VideoConfig, name: string, authorized: AbrAuthorizedVideo | undefined): Authorization {
  const alternatives = authorized?.alternatives;
  if (alternatives !== undefined && alternatives.length > 0) {
    const named = alternatives.filter((alternative) => alternative.track === undefined || alternative.track === name);
    if (named.length === 0) return 'not-authorized-track';
    const sameCodec = named.filter((alternative) => alternative.codec === config.codec);
    if (sameCodec.length === 0) return 'not-authorized-codec';
    return sameCodec.some((alternative) => withinGrant(config, alternative)) ? 'ok' : 'not-authorized-dimensions';
  }
  if (authorized === undefined) return 'ok';
  if (authorized.codec !== undefined && config.codec !== authorized.codec) return 'not-authorized-codec';
  return withinGrant(config, authorized) ? 'ok' : 'not-authorized-dimensions';
}

/** The granted bitrate for a rendition, used only where the catalog advertises none. */
function grantedBitrate(name: string, codec: string, authorized: AbrAuthorizedVideo | undefined): number | undefined {
  const alternatives = authorized?.alternatives;
  if (alternatives === undefined) return undefined;
  const match = alternatives.find((alternative) =>
    (alternative.track === undefined || alternative.track === name) && alternative.codec === codec && alternative.bitrate !== undefined);
  return match?.bitrate;
}

/** A missing cap imposes nothing; unknown dimensions cannot be shown to fit a wired cap. */
function withinGrant(
  config: Catalog.VideoConfig,
  cap: { codedWidth?: number | undefined; codedHeight?: number | undefined; description?: string | undefined },
): boolean {
  if (cap.description !== undefined && cap.description !== config.description) return false;
  if (cap.codedWidth === undefined && cap.codedHeight === undefined) return true;
  if (config.codedWidth === undefined || config.codedHeight === undefined) return false;
  if (cap.codedWidth !== undefined && config.codedWidth > cap.codedWidth) return false;
  if (cap.codedHeight !== undefined && config.codedHeight > cap.codedHeight) return false;
  return true;
}

interface Counters {
  readonly at: number;
  readonly videoBytes: number | undefined;
  readonly transportBytes: number | undefined;
  readonly presentedFrames: number | undefined;
  readonly lostBytes: number | undefined;
}

interface ProbeState {
  readonly candidate: string;
  /** When the request was issued, on the sampler's monotonic clock. */
  readonly startedAt: number;
  /** Decoded output observed since the request: proof the new subscription renders. */
  joined: boolean;
  /** Samples of healthy delivery observed after the join completed. */
  healthy: number;
  lossSamples: number;
  deficitSamples: number;
  measuredBits: number;
  measuredMs: number;
}

/** Whether the viewer's own render path is producing pictures right now. */
type FrameEvidence = 'progress' | 'none';

/**
 * The per-consumer ABR policy. Pure state machine: `observe` in, `AbrDecision` out.
 *
 * Every interval this policy uses is the interval it measured between two samples on the
 * sampler's monotonic clock, never the configured sampling period: a late or coalesced timer
 * must not be read as decoder output the interval never had.
 *
 * One decision per sample. A target is returned only when it differs from the one already
 * pinned, so a settled viewer costs no writes; a rendition switch is never started while a
 * previous request is still unselected, and a rendition that never renders is bounded by the
 * join deadline rather than being waited on forever.
 */
export class AbrController {
  readonly policy: AbrPolicy;
  readonly #authorized: AbrAuthorizedVideo | undefined;
  #previous: Counters | undefined;
  #arrivalBps: number | undefined;
  #instantBps: number | undefined;
  #measuredAt: number | undefined;
  #measurement: AbrMeasurement = 'unavailable';
  #requested: { name: string; bitrate: number } | undefined;
  #requestedAt: number | undefined;
  #lastSwitchAt: number | undefined;
  #headroomSamples = 0;
  #lastVideoBytes: { value: number; at: number } | undefined;
  #lastTransportBytes: { value: number; at: number } | undefined;
  #rateElapsed: number | undefined;
  #deliveryWindow: { bits: number; ms: number }[] = [];
  #deliveryRung: string | undefined;
  #deficitSamples = 0;
  #decodeDeficitSamples = 0;
  #stallSamples = 0;
  #graceRemaining = 0;
  #switches = 0;
  #samples = 0;
  #probe: ProbeState | undefined;
  #probeFailures = 0;
  #probeBackoffUntil: number | undefined;
  #unacknowledged = 0;
  #compat: AbrCompat | undefined;
  #queuedAheadMs: number | undefined;
  #lostBytes: number | undefined;
  #state: AbrState;
  #ladder: AbrLadder = { renditions: [], excluded: [] };
  #probeEstimate: { rtt?: number | undefined; estimatedRecvRate?: number | undefined } | undefined;

  constructor(policy: Partial<AbrPolicy> = {}, authorized?: AbrAuthorizedVideo) {
    this.policy = { ...DEFAULT_ABR_POLICY, ...policy };
    this.#authorized = authorized;
    this.#state = this.#snapshot('ladder-unavailable', undefined, undefined);
  }

  /** The authorization ceiling this controller was built with, if any. */
  get authorized(): AbrAuthorizedVideo | undefined {
    return this.#authorized;
  }

  /** Current observable state; replaced on every sample. */
  get state(): AbrState {
    return this.#state;
  }

  /** The sampled ladder of the last observation. */
  get ladder(): AbrLadder {
    return this.#ladder;
  }

  /**
   * Carry a previous session's adaptation across a refresh.
   *
   * Targets, backoff and the compatibility verdict survive; counters do not (a new session
   * restarts them), and the carried arrival keeps its original measurement time so the
   * staleness rule still applies to it.
   */
  restore(persisted: AbrPersistedState): void {
    this.#requested = persisted.requested === undefined
      ? undefined
      : { name: persisted.requested.name, bitrate: persisted.requested.bitrate };
    this.#arrivalBps = persisted.arrivalBps;
    this.#instantBps = persisted.instantBps;
    this.#measuredAt = persisted.measuredAtMs;
    this.#measurement = persisted.measurement;
    this.#lastSwitchAt = persisted.lastSwitchAtMs;
    this.#probeFailures = persisted.probeFailures;
    this.#probeBackoffUntil = persisted.probeBackoffUntilMs;
    this.#unacknowledged = persisted.unacknowledgedSwitches;
    this.#compat = persisted.compat;
    this.#switches = persisted.switches;
    this.#samples = persisted.samples;
    this.#requestedAt = undefined;
    this.#previous = undefined;
    this.#lastVideoBytes = undefined;
    this.#lastTransportBytes = undefined;
    this.#rateElapsed = undefined;
    this.#probe = undefined;
    this.#headroomSamples = 0;
    this.#deficitSamples = 0;
    this.#decodeDeficitSamples = 0;
    this.#stallSamples = 0;
  }

  observe(observation: AbrObservation): AbrDecision {
    this.#samples += 1;
    const { at, ladder, stalled } = observation;
    this.#ladder = ladder;
    // The selector names a requested subscription, not the active decoder. Report only a
    // uniquely identifiable presented picture; do not turn a selected label into output proof.
    const picture = observation.presentedFrame;
    const matches = picture === undefined ? [] : ladder.renditions.filter((r) =>
      r.width === picture.width && r.height === picture.height);
    const rendered = matches.length === 1 ? matches[0] : undefined;
    const effective = { name: rendered?.name, bitrate: rendered?.bitrate, framerate: rendered?.framerate };
    this.#probeEstimate = observation.probe;
    this.#queuedAheadMs = observation.bufferedMs;
    this.#lostBytes = observation.lostBytes;
    const previous = this.#previous;
    this.#previous = {
      at,
      videoBytes: observation.videoBytes,
      transportBytes: observation.transportBytes,
      presentedFrames: observation.presentedFrames,
      lostBytes: observation.lostBytes,
    };
    // The measured interval, not the configured one. A clock that did not advance, or a
    // counter that reset, invalidates the interval rather than being read as a healthy one.
    const elapsed = previous !== undefined && at > previous.at ? at - previous.at : undefined;
    let sample: number | undefined;
    this.#rateElapsed = undefined;
    const measure = (value: number | undefined, last: { value: number; at: number } | undefined): number | undefined => {
      if (value === undefined || last === undefined) return undefined;
      const measured = rateBps(last.value, value, at - last.at);
      if (measured !== undefined) this.#rateElapsed = at - last.at;
      return measured;
    };
    sample = measure(observation.videoBytes, this.#lastVideoBytes);
    if (sample !== undefined) this.#measurement = 'video-track-bytes';
    else {
      sample = measure(observation.transportBytes, this.#lastTransportBytes);
      if (sample !== undefined) this.#measurement = 'transport-received-bytes';
    }
    if (observation.videoBytes !== undefined) this.#lastVideoBytes = { value: observation.videoBytes, at };
    if (observation.transportBytes !== undefined) this.#lastTransportBytes = { value: observation.transportBytes, at };
    if (sample !== undefined) {
      this.#instantBps = sample;
      // Exponential smoothing, halved per sample: one bad sample moves the estimate half
      // way, which is what makes the downshift responsive without making it jumpy.
      this.#arrivalBps = this.#arrivalBps === undefined ? sample : this.#arrivalBps + 0.5 * (sample - this.#arrivalBps);
      this.#measuredAt = at;
    } else {
      // This sample measured nothing. The interval rate is never reused (an unknown
      // measurement is not a healthy one), and the smoothed arrival is dropped once it is
      // older than the staleness bound rather than steering on an unknown-age value.
      this.#instantBps = undefined;
      if (this.#measuredAt !== undefined && at - this.#measuredAt > this.policy.measurementStaleMs) {
        this.#arrivalBps = undefined;
        this.#measuredAt = undefined;
        this.#measurement = 'unavailable';
      }
    }
    // Advancing presented frames on the same interval. A reset (a discontinuity, a new
    // pipeline) yields no reading at all: not zero, and not healthy.
    let decoded: number | undefined;
    if (elapsed !== undefined && observation.presentedFrames !== undefined && previous?.presentedFrames !== undefined &&
      observation.presentedFrames >= previous.presentedFrames) {
      decoded = observation.presentedFrames - previous.presentedFrames;
    }
    // Without a decoder counter the library's own render-age flag is the frame evidence: it
    // reports whether a frame was rendered recently. Neither source is guessed at.
    const frames: FrameEvidence = (decoded !== undefined ? decoded > 0 : !stalled) ? 'progress' : 'none';
    this.#stallSamples = frames === 'progress' ? 0 : (stalled ? this.#stallSamples + 1 : 0);
    const lostGrowth = observation.lostBytes !== undefined && previous?.lostBytes !== undefined
      ? observation.lostBytes > previous.lostBytes
      : undefined;
    const decision = this.#decide({ at, elapsed, ladder, effective, frames, decoded,
      presentedFrame: observation.presentedFrame, lostGrowth });
    this.#state = this.#snapshot(decision.reason, effective.name, effective.bitrate);
    return decision;
  }


  #decide(input: {
    at: number;
    elapsed: number | undefined;
    ladder: AbrLadder;
    effective: { name: string | undefined; bitrate: number | undefined; framerate: number | undefined };
    frames: FrameEvidence;
    decoded: number | undefined;
    presentedFrame: { readonly width: number; readonly height: number } | undefined;
    lostGrowth: boolean | undefined;
  }): AbrDecision {
    const { at, ladder, effective, frames } = input;
    const order = ladder.renditions;
    const lowest = order[0];
    if (lowest === undefined) return this.#hold('ladder-unavailable');

    const requested = this.#requested;
    if (requested === undefined) {
      return this.#switch(lowest, at, order.length === 1 ? 'single-rendition' : 'initial');
    }

    // 1. An in-flight request is judged on the viewer's own presented output, not on the
    // reported track label: a transport that accepts the request but renders nothing must
    // fail within a bounded deadline, and a stall while the new subscription joins is not
    // by itself a failure.
    const probe = this.#probe;
    const probed = probe === undefined ? undefined : order.find((r) => r.name === probe.candidate);
    if (probe !== undefined && probed !== undefined) {
      if (input.lostGrowth === true) probe.lossSamples += 1;
      if (frames === 'progress' && this.#rendersRequested(input.presentedFrame, probed)) probe.joined = true;
      if (probe.lossSamples >= this.policy.lossProbeFailSamples) return this.#probeFailed(at, order, probed);
      if (!probe.joined) {
        // Nothing of the requested rendition has rendered yet. This is bounded by the join
        // deadline, not by a fixed stall window, and a transport that keeps rendering the old
        // rendition is reported instead of being waited on.
        if (at - probe.startedAt >= this.policy.joinTimeoutMs) return this.#probeFailed(at, order, probed, 'join-timeout');
        return this.#hold('hold-probe');
      }
      if (this.#instantBps !== undefined && this.#rateElapsed !== undefined) {
        probe.measuredBits += this.#instantBps * this.#rateElapsed / 1000;
        probe.measuredMs += this.#rateElapsed;
      }
      // The grace interval alone cannot decide a probe, but its bytes contribute to the
      // multi-interval GOP delivery window once presentation has identified the new rung.
      if (this.#graceRemaining > 0) {
        this.#graceRemaining -= 1;
        return this.#hold('hold-probe');
      }
      if (this.#stallSamples >= this.policy.stallDownshiftSamples) return this.#probeFailed(at, order, probed);
      const instant = this.#instantBps;
      const seconds = input.elapsed === undefined ? undefined : input.elapsed / 1000;
      const presentationHealthy = seconds !== undefined && probed.framerate !== undefined && input.decoded !== undefined &&
        input.decoded >= probed.framerate * seconds * this.policy.decodeDeficitRatio;
      // Advertised bitrate is a GOP average, not a promise for every one-second VBR window.
      // A rate deficit needs presentation starvation as corroboration; healthy pictures must
      // not be rejected merely because the encoder emitted a smaller group.
      const aggregateBps = probe.measuredMs > 0 ? probe.measuredBits * 1000 / probe.measuredMs : undefined;
      const aggregateDeficit = aggregateBps !== undefined && aggregateBps < probed.bitrate * this.policy.probeFailureRatio;
      const starved = instant !== undefined && instant < probed.bitrate * this.policy.probeFailureRatio &&
        (!presentationHealthy || aggregateDeficit);
      probe.deficitSamples = starved ? probe.deficitSamples + 1 : 0;
      if (starved && (aggregateDeficit || probe.deficitSamples >= this.policy.downshiftSamples)) return this.#probeFailed(at, order, probed);
      if (starved) return this.#hold('hold-probe');
      if (instant === undefined) {
        // No measurement to verify the probe with: an unverifiable probe is abandoned rather
        // than credited, because unknown is not healthy.
        if (this.#arrivalBps === undefined) this.#probe = undefined;
        return this.#hold('measurement-unavailable');
      }
      probe.healthy += 1;
      if (probe.healthy < this.policy.probeVerifySamples) return this.#hold('hold-probe');
      this.#probe = undefined;
      this.#probeFailures = 0;
      this.#probeBackoffUntil = undefined;
      if (this.#unacknowledged > 0) {
        // A rendition that joined and held is the acknowledgement the count was waiting for.
        this.#unacknowledged = 0;
        this.#compat = undefined;
      }
      return this.#hold('probe-accepted');
    }

    // 2. The rendition on screen is no longer offerable (unauthorized, stalled or removed).
    const effectiveIndex = order.findIndex((r) => r.name === effective.name);
    const effectiveRendition = effectiveIndex >= 0 ? order[effectiveIndex] : undefined;
    if (effectiveRendition === undefined) {
      this.#probe = undefined;
      const fallback = this.#fit(this.#arrivalBps, order) ?? lowest;
      if (fallback.name === requested.name) return this.#hold('effective-unavailable');
      return this.#switch(fallback, at, 'effective-unavailable');
    }

    // 3. A request the transport never honoured: bounded, then reported as a compatibility
    // outcome instead of being waited on again and again.
    if (effectiveRendition.name !== requested.name) {
      // A pending downshift must not hide a further collapse behind its join timeout.
      // Escalate only downward, on a fresh measured deficit against that pending target.
      const pendingIndex = order.findIndex((r) => r.name === requested.name);
      const pending = order[pendingIndex];
      if (pending !== undefined && pending.bitrate < effectiveRendition.bitrate &&
        this.#instantBps !== undefined && this.#instantBps < pending.bitrate * this.policy.downshiftRatio) {
        return this.#downshiftToFit(order, this.#instantBps, pendingIndex, at, 'downshift-bandwidth');
      }
      const started = this.#requestedAt;
      if (started === undefined || at - started < this.policy.settleTimeoutMs) return this.#hold('hold-pending');
      this.#probe = undefined;
      this.#requested = { name: effectiveRendition.name, bitrate: effectiveRendition.bitrate };
      this.#requestedAt = undefined;
      this.#unacknowledged += 1;
      if (this.#unacknowledged < this.policy.unacknowledgedLimit) return this.#hold('switch-timeout');
      this.#compat = 'switch-unacknowledged';
      this.#probeBackoffUntil = at + Math.max(300_000, this.policy.probeBackoffMaxMs);
      return this.#hold('switch-unacknowledged');
    }
    if (order.length === 1) return this.#hold('single-rendition');

    const arrival = this.#arrivalBps;
    const seconds = input.elapsed === undefined ? undefined : input.elapsed / 1000;
    const framerate = effectiveRendition.framerate;

    // 4. The interval that carried a switch mixes two renditions: hold, and do not judge the
    // new one from it.
    if (this.#graceRemaining > 0 && frames === 'progress') {
      this.#graceRemaining -= 1;
      return this.#hold('hold-settle');
    }

    // 5. A sustained stall is severe congestion feedback, bounded by the same sample count.
    if (this.#stallSamples >= this.policy.stallDownshiftSamples) {
      if (this.#deficit(this.#instantBps, effectiveRendition) || this.#deficit(arrival, effectiveRendition)) {
        return this.#downshiftToFit(order, this.#representativeRate(effectiveRendition), effectiveIndex, at, 'downshift-bandwidth');
      }
      if (arrival === undefined) return this.#stepDown(order, effectiveIndex, at, 'downshift-unmeasured');
      return this.#stepDown(order, effectiveIndex, at, 'downshift-stalled');
    }

    // 6. Decoder backpressure, on presented output against the interval actually measured.
    const deficitNow = seconds !== undefined && framerate !== undefined && input.decoded !== undefined &&
      input.decoded < framerate * seconds * this.policy.decodeDeficitRatio;
    this.#decodeDeficitSamples = deficitNow ? this.#decodeDeficitSamples + 1 : 0;
    if (deficitNow && this.#decodeDeficitSamples >= this.policy.downshiftSamples &&
      (arrival === undefined || arrival >= effectiveRendition.bitrate * this.policy.downshiftRatio)) {
      return this.#stepDown(order, effectiveIndex, at, 'downshift-decoder');
    }

    // 7. No usable measurement: hold, and drop an unverifiable probe without blaming the tier.
    if (arrival === undefined) {
      this.#probe = undefined;
      return this.#hold('measurement-unavailable');
    }

    // 8. A declared bitrate is a GOP average, not an instantaneous floor. Preserve healthy
    // presentation through short VBR dips; still react immediately to starvation, or to a
    // sustained three-interval delivery debt even while buffered pictures continue.
    if (this.#deliveryRung !== effectiveRendition.name) {
      this.#deliveryRung = effectiveRendition.name;
      this.#deliveryWindow = [];
    }
    if (this.#instantBps !== undefined && this.#rateElapsed !== undefined) {
      this.#deliveryWindow.push({ bits: this.#instantBps * this.#rateElapsed / 1000, ms: this.#rateElapsed });
      if (this.#deliveryWindow.length > 3) this.#deliveryWindow.shift();
    }
    // An unavailable interval is unknown, not repayment of previously measured delivery debt.
    const delivered = this.#deliveryWindow.reduce((sum, interval) => sum + interval.bits, 0);
    const duration = this.#deliveryWindow.reduce((sum, interval) => sum + interval.ms, 0);
    const sustainedDebt = this.#deliveryWindow.length >= 3 && duration > 0 &&
      this.#deficit(delivered * 1000 / duration, effectiveRendition);
    const presentingHealthy = seconds !== undefined && framerate !== undefined && input.decoded !== undefined &&
      input.decoded >= framerate * seconds * this.policy.decodeDeficitRatio;
    if (this.#deficit(this.#instantBps, effectiveRendition) && (!presentingHealthy || sustainedDebt)) {
      this.#deficitSamples = 0;
      return this.#downshiftToFit(order, this.#representativeRate(effectiveRendition), effectiveIndex, at, 'downshift-bandwidth');
    }
    if (this.#deficit(arrival, effectiveRendition) && (!presentingHealthy || sustainedDebt)) {
      this.#deficitSamples += 1;
      if (this.#deficitSamples >= this.policy.downshiftSamples) {
        this.#deficitSamples = 0;
        return this.#downshiftToFit(order, arrival, effectiveIndex, at, 'downshift-bandwidth');
      }
      return this.#hold('hold-deficit');
    }
    this.#deficitSamples = 0;

    const candidate = order[effectiveIndex + 1];
    if (candidate === undefined) {
      this.#headroomSamples = 0;
      return this.#hold('steady');
    }
    if (frames !== 'progress') {
      // No evidence the viewer is decoding right now: never ascend on an unknown.
      this.#headroomSamples = 0;
      return this.#hold('hold-headroom');
    }
    const backoffUntil = this.#probeBackoffUntil;
    if (backoffUntil !== undefined && at < backoffUntil) {
      this.#headroomSamples = 0;
      return this.#hold('probe-backoff');
    }
    const lastSwitch = this.#lastSwitchAt;
    const cooling = lastSwitch !== undefined && at - lastSwitch < this.policy.upshiftCooldownMs;

    // 9a. Measured headroom: the arrival itself exceeds the candidate's bar.
    const usable = arrival * this.policy.upshiftBudgetRatio;
    if (usable >= candidate.bitrate * this.policy.upshiftHeadroom) {
      if (cooling) {
        this.#headroomSamples = 0;
        return this.#hold('hold-cooldown');
      }
      this.#headroomSamples += 1;
      if (this.#headroomSamples < this.policy.upshiftHoldSamples) return this.#hold('hold-headroom');
      return this.#startProbe(candidate, at, 'upshift-headroom');
    }

    // 9b. The current rendition saturates its own delivery, so measurement cannot reveal
    // headroom: the only way up is a bounded probe of the next tier.
    const saturated = arrival >= effectiveRendition.bitrate * this.policy.probeSaturationRatio;
    if (!saturated) {
      this.#headroomSamples = 0;
      return this.#hold('steady');
    }
    if (cooling) {
      this.#headroomSamples = 0;
      return this.#hold('hold-cooldown');
    }
    this.#headroomSamples += 1;
    if (this.#headroomSamples < this.policy.upshiftHoldSamples) return this.#hold('hold-headroom');
    return this.#startProbe(candidate, at, 'probe-upshift');
  }

  /**
   * Whether what is rendering is the requested rendition.
   *
   * A picture must exactly identify one rung. Missing dimensions and same-size alternatives
   * cannot prove the requested subscription has reached presentation.
   */
  #rendersRequested(frame: { readonly width: number; readonly height: number } | undefined, probed: AbrRendition): boolean {
    if (probed.width === undefined || probed.height === undefined || frame === undefined) return false;
    // A same-size alternative is ambiguous without a decoder track identity. Do not credit it.
    const matches = this.#ladder.renditions.filter((r) => r.width === frame.width && r.height === frame.height);
    return matches.length === 1 && matches[0]?.name === probed.name;
  }

  /** Begin a bounded upshift, verified on the viewer's own presented output. */
  #startProbe(candidate: AbrRendition, at: number, reason: AbrReason): AbrDecision {
    this.#probe = { candidate: candidate.name, startedAt: at, joined: false, healthy: 0, lossSamples: 0, deficitSamples: 0, measuredBits: 0, measuredMs: 0 };
    return this.#switch(candidate, at, reason);
  }

  /**
   * A probed rendition did not deliver: step back immediately — one tier, not a fit, so the
   * viewer returns to the last known-good rendition — and quiet the probe path for an
   * exponentially growing period so a transport that cannot switch is not re-probed every
   * few samples.
   */
  #probeFailed(at: number, order: readonly AbrRendition[], probed: AbrRendition, cause: AbrReason = 'probe-failed'): AbrDecision {
    this.#probe = undefined;
    this.#probeFailures += 1;
    if (cause === 'join-timeout') {
      // A request whose rendition never rendered is an unhonoured switch, however it was
      // diagnosed; repeated ones are the transport-level compatibility verdict.
      this.#unacknowledged += 1;
      if (this.#unacknowledged >= this.policy.unacknowledgedLimit) {
        this.#compat = 'switch-unacknowledged';
        // A transport-level verdict is the strongest conclusion this policy draws, so it
        // quiets the upshift path for the long bound: recovery is a session refresh, not
        // another attempt every few seconds.
        this.#probeBackoffUntil = at + Math.max(300_000, this.policy.probeBackoffMaxMs);
        const index = order.findIndex((r) => r.name === probed.name);
        const previous = index > 0 ? order[index - 1] : undefined;
        if (previous === undefined) return this.#hold('switch-unacknowledged');
        return this.#switch(previous, at, 'switch-unacknowledged');
      }
    }
    const backoff = Math.min(this.policy.probeBackoffMs * 2 ** (this.#probeFailures - 1), this.policy.probeBackoffMaxMs);
    this.#probeBackoffUntil = at + backoff;
    const index = order.findIndex((r) => r.name === probed.name);
    const lower = index > 0 ? order[index - 1] : undefined;
    if (lower === undefined) return this.#hold(cause);
    return this.#switch(lower, at, cause);
  }

  /** Whether a measured rate is below the rendition it is sustaining. */
  #deficit(rate: number | undefined, effective: AbrRendition): boolean {
    return rate !== undefined && rate < effective.bitrate * this.policy.downshiftRatio;
  }

  /** The rate to size a downshift on: the worst of the interval and the smoothed reading. */
  #representativeRate(effective: AbrRendition): number {
    const instant = this.#instantBps;
    const smoothed = this.#arrivalBps;
    if (instant === undefined) return smoothed ?? effective.bitrate * this.policy.downshiftRatio;
    if (smoothed === undefined) return instant;
    return Math.min(instant, smoothed);
  }

  /** The highest rendition inside `arrival × downshiftMargin`, never an upshift. */
  #downshiftToFit(order: readonly AbrRendition[], arrival: number, effectiveIndex: number, at: number, reason: AbrReason): AbrDecision {
    const effective = order[effectiveIndex];
    const fit = this.#fit(arrival, order);
    if (effective !== undefined && fit !== undefined && fit.bitrate < effective.bitrate) return this.#switch(fit, at, reason);
    // No admitted tier fits: request the floor directly rather than stair-step through
    // additional unsustainable subscriptions and their GOP joins.
    if (fit === undefined && effectiveIndex > 0 && order[0] !== undefined) return this.#switch(order[0], at, reason);
    return this.#stepDown(order, effectiveIndex, at, reason);
  }

  #stepDown(order: readonly AbrRendition[], effectiveIndex: number, at: number, reason: AbrReason): AbrDecision {
    const lower = effectiveIndex > 0 ? order[effectiveIndex - 1] : undefined;
    // At the floor there is nothing lower to request, but the trigger is still the honest
    // reason: a decoder deficit or a stall at the floor must keep upshifts suppressed.
    if (lower === undefined) return this.#hold(reason);
    return this.#switch(lower, at, reason);
  }

  #fit(arrival: number | undefined, order: readonly AbrRendition[]): AbrRendition | undefined {
    if (arrival === undefined) return undefined;
    const budget = arrival * this.policy.downshiftMargin;
    let fit: AbrRendition | undefined;
    for (const rendition of order) {
      if (rendition.bitrate <= budget) fit = rendition;
    }
    return fit;
  }

  #switch(rendition: AbrRendition, at: number, reason: AbrReason): AbrDecision {
    const changed = this.#requested?.name !== rendition.name;
    this.#requested = { name: rendition.name, bitrate: rendition.bitrate };
    this.#headroomSamples = 0;
    this.#deficitSamples = 0;
    this.#decodeDeficitSamples = 0;
    if (!changed) return this.#hold(reason);
    this.#requestedAt = at;
    this.#lastSwitchAt = at;
    this.#switches += 1;
    this.#graceRemaining = this.policy.settleGraceSamples;
    return { reason, target: { name: rendition.name, bitrate: rendition.bitrate } };
  }

  #hold(reason: AbrReason): AbrDecision {
    return { reason };
  }

  #snapshot(reason: AbrReason, effectiveName: string | undefined, effectiveBitrate: number | undefined): AbrState {
    return {
      reason,
      measurement: this.#measurement,
      arrivalBps: this.#arrivalBps,
      instantBps: this.#instantBps,
      requested: this.#requested,
      effective: { name: effectiveName, bitrate: effectiveBitrate },
      ladder: this.#ladder.renditions,
      excluded: this.#ladder.excluded,
      probing: this.#probe?.candidate,
      probeFailures: this.#probeFailures,
      probeBackoffUntilMs: this.#probeBackoffUntil,
      unacknowledgedSwitches: this.#unacknowledged,
      compat: this.#compat,
      measuredAtMs: this.#measuredAt,
      queuedAheadMs: this.#queuedAheadMs,
      lostBytes: this.#lostBytes,
      probeRttMs: this.#probeEstimate?.rtt,
      peerEstimatedRecvRateBps: this.#probeEstimate?.estimatedRecvRate,
      switches: this.#switches,
      samples: this.#samples,
      lastSwitchAtMs: this.#lastSwitchAt,
    };
  }
}

/**
 * The part of the policy state that must survive a session refresh.
 *
 * A new lease means a new player, new counters and a new subscription, but the viewer's
 * situation has not changed: the same link, the same failed probes, the same rung it was
 * told to hold. Restoring this keeps a refresh from restarting at the floor and re-probing a
 * rendition already shown not to hold. Counters deliberately do not carry (`measuredAtMs`
 * rides along so a stale carry is treated as stale, not as a fresh measurement).
 */
export interface AbrPersistedState {
  readonly requested: { readonly name: string; readonly bitrate: number } | undefined;
  readonly arrivalBps: number | undefined;
  readonly instantBps: number | undefined;
  readonly measuredAtMs: number | undefined;
  readonly measurement: AbrMeasurement;
  readonly lastSwitchAtMs: number | undefined;
  readonly probeFailures: number;
  readonly probeBackoffUntilMs: number | undefined;
  readonly unacknowledgedSwitches: number;
  readonly compat: AbrCompat | undefined;
  readonly switches: number;
  readonly samples: number;
}

/** Capture the refresh-surviving part of an observed state. */
export function abrSnapshot(state: AbrState): AbrPersistedState {
  return {
    requested: state.requested === undefined ? undefined : { name: state.requested.name, bitrate: state.requested.bitrate },
    arrivalBps: state.arrivalBps,
    instantBps: state.instantBps,
    measuredAtMs: state.measuredAtMs,
    measurement: state.measurement,
    lastSwitchAtMs: state.lastSwitchAtMs,
    probeFailures: state.probeFailures,
    probeBackoffUntilMs: state.probeBackoffUntilMs,
    unacknowledgedSwitches: state.unacknowledgedSwitches,
    compat: state.compat,
    switches: state.switches,
    samples: state.samples,
  };
}

/**
 * One admitted rendition as the control plane reports it (`MoqLease.video_alternatives`, owned by the
 * lease factory). Mirrored structurally so the browser validates what the publisher admitted
 * rather than what the catalog happened to advertise.
 */
export interface AbrRenditionGrant {
  /** The published MoQ track name and catalog rendition key — the same string targeted. */
  readonly track: string;
  readonly codec: string;
  readonly codedWidth?: number | null | undefined;
  readonly codedHeight?: number | null | undefined;
  /** Decoder initialization data in base64, as the HTTP lease carries it. */
  readonly description?: string | null | undefined;
  /** Admitted average bitrate, used only when the catalog does not advertise one. */
  readonly bitrateBps?: number | null | undefined;
  readonly cadenceNumerator?: number | null | undefined;
  readonly cadenceDenominator?: number | null | undefined;
}

/**
 * Turn admitted renditions into the per-viewer authorization grant.
 *
 * Every field is validated per rung before selection: the exact track when the grant names
 * one, the codec, the coded size, and the decoder initialization data — a rendition whose
 * description differs from the granted one is refused rather than handed to a decoder
 * configured for other bytes. `bitrate_bps` and `cadence` are admitted facts: a catalog
 * rendition with no advertised bitrate may use the granted one (reported as such), and where
 * neither exists the rung is excluded as `unknown-bitrate` — never estimated.
 */
export function authorizeRenditions(grants: readonly AbrRenditionGrant[]): AbrAuthorizedVideo {
  return {
    alternatives: grants.map((grant) => ({
      track: grant.track,
      codec: grant.codec,
      ...(typeof grant.codedWidth === 'number' ? { codedWidth: grant.codedWidth } : {}),
      ...(typeof grant.codedHeight === 'number' ? { codedHeight: grant.codedHeight } : {}),
      ...(typeof grant.description === 'string' ? { description: base64ToHex(grant.description) } : {}),
      ...(typeof grant.bitrateBps === 'number' ? { bitrate: grant.bitrateBps } : {}),
      ...(typeof grant.cadenceNumerator === 'number' && typeof grant.cadenceDenominator === 'number' && grant.cadenceDenominator !== 0
        ? { cadence: grant.cadenceNumerator / grant.cadenceDenominator }
        : {}),
    })),
  };
}

/** Normalize the HTTP lease's base64 initialization data to the Hang catalog's hex. */
function base64ToHex(base64: string): string {
  try {
    return Array.from(atob(base64), (byte) => byte.charCodeAt(0).toString(16).padStart(2, '0')).join('');
  } catch {
    // An unusable marker fails authorization instead of matching malformed initialization data.
    return '<invalid-base64>';
  }
}

/** What the sampler reads off the live session and player. */
export interface AbrSource {
  /** `Connection.Established.stats()`; resolves to undefined when unavailable. */
  stats(): Promise<{ bytesReceived?: number | undefined; bytesLost?: number | undefined } | undefined>;
  /** `Connection.Established.probe.peek()`. */
  probe(): { rtt?: number | undefined; estimatedRecvRate?: number | undefined } | undefined;
  /** `Player.broadcast.out.catalog.peek()?.video`. */
  catalog(): Catalog.Video | undefined;
  /** `Player.video.source.out.available.peek()`. */
  available(): Record<string, Catalog.VideoConfig> | undefined;
  /** True when `Player.video.source.out.error.peek()` is 'unsupported'. */
  unsupported(): boolean;
  /**
   * Cumulative encoded bytes for the active video track, from
   * `Video.Decoder.out.stats.bytesReceived`.
   */
  videoBytes(): number | undefined;
  /**
   * Cumulative advancing frames presented by this player, observed on the per-consumer
   * render hook (`Video.Decoder.out.frame`). `out.stats.frameCount` counts received encoded
   * payload frames, not presented output, so it cannot stand in for this.
   */
  presentedFrames(): number | undefined;
  /** Shape of the newest presented frame, from the per-consumer render hook. */
  presentedFrame(): { readonly width: number; readonly height: number } | undefined;
  /** `Player.video.out.stalled.peek()`. */
  stalled(): boolean;
  /** Media buffered ahead from `Player.video.out.buffered.peek()`; diagnosis only. */
  bufferedMs(): number | undefined;
  /** `Player.video.source.out.track` / `out.config`. */
  selection(): { track: string | undefined; config: Catalog.VideoConfig | undefined };
}

/**
 * Drives the policy from a live session: one timer, one stats snapshot per sample.
 *
 * The sampler owns no timer beyond `intervalMs`, never reschedules off a signal change,
 * and contains every failure: a rejected `stats()` becomes an explicitly unmeasured sample
 * that holds the current rendition. It never closes the session and never reports
 * `MoqUnavailable`, so this layer cannot trigger a transport fallback.
 */
export class AbrSampler {
  readonly controller: AbrController;
  #timer: ReturnType<typeof setInterval> | undefined;
  #inFlight = false;
  #closed = false;
  #lastWritten: string | undefined;
  readonly #source: AbrSource;
  readonly #target: { set(value: Video.Target | undefined): void };
  readonly #now: () => number;
  readonly #onState: ((state: AbrState) => void) | undefined;

  constructor(options: {
    source: AbrSource;
    target: { set(value: Video.Target | undefined): void };
    authorized?: AbrAuthorizedVideo | undefined;
    policy?: Partial<AbrPolicy> | undefined;
    now?: (() => number) | undefined;
    onState?: ((state: AbrState) => void) | undefined;
    /** Adaptation carried from the session this one replaces, if any. */
    resume?: AbrPersistedState | undefined;
  }) {
    this.#source = options.source;
    this.#target = options.target;
    this.#now = options.now ?? monotonicNow;
    this.#onState = options.onState;
    this.controller = new AbrController(options.policy ?? {}, options.authorized);
    if (options.resume !== undefined) this.controller.restore(options.resume);
  }

  /** The refresh-surviving state to hand to the session that replaces this one. */
  snapshot(): AbrPersistedState {
    return abrSnapshot(this.controller.state);
  }

  /** The policy's sampling period. */
  get intervalMs(): number {
    return this.controller.policy.intervalMs;
  }

  /**
   * Start sampling. Idempotent; the returned function stops it.
   *
   * The first sample is taken immediately rather than one interval later: until a target is
   * pinned the pinned selector has only its own default (the largest rendition, or the peer's
   * uplink estimate), and a viewer should not spend the first second of a session there.
   */
  start(): () => void {
    if (this.#timer !== undefined || this.#closed) return () => { /* already started or closed */ };
    this.#timer = setInterval(() => { void this.sample(); }, this.intervalMs);
    void this.sample();
    return () => { this.close(); };
  }

  /** One sample. Resolves with the state it produced; never rejects. */
  async sample(): Promise<AbrState> {
    if (this.#closed || this.#inFlight) return this.controller.state;
    this.#inFlight = true;
    try {
      const stats = await this.#stats();
      // A close during the awaited stats call still owns the last observed state.
      if (this.#isClosed()) return this.controller.state;
      const selection = this.#source.selection();
      this.controller.observe({
        at: this.#now(),
        ladder: buildLadder(this.#source.catalog(), {
          available: this.#source.available(),
          unsupported: this.#source.unsupported(),
          authorized: this.controller.authorized,
        }),
        effective: {
          name: selection.track,
          bitrate: selection.config?.bitrate,
          framerate: selection.config?.framerate,
        },
        stalled: this.#source.stalled(),
        videoBytes: this.#source.videoBytes(),
        transportBytes: stats?.bytesReceived,
        presentedFrames: this.#source.presentedFrames(),
        presentedFrame: this.#source.presentedFrame(),
        lostBytes: stats?.bytesLost,
        bufferedMs: this.#source.bufferedMs(),
        probe: this.#source.probe(),
      });
      this.#apply();
    } catch (error) {
      // A measurement failure holds the rendition; it must never reach the caller as a
      // transport failure, and it must never be converted into a fabricated rate.
      console.debug('MoQ ABR sample failed', error);
    } finally {
      this.#inFlight = false;
    }
    return this.controller.state;
  }

  #apply(): void {
    const state = this.controller.state;
    const requested = state.requested;
    const key = requested === undefined ? undefined : `${requested.name}@${requested.bitrate.toFixed(0)}`;
    if (key !== this.#lastWritten) {
      this.#lastWritten = key;
      this.#target.set(requested === undefined ? undefined : { name: requested.name, bitrate: requested.bitrate });
    }
    this.#onState?.(state);
  }

  async #stats(): Promise<{ bytesReceived?: number | undefined; bytesLost?: number | undefined } | undefined> {
    try {
      return await this.#source.stats();
    } catch (error) {
      console.debug('MoQ ABR transport stats unavailable', error);
      return undefined;
    }
  }

  /** Read through a call so a close that lands during an awaited sample is still honoured. */
  #isClosed(): boolean {
    return this.#closed;
  }

  close(): void {
    this.#closed = true;
    if (this.#timer !== undefined) {
      clearInterval(this.#timer);
      this.#timer = undefined;
    }
  }
}

/**
 * The sampler's clock. Monotonic on purpose: a wall-clock jump (NTP step, manual change, a
 * device waking) must not be read as a longer or shorter delivery interval, and every policy
 * deadline is a duration rather than a date.
 */
function monotonicNow(): number {
  return typeof performance === 'undefined' ? Date.now() : performance.now();
}

/** The DOM dataset expressions the preview exposes; one place for callers and tests. */
export function abrDatasetEntries(state: AbrState): Record<string, string> {
  return {
    'abr-reason': state.reason,
    'abr-measurement': state.measurement,
    'abr-requested': state.requested?.name ?? '',
    'abr-requested-bps': state.requested?.bitrate.toString() ?? '',
    'abr-effective': state.effective.name ?? '',
    'abr-effective-bps': state.effective.bitrate?.toString() ?? '',
    'abr-arrival-bps': state.arrivalBps?.toFixed(0) ?? '',
    'abr-instant-bps': state.instantBps?.toFixed(0) ?? '',
    'abr-ladder': state.ladder.map((r) => `${r.name}:${r.bitrate.toString()}`).join(','),
    'abr-excluded': state.excluded.map((e) => `${e.name}:${e.cause}`).join(','),
    'abr-probing': state.probing ?? '',
    'abr-unacknowledged': state.unacknowledgedSwitches.toString(),
    'abr-compat': state.compat ?? '',
    'abr-probe-failures': state.probeFailures.toString(),
    'abr-probe-backoff-until-ms': state.probeBackoffUntilMs?.toString() ?? '',
    'abr-switches': state.switches.toString(),
    'abr-samples': state.samples.toString(),
    'abr-queued-ahead-ms': state.queuedAheadMs?.toFixed(0) ?? '',
    'abr-lost-bytes': state.lostBytes?.toString() ?? '',
    'abr-peer-estimate-bps': state.peerEstimatedRecvRateBps?.toString() ?? '',
    'abr-probe-rtt-ms': state.probeRttMs?.toString() ?? '',
  };
}
