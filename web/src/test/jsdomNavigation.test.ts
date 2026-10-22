// Guards the jsdom programmatic-anchor navigation shim wired in `setup.ts`.
//
// Our file-download helpers (downloadConfigExport, the DataPage/LicencePage
// snapshot downloads) trigger a programmatic `<a download>.click()`. jsdom does
// not honour the `download` attribute, so it treats the click as a real
// navigation and queues a deferred `navigate()` on a timer. That timer cannot
// complete ("Not implemented: navigation") and, when it fires while a worker is
// being torn down under host load, aborts the Node process with a libuv
// `uv__stream_destroy` assertion — surfacing in CI as a flaky "Worker exited
// unexpectedly" failure. The shim cancels the navigating default action of a
// programmatic anchor click, so no navigation is ever scheduled.
import { afterEach, describe, expect, it, vi } from 'vitest';

describe('jsdom programmatic download-anchor navigation shim (CI worker-abort guard)', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('schedules no deferred jsdom navigation for a programmatic <a download> click', () => {
    const anchor = document.createElement('a');
    anchor.href = 'https://example.invalid/snapshot.bin';
    anchor.download = 'snapshot.bin';
    document.body.append(anchor);

    let clicked = false;
    anchor.addEventListener('click', () => {
      clicked = true;
    });

    vi.useFakeTimers();
    const timersBefore = vi.getTimerCount();
    anchor.click();
    const timersAfter = vi.getTimerCount();
    anchor.remove();

    // The click still dispatches, so download handlers/observers still see it.
    expect(clicked).toBe(true);
    // ...but no deferred navigation timer is queued. Without the shim the
    // programmatic click schedules a navigate() that aborts the worker when it
    // fires during teardown (libuv `uv__stream_destroy`).
    expect(timersAfter).toBe(timersBefore);
  });
});
