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
export function useJpegPreview(path: string, refreshMs: number): string | undefined {
  const [url, setUrl] = useState<string | undefined>(undefined);
  const current = useRef<string | undefined>(undefined);

  useEffect(() => {
    let cancelled = false;

    const revoke = (): void => {
      if (current.current !== undefined) {
        URL.revokeObjectURL(current.current);
        current.current = undefined;
      }
    };

    const tick = async (): Promise<void> => {
      const headers: Record<string, string> = {};
      const token = getStoredToken();
      if (token !== undefined) {
        headers.Authorization = `Bearer ${token}`;
      }
      try {
        const resp = await fetch(path, { headers, cache: 'no-store' });
        if (!resp.ok) {
          return;
        }
        const blob = await resp.blob();
        if (cancelled) {
          return;
        }
        const next = URL.createObjectURL(blob);
        revoke();
        current.current = next;
        setUrl(next);
      } catch {
        // Best-effort preview; a failed fetch just keeps the last frame.
      }
    };

    void tick();
    const handle = window.setInterval(() => void tick(), refreshMs);
    return (): void => {
      cancelled = true;
      window.clearInterval(handle);
      revoke();
    };
  }, [path, refreshMs]);

  return url;
}
