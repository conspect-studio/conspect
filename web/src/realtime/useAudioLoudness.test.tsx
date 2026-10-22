// useAudioLoudness: the `audio.loudness` topic binding (AUD-8). The engine
// pushes a conflated EBU R128 loudness sample (M/S/I/LRA/dBTP + compliance
// reference); this hook folds the latest into a small state and applies the
// CLIENT-SIDE ballistics (the wire carries raw values; the browser does the
// display decay/peak-hold) per ADR-R006.
//
// These tests cover the pure, deterministic core:
//   - parseAudioLoudness: defensive narrowing of an unknown `data` body.
//   - classifyLoudness: the compliance colour-zone classifier (in-spec / near /
//     out, plus the over-ceiling / clip flag) the meter colours against.
//   - LoudnessBallistics: client-side momentary decay + true-peak hold.
import { describe, expect, it } from 'vitest';

import {
  LoudnessBallistics,
  classifyLoudness,
  parseAudioLoudness,
} from './useAudioLoudness';
import type { AudioLoudnessSample } from './useAudioLoudness';

function sample(over: Partial<AudioLoudnessSample> = {}): AudioLoudnessSample {
  return {
    program: 0,
    target_lufs: -23,
    ceiling_dbtp: -1.5,
    tolerance_lu: 1,
    sampled_hz: 10,
    ...over,
  };
}

describe('parseAudioLoudness', () => {
  it('returns undefined for non-object data', () => {
    expect(parseAudioLoudness(null)).toBeUndefined();
    expect(parseAudioLoudness('nope')).toBeUndefined();
    expect(parseAudioLoudness(7)).toBeUndefined();
  });

  it('returns undefined when the required compliance reference is missing', () => {
    // Without the always-present reference the meter cannot colour, so the
    // sample is rejected rather than rendered against guessed thresholds.
    expect(
      parseAudioLoudness({ program: 0, sampled_hz: 10, ceiling_dbtp: -1.5, tolerance_lu: 1 }),
    ).toBeUndefined();
    expect(
      parseAudioLoudness({ program: 0, sampled_hz: 10, target_lufs: -23, tolerance_lu: 1 }),
    ).toBeUndefined();
  });

  it('parses a minimal sample (reference only, gated silence)', () => {
    const parsed = parseAudioLoudness({
      program: 1,
      target_lufs: -16,
      ceiling_dbtp: -1.5,
      tolerance_lu: 1,
      sampled_hz: 10,
    });
    expect(parsed).not.toBeUndefined();
    expect(parsed?.program).toBe(1);
    expect(parsed?.target_lufs).toBe(-16);
    // The integrating fields are absent below the gate — never a false value.
    expect(parsed?.momentary).toBeUndefined();
    expect(parsed?.short_term).toBeUndefined();
    expect(parsed?.integrated).toBeUndefined();
    expect(parsed?.true_peak_dbtp).toBeUndefined();
    expect(parsed?.gain_db).toBeUndefined();
  });

  it('parses the measured loudness fields when present', () => {
    const parsed = parseAudioLoudness({
      program: 0,
      momentary: -22.5,
      short_term: -23.1,
      integrated: -23,
      lra: 4.2,
      true_peak_dbtp: -2.3,
      target_lufs: -23,
      ceiling_dbtp: -1.5,
      tolerance_lu: 1,
      gain_db: 0.4,
      sampled_hz: 10,
    });
    expect(parsed?.momentary).toBe(-22.5);
    expect(parsed?.short_term).toBe(-23.1);
    expect(parsed?.integrated).toBe(-23);
    expect(parsed?.lra).toBe(4.2);
    expect(parsed?.true_peak_dbtp).toBe(-2.3);
    expect(parsed?.gain_db).toBe(0.4);
  });

  it('ignores a mistyped measured field (undefined, not 0)', () => {
    const parsed = parseAudioLoudness({
      program: 0,
      momentary: 'loud',
      short_term: null,
      target_lufs: -23,
      ceiling_dbtp: -1.5,
      tolerance_lu: 1,
      sampled_hz: 10,
    });
    expect(parsed?.momentary).toBeUndefined();
    expect(parsed?.short_term).toBeUndefined();
  });
});

