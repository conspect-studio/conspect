import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  commandKeyer, flyPresetRect, keyerFillRedacted, keyerFillSource, keyerKeyRedacted,
  keyerKeySource, keyerRampWeight, keyerRamping, keyTallyForSource, planKeyer,
} from './keyers';
import type { KeyerRow } from './keyers';

afterEach(() => { vi.unstubAllGlobals(); });

const ROW: KeyerRow = {
  id: 'dsk1', family: 'dsk', fill: 'gfx-channel-1', key: null, type: 'linear',
  fly: null, mask: null, on_air: false, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', stale: false, pending: false,
};
/** `ramp` is a free-form object in the generated schema; build a row with a concrete ramp shape for these tests. */
function withRamp(ramp: Record<string, unknown>): KeyerRow {
  return { ...ROW, ramp } as unknown as KeyerRow;
}

describe('keyerFillSource / keyerKeySource / redaction', () => {
  it('reads a normal fill and a self-key (null) as no key source', () => {
    expect(keyerFillSource(ROW)).toBe('gfx-channel-1');
    expect(keyerKeySource(ROW)).toBeUndefined();
    expect(keyerFillRedacted(ROW)).toBe(false);
    expect(keyerKeyRedacted(ROW)).toBe(false);
  });

  it('treats "redacted" as hidden, not a real source id', () => {
    const row = { ...ROW, fill: 'redacted', key: 'redacted' };
    expect(keyerFillSource(row)).toBeUndefined();
    expect(keyerKeySource(row)).toBeUndefined();
    expect(keyerFillRedacted(row)).toBe(true);
    expect(keyerKeyRedacted(row)).toBe(true);
  });
});

describe('keyerRamping / keyerRampWeight', () => {
  it('is not ramping without a well-shaped ramp descriptor', () => {
    expect(keyerRamping(ROW)).toBe(false);
    expect(keyerRampWeight(ROW, 100)).toBeUndefined();
  });

  it('computes weight going on: k/N at tick k', () => {
    const row = withRamp({ start_tick: 90, rate: 20, to_on: true });
    expect(keyerRamping(row)).toBe(true);
    expect(keyerRampWeight(row, 100)).toBeCloseTo(0.5);
    expect(keyerRampWeight(row, 90)).toBeCloseTo(0);
    expect(keyerRampWeight(row, 110)).toBeCloseTo(1);
    expect(keyerRampWeight(row, 200)).toBeCloseTo(1); // clamped
  });

  it('computes weight going off: 1 - k/N', () => {
    const row = withRamp({ start_tick: 90, rate: 20, to_on: false });
    expect(keyerRampWeight(row, 100)).toBeCloseTo(0.5);
    expect(keyerRampWeight(row, 110)).toBeCloseTo(0);
  });

  it('ignores a malformed ramp object', () => {
    const row = withRamp({ start_tick: 90 });
    expect(keyerRamping(row)).toBe(false);
  });
});

describe('keyTallyForSource', () => {
  const tally = { program: ['gfx-channel-1'], preview: ['cam-2'] };
  it('badges a keyer fill on Program as program', () => {
    expect(keyTallyForSource('gfx-channel-1', [ROW], tally)).toBe('program');
  });
  it('badges a keyer fill on Preview as preview', () => {
    const previewRow = { ...ROW, fill: 'cam-2' };
    expect(keyTallyForSource('cam-2', [previewRow], tally)).toBe('preview');
  });
  it('is undefined for a source that is not a keyer fill/key at all', () => {
    expect(keyTallyForSource('cam-1', [ROW], tally)).toBeUndefined();
  });
  it('is undefined for a keyer source that the tally does not carry', () => {
    const idle = { ...ROW, fill: 'gfx-channel-2' };
    expect(keyTallyForSource('gfx-channel-2', [idle], tally)).toBeUndefined();
  });
  it('never matches a redacted row to a real source id', () => {
    const redacted = { ...ROW, fill: 'redacted' };
    expect(keyTallyForSource('gfx-channel-1', [redacted], tally)).toBeUndefined();
  });
});

describe('flyPresetRect', () => {
  const canvas = { w: 1920, h: 1080 };
  const box = { w: 480, h: 270 };
  it('full ignores the box and covers the canvas', () => {
    expect(flyPresetRect('full', canvas, box)).toEqual({ x: 0, y: 0, w: 1920, h: 1080 });
  });
  it('corners place the box at each edge', () => {
    expect(flyPresetRect('top_left', canvas, box)).toEqual({ x: 0, y: 0, w: 480, h: 270 });
    expect(flyPresetRect('top_right', canvas, box)).toEqual({ x: 1440, y: 0, w: 480, h: 270 });
    expect(flyPresetRect('bottom_left', canvas, box)).toEqual({ x: 0, y: 810, w: 480, h: 270 });
    expect(flyPresetRect('bottom_right', canvas, box)).toEqual({ x: 1440, y: 810, w: 480, h: 270 });
  });
  it('centre places the box in the middle', () => {
    expect(flyPresetRect('centre', canvas, box)).toEqual({ x: 720, y: 405, w: 480, h: 270 });
  });
  it('clamps an oversize box to the canvas', () => {
    expect(flyPresetRect('top_left', canvas, { w: 4000, h: 3000 })).toEqual({ x: 0, y: 0, w: 1920, h: 1080 });
  });
});

describe('planKeyer / commandKeyer', () => {
  const req = { me: 'me1', program: 'main', action: 'keyer_cut' as const, keyer: 'dsk1', on: true };

  it('planKeyer posts to the shared /switcher/plan route and parses a KeyerPlan', async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({ can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false }), { status: 200 }));
    vi.stubGlobal('fetch', fetchMock);
    const plan = await planKeyer(req);
    expect(plan.can_take).toBe(true);
    expect(fetchMock.mock.calls[0]?.[0]).toBe('/api/v1/switcher/plan');
  });

  it('commandKeyer posts to /switcher/commands/<action> and parses the accepted body', async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({ operation_id: 'op-1', kind: 'keyer_cut' }), { status: 202 }));
    vi.stubGlobal('fetch', fetchMock);
    const accepted = await commandKeyer('keyer_cut', req, { idempotencyKey: 'k1' });
    expect(accepted).toEqual({ operation_id: 'op-1', kind: 'keyer_cut' });
    expect(fetchMock.mock.calls[0]?.[0]).toBe('/api/v1/switcher/commands/keyer_cut');
    expect(new Headers(fetchMock.mock.calls[0]?.[1]?.headers).get('Idempotency-Key')).toBe('k1');
  });
});
