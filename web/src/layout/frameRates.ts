// Common canvas frame rates mapped to their EXACT rationals (never floats).

/** One preset: display label → exact rational. */
export interface FrameRatePreset {
  readonly label: string;
  readonly rational: string;
}

/** The common rates, in display order. Rationals are exact (NTSC = n*1000/1001). */
export const FRAME_RATE_PRESETS: readonly FrameRatePreset[] = [
  { label: '23.976 (24000/1001)', rational: '24000/1001' },
  { label: '24', rational: '24/1' },
  { label: '25 (PAL)', rational: '25/1' },
  { label: '29.97 (30000/1001, NTSC)', rational: '30000/1001' },
  { label: '30', rational: '30/1' },
  { label: '50 (PAL)', rational: '50/1' },
  { label: '59.94 (60000/1001, NTSC)', rational: '60000/1001' },
  { label: '60', rational: '60/1' },
];

/** Normalise a rational for preset matching: trim, and `n` alone means `n/1`. */
export function normaliseRational(value: string): string {
  const v = value.trim();
  return /^\d+$/.test(v) ? `${v}/1` : v;
}

/** The preset whose rational equals `value`, if any. */
export function presetFor(value: string): FrameRatePreset | undefined {
  const n = normaliseRational(value);
  return FRAME_RATE_PRESETS.find((p) => p.rational === n);
}
