// Duration formatter tests — the production regression contract for the
// prototype's two worst defects (correction-4-review-comfort §N8):
//   1. A valid audio asset with a sample rate must NEVER render "INVALID"
//      (the prototype routed audio through the frame-based timecode path).
//   2. Fractional video cadence (30000/1001) must format via exact integer
//      frame indexes and NDF labels, never a divided float fps, and the
//      exact rational boundary must survive the round trip.
// Unknown assets get a meaningful "unknown" state — never INVALID, never
// NaN, never a fabricated precision grid.
import { describe, expect, it } from 'vitest';

import {
  describeGrid,
  formatAssetDuration,
  groupThousands,
} from './duration';

describe('formatAssetDuration — audio', () => {
  it('formats a 48 kHz asset in elapsed clock units, never INVALID', () => {
    const out = formatAssetDuration({
      grid: { kind: 'audio', sampleRateHz: 48000 },
      units: 96000,
    });
    expect(out.state).toBe('formatted');
    expect(out.duration).toBe('00:00:02.000');
    expect(out.gridOrigin).toBe('measured');
    expect(out.boundary).toEqual({
      kind: 'samples',
      index: 96000,
      sampleRateHz: 48000,
    });
  });

  it('keeps exact sample math at 44.1 kHz', () => {
    const out = formatAssetDuration({
      grid: { kind: 'audio', sampleRateHz: 44100 },
      units: 88200,
    });
    expect(out.duration).toBe('00:00:02.000');
  });

  it('floors sub-millisecond remainders instead of rounding up', () => {
    // 48,001 samples at 48 kHz is 1.0000208… s — the elapsed clock reads the
    // same second as 48,000; the SMP boundary is what distinguishes them.
    const out = formatAssetDuration({
      grid: { kind: 'audio', sampleRateHz: 48000 },
      units: 48001,
    });
    expect(out.duration).toBe('00:00:01.000');
    expect(out.boundary?.kind === 'samples' && out.boundary.index).toBe(48001);
  });

  it('renders hours beyond an hour', () => {
    const out = formatAssetDuration({
      grid: { kind: 'audio', sampleRateHz: 48000 },
      units: 48000 * 3661,
    });
    expect(out.duration).toBe('01:01:01.000');
  });
});

describe('formatAssetDuration — video', () => {
  it('formats fractional NTSC cadence with exact NDF integer frame labels', () => {
    const out = formatAssetDuration({
      grid: { kind: 'video', numerator: 30000, denominator: 1001 },
      units: 300,
    });
    expect(out.state).toBe('formatted');
    expect(out.duration).toBe('00:00:10:00');
    expect(out.boundary).toEqual({
      kind: 'frames',
      index: 300,
      numerator: 30000,
      denominator: 1001,
    });
  });

  it('keeps the frame field exact for the same NTSC second', () => {
    const out = formatAssetDuration({
      grid: { kind: 'video', numerator: 30000, denominator: 1001 },
      units: 304,
    });
    expect(out.duration).toBe('00:00:10:04');
  });

  it('formats integer cadences without inventing a fractional label', () => {
    expect(
      formatAssetDuration({
        grid: { kind: 'video', numerator: 25, denominator: 1 },
        units: 625,
      }).duration,
    ).toBe('00:00:25:00');
    expect(
      formatAssetDuration({
        grid: { kind: 'video', numerator: 50, denominator: 1 },
        units: 75,
      }).duration,
    ).toBe('00:00:01:25');
  });

  it('carries hours and minutes in HH:MM:SS:FF', () => {
    // 107,892 frames at 30-frame NDF labels = 00:59:56:12
    const out = formatAssetDuration({
      grid: { kind: 'video', numerator: 30000, denominator: 1001 },
      units: 107892,
    });
    expect(out.duration).toBe('00:59:56:12');
  });
});

