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

  // jsdom does not honour the `<a download>` attribute: it treats a programmatic
  // `anchor.click()` (our file-download helpers — downloadConfigExport, the
  // DataPage/LicencePage snapshot downloads) as a real navigation and queues a
  // deferred `navigate()` on a timer. That timer cannot complete ("Not
  // implemented: navigation") and, when it fires while a Vitest worker is torn
  // down under host load, aborts the process with a libuv `uv__stream_destroy`
  // assertion — a flaky "Worker exited unexpectedly" CI failure. A download is a
  // no-op in jsdom regardless, so cancel the navigating default action for
  // programmatic download-anchor clicks. The click event still dispatches, so
  // observers/spies and the helper's own flow are unaffected.
  HTMLAnchorElement.prototype.click = function click(this: HTMLAnchorElement): void {
    // Dispatch a real click so download handlers, spies and listeners still
    // observe it; for a `download` anchor pre-cancel the event so jsdom's
    // navigating default action (the deferred, worker-aborting navigate()) never
    // runs. Replicating jsdom's own click() this way avoids holding an unbound
    // reference to the native prototype method.
    const event = new MouseEvent('click', { bubbles: true, cancelable: true });
    if (this.hasAttribute('download')) {
      event.preventDefault();
    }
    this.dispatchEvent(event);
  };
}

afterEach(() => {
  cleanup();
});
