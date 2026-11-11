export const FADER_KNOB_PX = 48;

/** 0 (min, bottom) … 1 (max, top) for a viewport Y inside the track rect. */
export function faderValueFromClientY(
  clientY: number,
  rect: { readonly top: number; readonly height: number },
  min: number,
  max: number,
  step: number,
  grabOffsetPx = 0,
): number {
  const travel = rect.height - FADER_KNOB_PX;
  if (travel <= 0) return min;
  const pos = 1 - (clientY + grabOffsetPx - (rect.top + FADER_KNOB_PX / 2)) / travel;
  const raw = min + Math.min(1, Math.max(0, pos)) * (max - min);
  const snapped = Math.round(raw / step) * step;
  return Math.min(max, Math.max(min, snapped));
}

