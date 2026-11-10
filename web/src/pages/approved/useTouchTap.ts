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
  onPointerCancel: (event: PointerEvent<HTMLElement>) => void;
  onClick: () => void;
} {
  // Every finger is tracked by its own pointerId: a second finger on the same
  // button must not displace the first.
  const down = useRef<Set<number>>(new Set());
  const handledAt = useRef(-Infinity);
  return {
    onPointerDown: (event): void => {
      if (event.pointerType === 'touch') down.current.add(event.pointerId);
    },
    onPointerUp: (event): void => {
      if (event.pointerType !== 'touch' || !down.current.delete(event.pointerId)) return;
      const rect = event.currentTarget.getBoundingClientRect();
      const inside =
        event.clientX >= rect.left && event.clientX <= rect.right &&
        event.clientY >= rect.top && event.clientY <= rect.bottom;
      if (!inside || event.currentTarget.hasAttribute('disabled')) return;
      handledAt.current = performance.now();
      action();
    },
    onPointerCancel: (event): void => {
      down.current.delete(event.pointerId);
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
