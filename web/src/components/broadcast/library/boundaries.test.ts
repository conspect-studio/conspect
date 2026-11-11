// boundary helpers — exact-rational/integer-grid contract tests (audit §5,
// §9.7): the labels and proposals keep the integer index plus the exact
// rational pair (or integer sample rate); float seconds never become
// authority; invalid inputs degrade to honest nulls, never NaN/Infinity.
import { describe, expect, it } from 'vitest';

import type { RationalBoundary } from '../duration';
import {
  boundaryAtGridIndex,
  compareBoundaries,
  describeBoundaryUnit,
  formatBoundaryLabel,
  isSameGrid,
  stepBoundary,
} from './boundaries';

const NTSC: RationalBoundary = {
  kind: 'frames',
  index: 0,
  numerator: 30000,
  denominator: 1001,
};
const AUDIO48K: RationalBoundary = {
  kind: 'samples',
  index: 0,
  sampleRateHz: 48000,
};

describe('formatBoundaryLabel', () => {
  it('labels frames as NDF timecode counting ceil(n/d) frames per second', () => {
    // 30000/1001: the frame field counts 30 per second; the position stays
    // the exact integer index. Label 00:00:02:01 = 2 s field + 1 frame field
    // = integer index 61.
    const one = { ...NTSC, index: 61 };
    expect(formatBoundaryLabel(one)).toBe('00:00:02:01');
  });

  it('labels samples as floored elapsed clock, never float seconds', () => {
    // 96,000 samples @ 48 kHz = exactly 2 s
    expect(formatBoundaryLabel({ ...AUDIO48K, index: 96_000 })).toBe('00:00:02.000');
    // 48,001 samples must floor, not round, to .000
    expect(formatBoundaryLabel({ ...AUDIO48K, index: 48_001 })).toBe('00:00:01.000');
  });

  it('degrades unusable boundaries to null — never NaN', () => {
    expect(formatBoundaryLabel(null)).toBeNull();
    expect(formatBoundaryLabel({ ...NTSC, index: -1 })).toBeNull();
    expect(formatBoundaryLabel({ ...NTSC, index: 1.5 })).toBeNull();
    expect(formatBoundaryLabel({ ...NTSC, index: Number.NaN })).toBeNull();
    expect(formatBoundaryLabel({ ...NTSC, index: 10, numerator: 0 })).toBeNull();
    expect(formatBoundaryLabel({ ...AUDIO48K, index: 10, sampleRateHz: 0 })).toBeNull();
  });
});

describe('describeBoundaryUnit', () => {
  it('carries the integer index and the exact rational pair', () => {
    expect(describeBoundaryUnit({ ...NTSC, index: 250 })).toBe('FR 250 · 30000/1001');
  });

  it('carries the integer sample count and rate', () => {
    expect(describeBoundaryUnit({ ...AUDIO48K, index: 96_000 })).toBe(
      'SMP 96,000 · 48 kHz',
    );
  });

  it('degrades unusable boundaries to null', () => {
    expect(describeBoundaryUnit(undefined)).toBeNull();
    expect(describeBoundaryUnit({ ...AUDIO48K, index: -2 })).toBeNull();
    expect(describeBoundaryUnit({ ...NTSC, index: 2, denominator: 0 })).toBeNull();
  });
});

describe('boundaryAtGridIndex', () => {
  it('builds a frames boundary carrying the exact rational pair', () => {
    expect(
      boundaryAtGridIndex({ kind: 'video', numerator: 24000, denominator: 1001 }, 1234),
    ).toEqual({
      kind: 'frames',
      index: 1234,
      numerator: 24000,
      denominator: 1001,
    });
  });

  it('builds a samples boundary carrying the integer rate', () => {
    expect(boundaryAtGridIndex({ kind: 'audio', sampleRateHz: 44100 }, 96_000)).toEqual({
      kind: 'samples',
      index: 96_000,
      sampleRateHz: 44100,
    });
  });

  it('rejects unusable grids and indices', () => {
    expect(boundaryAtGridIndex(null, 5)).toBeNull();
    expect(boundaryAtGridIndex(undefined, 5)).toBeNull();
    expect(boundaryAtGridIndex({ kind: 'video', numerator: 25, denominator: 1 }, -1)).toBeNull();
    expect(boundaryAtGridIndex({ kind: 'video', numerator: 25, denominator: 1 }, 2.5)).toBeNull();
    expect(boundaryAtGridIndex({ kind: 'audio', sampleRateHz: 0 }, 5)).toBeNull();
  });
});

describe('stepBoundary', () => {
  it('proposes the adjacent index on the same exact grid', () => {
    const next = stepBoundary({ ...NTSC, index: 10 }, 1, null);
    expect(next).toEqual({ ...NTSC, index: 11 });
    const prev = stepBoundary({ ...NTSC, index: 10 }, -1, null);
    expect(prev).toEqual({ ...NTSC, index: 9 });
  });

  it('clamps to zero and to the measured length', () => {
    expect(stepBoundary({ ...NTSC, index: 0 }, -1, null)).toBeNull();
    expect(stepBoundary({ ...AUDIO48K, index: 100 }, 1, 100)).toBeNull();
    expect(stepBoundary({ ...AUDIO48K, index: 99 }, 1, 100)).toEqual({
      ...AUDIO48K,
      index: 100,
    });
  });

  it('refuses unusable boundaries and directions', () => {
    expect(stepBoundary({ ...NTSC, index: -3 }, 1, null)).toBeNull();
    expect(stepBoundary({ ...NTSC, index: 1 }, 2 as never, null)).toBeNull();
    expect(stepBoundary({ ...AUDIO48K, index: 1, sampleRateHz: 0 }, 1, null)).toBeNull();
  });
});

describe('isSameGrid / compareBoundaries', () => {
  it('orders boundaries only on the same grid', () => {
    expect(compareBoundaries({ ...NTSC, index: 5 }, { ...NTSC, index: 9 })).toBe(-1);
    expect(compareBoundaries({ ...NTSC, index: 9 }, { ...NTSC, index: 5 })).toBe(1);
    expect(compareBoundaries({ ...NTSC, index: 5 }, { ...NTSC, index: 5 })).toBe(0);
  });

  it('refuses cross-grid and cross-kind ordering', () => {
    expect(
      compareBoundaries(
        { ...NTSC, index: 5 },
        { kind: 'frames', index: 5, numerator: 25, denominator: 1 },
      ),
    ).toBeNull();
    expect(compareBoundaries({ ...NTSC, index: 5 }, { ...AUDIO48K, index: 5 })).toBeNull();
  });

  it('detects the same grid across rational pairs and sample rates', () => {
    expect(
      isSameGrid({ ...NTSC, index: 1 }, { ...NTSC, index: 2 }),
    ).toBe(true);
    expect(
      isSameGrid({ ...AUDIO48K, index: 1 }, { ...AUDIO48K, index: 2 }),
    ).toBe(true);
    expect(
      isSameGrid(
        { ...AUDIO48K, index: 1 },
        { kind: 'samples', index: 2, sampleRateHz: 44100 },
      ),
    ).toBe(false);
  });
});