describe('classifyLoudness', () => {
  it('is "absent" when there is no loudness measurement (gated silence)', () => {
    const z = classifyLoudness(sample(), undefined);
    expect(z.loudnessZone).toBe('absent');
  });

  it('is "in-spec" within ±tolerance of the target', () => {
    // target -23 ± 1 LU → [-24, -22] is in-spec.
    expect(classifyLoudness(sample(), -23).loudnessZone).toBe('in-spec');
    expect(classifyLoudness(sample(), -22).loudnessZone).toBe('in-spec');
    expect(classifyLoudness(sample(), -24).loudnessZone).toBe('in-spec');
  });

  it('is "near" just outside the in-spec band but within 2x tolerance', () => {
    // Between 1 and 2 LU off target → near (amber).
    expect(classifyLoudness(sample(), -21.5).loudnessZone).toBe('near');
    expect(classifyLoudness(sample(), -25).loudnessZone).toBe('near');
  });

  it('is "out" beyond 2x tolerance from target', () => {
    expect(classifyLoudness(sample(), -19).loudnessZone).toBe('out');
    expect(classifyLoudness(sample(), -28).loudnessZone).toBe('out');
  });

  it('flags the true-peak as over the ceiling at or above it', () => {
    // ceiling -1.5 dBTP: -1.0 is over, -2.0 is under.
    expect(classifyLoudness(sample({ true_peak_dbtp: -1.0 }), -23).peakOver).toBe(true);
    expect(classifyLoudness(sample({ true_peak_dbtp: -2.0 }), -23).peakOver).toBe(false);
    // Absent dBTP is never "over" (no false alarm).
    expect(classifyLoudness(sample(), -23).peakOver).toBe(false);
  });
});

describe('LoudnessBallistics', () => {
  it('tracks a rising momentary instantly (fast attack) and decays slowly', () => {
    const b = new LoudnessBallistics();
    // Attack: a louder reading is adopted immediately.
    b.push(sample({ momentary: -30 }));
    expect(b.displayMomentary()).toBeCloseTo(-30, 5);
    b.push(sample({ momentary: -18 }));
    expect(b.displayMomentary()).toBeCloseTo(-18, 5);
    // Decay: a quieter reading is approached gradually, not jumped to.
    b.push(sample({ momentary: -40 }));
    const decayed = b.displayMomentary();
    expect(decayed).toBeGreaterThan(-40); // has not fallen all the way yet
    expect(decayed).toBeLessThan(-18); // but is moving down
  });

  it('holds the true-peak at its maximum (peak-hold) until reset', () => {
    const b = new LoudnessBallistics();
    b.push(sample({ true_peak_dbtp: -6 }));
    expect(b.heldPeakDbtp()).toBeCloseTo(-6, 5);
    b.push(sample({ true_peak_dbtp: -2 }));
    expect(b.heldPeakDbtp()).toBeCloseTo(-2, 5);
    // A lower subsequent peak does NOT lower the held value.
    b.push(sample({ true_peak_dbtp: -10 }));
    expect(b.heldPeakDbtp()).toBeCloseTo(-2, 5);
    // Reset clears the hold.
    b.resetPeak();
    b.push(sample({ true_peak_dbtp: -8 }));
    expect(b.heldPeakDbtp()).toBeCloseTo(-8, 5);
  });

  it('passes integrated/short-term through as the raw measured value (no ballistics)', () => {
    // The slow meters are read directly from the latest sample — the ballistics
    // are only the momentary decay + peak hold (the fast bar + the peak marker).
    const b = new LoudnessBallistics();
    b.push(sample({ short_term: -23.4, integrated: -23.0, lra: 5.1 }));
    expect(b.latest()?.short_term).toBe(-23.4);
    expect(b.latest()?.integrated).toBe(-23.0);
    expect(b.latest()?.lra).toBe(5.1);
  });
});
