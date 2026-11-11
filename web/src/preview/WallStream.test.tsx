import { act, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { WallStreamProvider, WallCrop, useWallSurface } from './WallStream';
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
});
