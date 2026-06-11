// Vitest global setup: jest-dom matchers + a deterministic, jsdom-friendly
// environment for the layout editor's component tests.
import '@testing-library/jest-dom/vitest';
import { afterEach } from 'vitest';
import { cleanup } from '@testing-library/react';

// jsdom does not implement these APIs that Radix + react-konva touch; provide
// minimal, typed shims so component tests can mount without throwing.
if (typeof window !== 'undefined') {
  if (typeof window.matchMedia !== 'function') {
    window.matchMedia = (query: string): MediaQueryList => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => false,
    });
  }
  // Radix Select/Dialog probe these Element methods jsdom does not implement.
  if (typeof Element.prototype.hasPointerCapture !== 'function') {
    Element.prototype.hasPointerCapture = (): boolean => false;
  }
  if (typeof Element.prototype.setPointerCapture !== 'function') {
    Element.prototype.setPointerCapture = (): void => undefined;
  }
  if (typeof Element.prototype.releasePointerCapture !== 'function') {
    Element.prototype.releasePointerCapture = (): void => undefined;
  }
  if (typeof Element.prototype.scrollIntoView !== 'function') {
    Element.prototype.scrollIntoView = (): void => undefined;
  }
  if (typeof window.ResizeObserver !== 'function') {
    class ResizeObserverShim {
      observe(): void {
        // no-op: layout measurement is irrelevant in jsdom tests.
      }
      unobserve(): void {
        // no-op
      }
      disconnect(): void {
        // no-op
      }
    }
    window.ResizeObserver = ResizeObserverShim;
  }
}

afterEach(() => {
  cleanup();
});
