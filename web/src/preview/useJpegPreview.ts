// The JPEG-poll preview rung (ADR-W023).
//
// An <img> cannot send an Authorization header, so each still is FETCHED with
// the stored bearer and shown via an object URL (revoked on refresh). This is
// the always-available fallback rung of the `<PreviewSurface>` ladder.
//
// Honesty rules:
//   - Only an `image/*` answer is a frame. The SPA index (200 text/html from a
//     proxy fallback) or any other body is "unavailable", never an <img>.
//   - A transient failure (e.g. 503 while the engine has no frame) keeps the
//     last good frame; it is marked `stale` only after STALE_AFTER bounded age.
//   - 404/405/501 or a non-image 200 mean the route is not served at all:
//     the last frame is dropped and state is `unavailable`.
import { useEffect, useRef, useState } from 'react';

import { getStoredToken } from '../api/token';

export type StillState = 'loading' | 'fresh' | 'stale' | 'unavailable';

export interface JpegStill {
  /** Object URL of the last good frame (kept across failed polls). */
  readonly url: string | undefined;
  /** loading: no frame yet; fresh; stale: last frame older than the bound; unavailable: route not served. */
  readonly state: StillState;
}

/** A still older than max(5 polls, 10 s) without a new good frame is stale. */
export function staleAfterMs(refreshMs: number): number {
  return Math.max(refreshMs * 5, 10_000);
}

/** True when a response is a usable image answer. */
export function isImageResponse(resp: { ok: boolean; headers?: Headers | undefined }): boolean {
  return resp.ok && (resp.headers?.get('content-type') ?? '').toLowerCase().startsWith('image/');
}

/**
 * Fetch a preview JPEG with the bearer token and expose it as an object URL,
 * refreshed every `refreshMs`, with an honest freshness state.
 */
export function useJpegStill(path: string, refreshMs: number, enabled = true): JpegStill {
  const [frame, setFrame] = useState<{ path: string; url: string }>();
  const [status, setStatus] = useState<{ path: string; state: StillState }>();
  const current = useRef<string | undefined>(undefined);

  useEffect(() => {
    if (!enabled) return;
    let inFlight = false;
    let controller: AbortController | undefined;
    let staleTimer: number | undefined;

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
        if (isImageResponse(resp)) {
          const blob = await resp.blob();
          const next = URL.createObjectURL(blob);
          if (controller !== requestController) {
            URL.revokeObjectURL(next);
            return;
          }
          revoke();
          current.current = next;
          setFrame({ path, url: next });
          setStatus({ path, state: 'fresh' });
          window.clearTimeout(staleTimer);
          staleTimer = window.setTimeout(() => {
            setStatus((s) => (s?.path === path && s.state === 'fresh' ? { path, state: 'stale' } : s));
          }, staleAfterMs(refreshMs));
          return;
        }
        // 2xx non-image (SPA index) or 404/405/501: the route is not served.
        if (controller !== requestController) return;
        if (resp.ok || resp.status === 404 || resp.status === 405 || resp.status === 501) {
          window.clearTimeout(staleTimer);
          revoke();
          setFrame(undefined);
          setStatus({ path, state: 'unavailable' });
        }
        // Any other failure (503, 5xx, 401…) is transient: keep the last frame.
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
      window.clearTimeout(staleTimer);
      controller?.abort();
      controller = undefined;
      revoke();
    };
  }, [path, refreshMs, enabled]);

  const url = frame?.path === path ? frame.url : undefined;
  const state = status?.path === path ? status.state : 'loading';
  return { url, state };
}

/** Compatibility wrapper: the frame URL only (undefined while loading/unavailable). */
export function useJpegPreview(path: string, refreshMs: number, enabled = true): string | undefined {
  return useJpegStill(path, refreshMs, enabled).url;
}
