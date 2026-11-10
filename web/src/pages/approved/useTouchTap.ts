import { type PointerEvent, useRef } from 'react';

// Chrome synthesises a `click` only for a single-finger tap: once a second
// finger lands while the first is still down, neither finger clicks. Operators
// ride and tap with several fingers at once, so a touch tap activates on
// pointerup (finger lifted inside the button it started on) and the browser's
// follow-up click for that same tap is swallowed, so each tap registers once.
// Mouse, pen and keyboard keep using click.
export function useTouchTap(action: () => void): {
  onPointerDown: (event: PointerEvent<HTMLElement>) => void;
  onPointerUp: (event: PointerEvent<HTMLElement>) => void;
  onPointerCancel: () => void;
  onClick: () => void;
} {
  const down = useRef<number | undefined>(undefined);
  const handledAt = useRef(-Infinity);
  return {
    onPointerDown: (event): void => {
      if (event.pointerType === 'touch') down.current = event.pointerId;
    },
    onPointerUp: (event): void => {
      if (event.pointerType !== 'touch' || down.current !== event.pointerId) return;
      down.current = undefined;
      const rect = event.currentTarget.getBoundingClientRect();
      const inside =
        event.clientX >= rect.left && event.clientX <= rect.right &&
        event.clientY >= rect.top && event.clientY <= rect.bottom;
      if (!inside || event.currentTarget.hasAttribute('disabled')) return;
      handledAt.current = performance.now();
      action();
    },
    onPointerCancel: (): void => {
      down.current = undefined;
    },
    onClick: (): void => {
      if (performance.now() - handledAt.current < 700) {
        handledAt.current = -Infinity;
        return;
      }
      action();
    },
  };
}