describe('formatAssetDuration — unknown and unmeasured', () => {
  it('reports an asset with no grid as unknown, never INVALID or NaN', () => {
    const out = formatAssetDuration({});
    expect(out.state).toBe('unknown');
    expect(out.duration).toBeNull();
    expect(out.boundary).toBeNull();
    expect(out.grid).toBeNull();
    expect(out.gridOrigin).toBe('none');
  });

  it('never invents a precision grid for units without a cadence', () => {
    const out = formatAssetDuration({ units: 100 });
    expect(out.state).toBe('unknown');
    expect(out.duration).toBeNull();
  });

  it('keeps a known cadence distinct from a measured length', () => {
    const out = formatAssetDuration({
      grid: { kind: 'audio', sampleRateHz: 48000 },
    });
    expect(out.state).toBe('unmeasured');
    expect(out.duration).toBeNull();
    expect(out.grid).toEqual({ kind: 'audio', sampleRateHz: 48000 });
    expect(out.gridOrigin).toBe('measured');
  });

  it('treats invalid grids as unknown instead of throwing', () => {
    for (const grid of [
      { kind: 'audio', sampleRateHz: 0 },
      { kind: 'audio', sampleRateHz: -48000 },
      { kind: 'audio', sampleRateHz: 48000.5 },
      { kind: 'audio', sampleRateHz: Number.NaN },
      { kind: 'audio', sampleRateHz: Number.POSITIVE_INFINITY },
      { kind: 'video', numerator: 0, denominator: 1001 },
      { kind: 'video', numerator: 30000, denominator: 0 },
      { kind: 'video', numerator: 30000.5, denominator: 1001 },
    ] as const) {
      const out = formatAssetDuration({ grid, units: 100 });
      expect(out.state).toBe('unknown');
      expect(out.duration).toBeNull();
    }
  });

  it('treats invalid unit counts as unmeasured, never negative or fractional', () => {
    for (const units of [-1, 1.5, Number.NaN, Number.POSITIVE_INFINITY]) {
      const out = formatAssetDuration({
        grid: { kind: 'video', numerator: 25, denominator: 1 },
        units,
      });
      expect(out.state).toBe('unmeasured');
      expect(out.duration).toBeNull();
      expect(out.boundary).toBeNull();
    }
  });
});

describe('formatAssetDuration — derivative fallback', () => {
  it('inherits the parent grid and marks the origin inherited', () => {
    const out = formatAssetDuration(
      { units: 625 },
      { grid: { kind: 'video', numerator: 25, denominator: 1 }, units: 15000 },
    );
    expect(out.state).toBe('formatted');
    expect(out.duration).toBe('00:00:25:00');
    expect(out.gridOrigin).toBe('inherited');
    expect(out.boundary).toEqual({
      kind: 'frames',
      index: 625,
      numerator: 25,
      denominator: 1,
    });
  });

  it('keeps an unmeasured derivative distinct from an unknown one', () => {
    const out = formatAssetDuration(
      {},
      { grid: { kind: 'audio', sampleRateHz: 48000 }, units: 48000 },
    );
    expect(out.state).toBe('unmeasured');
    expect(out.gridOrigin).toBe('inherited');
  });

  it('falls back to unknown when neither asset nor parent has a grid', () => {
    const out = formatAssetDuration({ units: 10 }, {});
    expect(out.state).toBe('unknown');
    expect(out.gridOrigin).toBe('none');
  });
});

describe('describeGrid', () => {
  it('labels audio grids by sample rate', () => {
    expect(describeGrid({ kind: 'audio', sampleRateHz: 48000 })).toBe('48 kHz');
    expect(describeGrid({ kind: 'audio', sampleRateHz: 44100 })).toBe(
      '44.1 kHz',
    );
  });

  it('labels integer video cadences plainly', () => {
    expect(describeGrid({ kind: 'video', numerator: 25, denominator: 1 })).toBe(
      '25 fps',
    );
  });

  it('labels fractional cadence with the exact rational pair and NDF', () => {
    expect(
      describeGrid({ kind: 'video', numerator: 30000, denominator: 1001 }),
    ).toBe('29.97 (30000/1001) fps NDF');
    expect(
      describeGrid({ kind: 'video', numerator: 24000, denominator: 1001 }),
    ).toBe('23.976 (24000/1001) fps NDF');
  });
});

describe('groupThousands', () => {
  it('groups sample counts the way the operator reads them', () => {
    expect(groupThousands(96000)).toBe('96,000');
    expect(groupThousands(48000)).toBe('48,000');
    expect(groupThousands(999)).toBe('999');
  });
});
