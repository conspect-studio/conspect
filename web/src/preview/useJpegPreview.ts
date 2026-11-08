// The JPEG-poll preview rung (ADR-W023), extracted unchanged from
// MonitoringPage's local `usePreviewUrl`.
//
// An <img> cannot send an Authorization header, so each still is FETCHED with
// the stored bearer and shown via an object URL (revoked on refresh). This is
// the always-available fallback rung of the `<PreviewSurface>` ladder and the
// primary path on a build with no WebRTC. Best-effort: a failed fetch keeps the
// last frame and never throws (engine isolation, inv #10).
import { useEffect, useRef, useState } from 'react';

import { getStoredToken } from '../api/token';

/**
 * Fetch a preview JPEG with the bearer token and expose it as an object URL,
 * refreshed every `refreshMs`. Returns `undefined` until a frame arrives (the
 * endpoint answers 503 when the engine has produced none yet).
 */
export function useJpegPreview(path: string, refreshMs: number, enabled = true): string | undefined {
  const [frame, setFrame] = useState<{ path: string; url: string }>();
  const current = useRef<string | undefined>(undefined);

  useEffect(() => {
    if (!enabled) return;
    let inFlight = false;
    let controller: AbortController | undefined;

    const revoke = (): void => {
      if (current.current !== undefined) {
        URL.revokeObjectURL(current.current);
        current.current = undefined;
      }
    };

    const tick = async (): Promise<void> => {
      if (inFlight || document.visibilityState === 'hidden') {
        return;
      }
      inFlight = true;
      const requestController = new AbortController();
      controller = requestController;
      const headers: Record<string, string> = {};
      const token = getStoredToken();
      if (token !== undefined) {
        headers.Authorization = `Bearer ${token}`;
      }
      try {
        const resp = await fetch(path, {
          headers,
          cache: 'no-store',
          signal: requestController.signal,
        });
        if (!resp.ok) {
          return;
        }
        const blob = await resp.blob();
        const next = URL.createObjectURL(blob);
        if (controller !== requestController) {
          URL.revokeObjectURL(next);
          return;
        }
        revoke();
        current.current = next;
        setFrame({ path, url: next });
      } catch {
        // Best-effort preview; a failed fetch just keeps the last frame.
      } finally {
        if (controller === requestController) {
          controller = undefined;
          inFlight = false;
        }
      }
    };

    void tick();
    const handle = window.setInterval(() => void tick(), refreshMs);
    return (): void => {
      window.clearInterval(handle);
      controller?.abort();
      controller = undefined;
      revoke();
    };
  }, [path, refreshMs, enabled]);

  return frame?.path === path ? frame.url : undefined;
}
