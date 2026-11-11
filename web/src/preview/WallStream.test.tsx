import { act, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { WallStreamProvider, WallCrop, useWallStatus, useWallSurface } from './WallStream';
import type { WallDeps } from './wallStore';
import { WallUnavailable, cropRect, findCell, type WallGeometry } from './wallApi';

const geometry: WallGeometry = {
  generation: 1, width: 640, height: 360, cadence: { num: 15000, den: 1001 }, preview_source: 'b',
  cells: [{ source: 'a', x: 0, y: 0, w: 320, h: 180 }, { source: 'b', x: 320, y: 0, w: 320, h: 180 }],
};

describe('wall geometry helpers', () => {
  it('finds cells and tolerates missing ones', () => {
    expect(findCell(geometry, 'b')?.x).toBe(320);
    expect(findCell(geometry, 'zzz')).toBeUndefined();
    expect(findCell(geometry, null)).toBeUndefined();
    expect(findCell(undefined, 'a')).toBeUndefined();
    const cold = { ...geometry, cells: [{ source: 'a', x: 0, y: 0, w: 320, h: 180, live: false }] };
    expect(findCell(cold, 'a')).toBeUndefined();
  });
  it('computes and clamps the crop rectangle', () => {
    expect(cropRect(geometry.cells[1]!, 640, 360)).toEqual([320, 0, 320, 180]);
    expect(cropRect({ source: 'x', x: 600, y: 0, w: 320, h: 180 }, 640, 360)).toEqual([600, 0, 40, 180]);
    expect(cropRect({ source: 'x', x: 700, y: 0, w: 10, h: 10 }, 640, 360)).toBeUndefined();
  });
});

function Probe({ id }: { readonly id: string }) {
  const surface = useWallSurface(id);
  return surface.live ? <WallCrop source={id} label={id} testId={`crop-${id}`} /> : <span data-testid={`still-${id}`} />;
}

function deps(over: Partial<WallDeps> & { frames?: boolean } = {}) {
  const lease = vi.fn();
  const frame = { displayWidth: 640, displayHeight: 360, clone() { return this; }, close: vi.fn() } as unknown as VideoFrame;
  const play = vi.fn(async (o: { onActive: () => void; onFrame?: (f: VideoFrame) => void; signal: AbortSignal }) => {
    lease();
    o.onActive();
    if (over.frames !== false) o.onFrame?.(frame);
    await new Promise<void>((resolve) => { o.signal.addEventListener('abort', () => { resolve(); }); });
  });
  const value: WallDeps = {
    fetchWall: over.fetchWall ?? vi.fn(() => Promise.resolve(geometry)),
    play: play as unknown as WallDeps['play'],
    createLeaseApi: () => ({ acquire: vi.fn(), release: vi.fn() }),
  };
  return { value, lease, play };
}

beforeEach(() => { vi.useFakeTimers(); HTMLCanvasElement.prototype.getContext = vi.fn(() => ({ drawImage: vi.fn() })) as never; });

/** Records every 2D drawImage call: whole-frame conversions (3 args) and cell crops (9 args). */
function recordDraws() {
  const calls: unknown[][] = [];
  HTMLCanvasElement.prototype.getContext = vi.fn(() => ({ drawImage: (...args: unknown[]) => { calls.push(args); } })) as never;
  return {
    conversions: () => calls.filter((call) => call.length === 3),
    crops: () => calls.filter((call) => call.length === 9),
  };
}
afterEach(() => { vi.useRealTimers(); });

describe('WallStreamProvider', () => {
  it('opens exactly one lease for N tiles and marks crops live', async () => {
    const d = deps();
    render(<WallStreamProvider deps={d.value}><Probe id="a" /><Probe id="b" /><Probe id="a" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); });
    expect(d.lease).toHaveBeenCalledTimes(1);
    expect(screen.getAllByTestId(/^crop-/)).toHaveLength(3);
    expect(document.querySelectorAll('[data-wall-live="1"]')).toHaveLength(3);
    expect(window.__wallStats?.sessions).toBe(1);
    expect(window.__wallStats?.decoders).toBe(1);
    expect(window.__wallStats?.frames).toBe(1);
  });

  it.each([403, 503])('falls back to stills when the wall answers %i', async () => {
    const d = deps({ fetchWall: vi.fn(() => Promise.reject(new WallUnavailable('x'))) });
    render(<WallStreamProvider deps={d.value}><Probe id="a" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); });
    expect(screen.getByTestId('still-a')).toBeTruthy();
    expect(d.lease).not.toHaveBeenCalled();
  });

  it('falls back for a source with no cell', async () => {
    const d = deps();
    render(<WallStreamProvider deps={d.value}><Probe id="a" /><Probe id="nope" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); });
    expect(screen.getByTestId('crop-a')).toBeTruthy();
    expect(screen.getByTestId('still-nope')).toBeTruthy();
  });

  it('falls back when no frame arrives for more than 2 s', async () => {
    const d = deps({ frames: false });
    render(<WallStreamProvider deps={d.value}><Probe id="a" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
    expect(screen.getByTestId('still-a')).toBeTruthy();
  });

  it('drops the old-layout picture on a generation change and reopens one lease', async () => {
    const next: WallGeometry = { ...geometry, generation: 2, cells: [{ source: 'a', x: 320, y: 0, w: 320, h: 180 }] };
    let answer = geometry;
    const d = deps({ fetchWall: vi.fn(() => Promise.resolve(answer)) });
    const draws = recordDraws();
    // Each session gets its own frame. Only the first session delivers one at once;
    // the second waits, so the test can see what surfaces show in between.
    const clones: ReturnType<typeof vi.fn>[] = [];
    const feeds: (() => void)[] = [];
    const inner = d.value.play;
    const play = (o: Parameters<WallDeps['play']>[0]): ReturnType<WallDeps['play']> => {
      const clone = vi.fn();
      clones.push(clone);
      const frame = { displayWidth: 640, displayHeight: 360, clone, close: vi.fn() } as unknown as VideoFrame;
      const forward = o.onFrame;
      const first = feeds.length === 0;
      feeds.push(() => { forward?.(frame); });
      return inner({ ...o, onFrame: () => { if (first) forward?.(frame); } });
    };
    d.value = { ...d.value, play };
    render(<WallStreamProvider deps={d.value}><Probe id="a" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); });
    expect(screen.getByTestId('crop-a')).toBeTruthy();
    expect(draws.crops().at(-1)?.slice(1, 5)).toEqual([0, 0, 320, 180]);
    const before = draws.crops().length;
    answer = next;
    await act(async () => { await vi.advanceTimersByTimeAsync(2100); });
    // The old session is retired and a second lease is open, but no frame of the new
    // layout has arrived: the surface is back on its still and nothing was cropped
    // from the old picture with the new cell.
    expect(d.lease).toHaveBeenCalledTimes(2);
    expect(feeds).toHaveLength(2);
    expect(screen.getByTestId('still-a')).toBeTruthy();
    expect(draws.crops()).toHaveLength(before);
    // The first frame of the new session brings the surface back, cropped with the new cell.
    await act(async () => { feeds[1]?.(); await vi.advanceTimersByTimeAsync(600); });
    expect(screen.getByTestId('crop-a')).toBeTruthy();
    expect(draws.crops().at(-1)?.slice(1, 5)).toEqual([320, 0, 320, 180]);
    // The store keeps no decoded frame: it never clones one, so there is none to leak.
    for (const clone of clones) expect(clone).not.toHaveBeenCalled();
  });

  it('converts each decoded frame once, however many surfaces crop it', async () => {
    const d = deps({ frames: false });
    const draws = recordDraws();
    let feed: (() => void) | undefined;
    const inner = d.value.play;
    const frame = { displayWidth: 640, displayHeight: 360, clone: vi.fn(), close: vi.fn() } as unknown as VideoFrame;
    const play = (o: Parameters<WallDeps['play']>[0]): ReturnType<WallDeps['play']> => {
      feed = () => { o.onFrame?.(frame); };
      return inner(o);
    };
    d.value = { ...d.value, play };
    render(<WallStreamProvider deps={d.value}><Probe id="a" /><Probe id="b" /><Probe id="a" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); });
    await act(async () => { feed?.(); await vi.advanceTimersByTimeAsync(600); });
    expect(screen.getAllByTestId(/^crop-/)).toHaveLength(3);
    // One whole-frame conversion; each surface then copies its cell from that canvas.
    expect(draws.conversions()).toHaveLength(1);
    expect(draws.conversions()[0]?.[0]).toBe(frame);
    expect(draws.crops()).toHaveLength(3);
    for (const crop of draws.crops()) expect(crop[0]).toBeInstanceOf(HTMLCanvasElement);
    await act(async () => { feed?.(); await Promise.resolve(); });
    expect(draws.conversions()).toHaveLength(2);
    expect(draws.crops()).toHaveLength(6);
  });

  it('backs off instead of polling every 2 s when the wall is off', async () => {
    const fetchWall = vi.fn(() => Promise.reject(new WallUnavailable('off')));
    const d = deps({ fetchWall });
    render(<WallStreamProvider deps={d.value}><Probe id="a" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(20_000); });
    // 0 s, +5 s, +10 s: three probes in 20 s, not ten.
    expect(fetchWall.mock.calls.length).toBeLessThanOrEqual(3);
    expect(screen.getByTestId('still-a')).toBeTruthy();
  });

  it('uses one WebRTC video for all surfaces after MoQ fails and releases it on unmount', async () => {
    const d = deps({ frames: false });
    const draws = recordDraws();
    const failed = vi.fn(() => Promise.reject(new Error('MoQ transport unavailable')));
    let videoFrame: (() => void) | undefined;
    let released = false;
    const whep = vi.fn(async (o: Parameters<NonNullable<WallDeps['playWhep']>>[0]) => {
      o.onActive();
      const video = document.createElement('video');
      Object.defineProperties(video, { videoWidth: { value: 640 }, videoHeight: { value: 360 } });
      videoFrame = () => { o.onFrame(video); };
      await new Promise<void>((resolve) => { o.signal.addEventListener('abort', () => { released = true; resolve(); }, { once: true }); });
    });
    d.value = { ...d.value, play: failed, playWhep: whep };
    const app = render(<WallStreamProvider deps={d.value}><Probe id="a" /><Probe id="b" /><Probe id="a" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); videoFrame?.(); await Promise.resolve(); });
    expect(failed).toHaveBeenCalledTimes(1);
    expect(whep).toHaveBeenCalledTimes(1);
    expect(screen.getAllByTestId(/^crop-/)).toHaveLength(3);
    expect(draws.conversions()).toHaveLength(1);
    expect(draws.crops()).toHaveLength(3);
    app.unmount();
    expect(released).toBe(true);
  });

  it('drops old-layout video before accepting new-generation WebRTC frames', async () => {
    let layout = geometry;
    const d = deps({ fetchWall: vi.fn(() => Promise.resolve(layout)) });
    const draws = recordDraws();
    const frames: (() => void)[] = [];
    const whep = vi.fn(async (o: Parameters<NonNullable<WallDeps['playWhep']>>[0]) => {
      const video = document.createElement('video');
      Object.defineProperties(video, { videoWidth: { value: 640 }, videoHeight: { value: 360 } });
      frames.push(() => { o.onFrame(video); });
      await new Promise<void>((resolve) => { o.signal.addEventListener('abort', () => { resolve(); }, { once: true }); });
    });
    d.value = { ...d.value, play: vi.fn(() => Promise.reject(new Error('no WebTransport'))), playWhep: whep };
    render(<WallStreamProvider deps={d.value}><Probe id="a" /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); frames[0]?.(); await Promise.resolve(); });
    expect(screen.getByTestId('crop-a')).toBeTruthy();
    const before = draws.crops().length;
    layout = { ...geometry, generation: 2, cells: [{ source: 'a', x: 320, y: 0, w: 320, h: 180 }] };
    await act(async () => { await vi.advanceTimersByTimeAsync(2100); });
    expect(whep).toHaveBeenCalledTimes(2);
    await act(async () => { frames[0]?.(); await Promise.resolve(); });
    expect(screen.getByTestId('still-a')).toBeTruthy();
    expect(draws.crops()).toHaveLength(before);
    await act(async () => { frames[1]?.(); await Promise.resolve(); });
    expect(screen.getByTestId('crop-a')).toBeTruthy();
    expect(draws.crops().at(-1)?.slice(1, 5)).toEqual([320, 0, 320, 180]);
  });

  it('states why the wall is on stills when the live session fails', async () => {
    const d = deps();
    const failing = (() => Promise.reject(new Error('remote error: 2 (unauthorized)'))) as unknown as WallDeps['play'];
    d.value = { ...d.value, play: failing, playWhep: vi.fn(() => Promise.reject(new Error('WHEP 503'))) };
    function Reason() { const w = useWallStatus(); return <span data-testid="reason">{w.live ? 'live' : w.reason ?? ''}</span>; }
    render(<WallStreamProvider deps={d.value}><Reason /></WallStreamProvider>);
    await act(async () => { await vi.advanceTimersByTimeAsync(100); });
    expect(screen.getByTestId('reason').textContent).toContain('unauthorized');
  });
});
