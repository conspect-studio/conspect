// jsdom has no layout, so @tanstack/react-virtual would see a 0-height scroll
// element and render nothing. Fall back to a nominal viewport when the measured
// rect is empty; in a real browser the measured rect always wins.
import { observeElementRect } from '@tanstack/react-virtual';
import type { Virtualizer } from '@tanstack/react-virtual';

const FALLBACK = { width: 800, height: 640 };

/** `observeElementRect` with a non-zero fallback viewport. */
export function observeRectWithFallback<T extends Element>(
  instance: Virtualizer<T, Element>,
  cb: (rect: { width: number; height: number }) => void,
): (() => void) | undefined {
  return observeElementRect(instance, (rect) => {
    cb(rect.height === 0 ? FALLBACK : rect);
  });
}
