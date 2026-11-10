// A custom vertical fader built on Pointer Events, NOT a rotated native
// <input type="range">. Every finger is tracked by its own pointerId and
// captured by the fader it started on, so several faders can be ridden at the
// same time (multi-touch). The value comes from clientY relative to the
// track's getBoundingClientRect — no dependence on CSS transforms — and
// touch-action:none keeps the page from scrolling during a drag.
//
// Geometry matches the visual scale in AudioPage: the 48 px knob centre
// travels (height − 48 px), so the knob centre is at 24 px from the top at
// `max` and 24 px from the bottom at `min`.
import { type JSX, type KeyboardEvent, type PointerEvent, useRef } from 'react';

import { FADER_KNOB_PX as KNOB_PX, faderValueFromClientY } from './faderMath';

export interface FaderProps {
  /** The displayed value (already clamped into [min, max]). */
  readonly value: number;
  readonly min: number;
  readonly max: number;
  /** Integer step used for drag quantisation and Arrow keys. */
  readonly step?: number;
  readonly disabled?: boolean;
  readonly 'aria-label': string;
  readonly 'aria-valuetext'?: string;
  readonly 'data-testid'?: string;
  readonly 'data-staged'?: boolean;
  readonly elementRef?: ((element: HTMLDivElement | null) => void) | undefined;
  /** Called with each new quantised value from drag or Arrow/Home/End keys. */
  readonly onValueChange: (next: number) => void;
  /** Runs first; calling preventDefault() skips the built-in keys. */
  readonly onKeyDown?: (event: KeyboardEvent<HTMLDivElement>) => void;
}

export function Fader({
  value,
  min,
  max,
  step = 1,
  disabled = false,
  elementRef,
  onValueChange,
  onKeyDown,
  'data-testid': testId,
  'data-staged': staged,
  ...aria
}: FaderProps): JSX.Element {
  const ref = useRef<HTMLDivElement | null>(null);
  // The one pointer currently driving THIS fader, and where in the knob it
  // grabbed. Other fingers on other faders have their own component state.
  const active = useRef<{ id: number; offset: number } | undefined>(undefined);
  const pos = max === min ? 0 : (value - min) / (max - min);

  const apply = (clientY: number, offset: number): void => {
    const element = ref.current;
    if (element === null) return;
    const next = faderValueFromClientY(clientY, element.getBoundingClientRect(), min, max, step, offset);
    onValueChange(next);
  };

  const release = (event: PointerEvent<HTMLDivElement>): void => {
    if (active.current?.id !== event.pointerId) return;
    active.current = undefined;
    const element = ref.current;
    if (element !== null && 'hasPointerCapture' in element && element.hasPointerCapture(event.pointerId)) {
      element.releasePointerCapture(event.pointerId);
    }
  };

  return (
    <div
      ref={(element): void => {
        ref.current = element;
        elementRef?.(element);
      }}
      role="slider"
      tabIndex={disabled ? -1 : 0}
      data-testid={testId}
      data-staged={staged === true ? 'true' : undefined}
      aria-orientation="vertical"
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuenow={value}
      aria-valuetext={aria['aria-valuetext']}
      aria-label={aria['aria-label']}
      aria-disabled={disabled ? true : undefined}
      className="audio-fader-hit"
      style={{ touchAction: 'none' }}
      onPointerDown={(event): void => {
        if (disabled || active.current !== undefined) return;
        if (event.pointerType === 'mouse' && event.button !== 0) return;
        const element = event.currentTarget;
        const rect = element.getBoundingClientRect();
        const knobCentre = rect.top + KNOB_PX / 2 + (rect.height - KNOB_PX) * (1 - pos);
        // Grabbing the knob keeps the finger's offset (no jump); grabbing the
        // track elsewhere jumps the knob under the finger.
        const offset = Math.abs(event.clientY - knobCentre) <= KNOB_PX / 2 ? knobCentre - event.clientY : 0;
        active.current = { id: event.pointerId, offset };
        if ('setPointerCapture' in element) element.setPointerCapture(event.pointerId);
        event.preventDefault();
        element.focus({ preventScroll: true });
        apply(event.clientY, offset);
      }}
      onPointerMove={(event): void => {
        const drag = active.current;
        if (drag?.id !== event.pointerId) return;
        apply(event.clientY, drag.offset);
      }}
      onPointerUp={release}
      // A cancelled pointer (system gesture, palm rejection) keeps whatever
      // value it had reached and never touches another fader.
      onPointerCancel={release}
      onLostPointerCapture={release}
      onKeyDown={(event): void => {
        onKeyDown?.(event);
        if (event.defaultPrevented || disabled) return;
        let next: number | undefined;
        if (event.key === 'ArrowUp' || event.key === 'ArrowRight') next = value + step;
        else if (event.key === 'ArrowDown' || event.key === 'ArrowLeft') next = value - step;
        else if (event.key === 'Home') next = min;
        else if (event.key === 'End') next = max;
        if (next === undefined) return;
        event.preventDefault();
        onValueChange(Math.min(max, Math.max(min, next)));
      }}
    >
      <span
        aria-hidden="true"
        className="audio-fader-knob"
        style={{ top: `calc((100% - ${String(KNOB_PX)}px) * ${String(1 - pos)})` }}
      />
    </div>
  );
}
