import { describe, expect, it } from 'vitest';
import { FRAME_RATE_PRESETS, normaliseRational, presetFor } from './frameRates';

describe('frame-rate presets', () => {
  it('map common rates to EXACT rationals (never floats)', () => {
    const byLabel = new Map(FRAME_RATE_PRESETS.map((p) => [p.label.split(' ')[0], p.rational]));
    expect(byLabel.get('29.97')).toBe('30000/1001');
    expect(byLabel.get('59.94')).toBe('60000/1001');
    expect(byLabel.get('23.976')).toBe('24000/1001');
    expect(byLabel.get('25')).toBe('25/1');
    for (const p of FRAME_RATE_PRESETS) {
      expect(p.rational).toMatch(/^\d+\/\d+$/);
    }
  });

  it('recognise a stored rational as its preset, including a bare integer', () => {
    expect(presetFor('30000/1001')?.label).toMatch(/^29\.97/);
    expect(presetFor(' 25 ')?.rational).toBe('25/1');
    expect(normaliseRational('50')).toBe('50/1');
  });

  it('treat an uncommon rational as custom', () => {
    expect(presetFor('48/1')).toBeUndefined();
  });
});
