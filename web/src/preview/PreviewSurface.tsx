// <PreviewSurface> — the capability-driven WHEP→JPEG fallback ladder
// (ADR-W023 §2).
//
// Mounts render THIS, never <WhepPlayer> directly. It:
//   - probes GET /api/v1/preview/capabilities once (cached); webrtc:false (or
//     the scope absent) ⇒ straight to the JPEG poll, NO badge — JPEG is then
//     the deployment's honest primary, not a degradation;
//   - otherwise plays WHEP, degrading to JPEG on a fatal session error
//     (non-2xx POST incl. 503 capacity, connectionState=failed, or the stats
//     stall watchdog) with a VISIBLE "Fallback — still preview" badge and a
//     "retry live preview" affordance (a fresh WHEP attempt), never a hot loop.
import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { MoqPlayer } from './MoqPlayer';
import { createMoqLeaseApi } from './moqLeaseApi';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { RotateCw, Wifi } from 'lucide-react';

import { Button } from '../components/ui/button';
import { whepAvailable } from './capabilities';
import type { PreviewCapabilities, PreviewScope } from './capabilities';
import { capacityRetryDelayMs } from './capacityBackoff';
import { useJpegPreview } from './useJpegPreview';
import { PreviewDiagnostics } from './PreviewDiagnostics';
import type { PreviewDiagnostics as Diagnostics } from './diagnostics';
import { WhepPlayer } from './WhepPlayer';
import type { PeerConnectionFactory } from './pcFactory';

/** A placeholder shown until the first JPEG frame arrives. */
function JpegImage({
  src,
  label,
  className,
}: {
  readonly src: string | undefined;
  readonly label: string;
  readonly className?: string | undefined;
}): JSX.Element {
  if (src === undefined) {
    return (
      <div
        className={`flex items-center justify-center bg-muted text-xs text-muted-foreground ${className ?? ''}`}
        role="img"
        aria-label={label}
      >
        <Trans>Waiting for a frame…</Trans>
      </div>
    );
  }
  return <img src={src} alt={label} className={className} />;
}

/** The JPEG rung with an optional honest "fallback" badge + retry affordance. */
function JpegRung({
  jpegPath,
  label,
  className,
  degraded,
  capacity = false,
  onRetry,
}: {
  readonly jpegPath: string;
  readonly label: string;
  readonly className?: string | undefined;
  /** True when this rung is a DEGRADATION from WHEP (shows the badge). */
  readonly degraded: boolean;
  /** True when the engine refused live video because another viewer holds the slot. */
  readonly capacity?: boolean;
  readonly onRetry?: (() => void) | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const src = useJpegPreview(jpegPath, 1000);
  return (
    <div className="relative" data-testid="jpeg-rung">
      <JpegImage src={src} label={label} className={className} />
      {degraded ? (
        <div
          className="absolute left-2 top-2 flex max-w-[calc(100%-1rem)] flex-nowrap items-center gap-2 rounded-md bg-background/85 px-2 py-1 text-xs"
          role="status"
          data-testid="fallback-badge"
        >
          <Wifi className="size-3.5 text-muted-foreground" aria-hidden="true" />
          <span data-testid="fallback-reason" className="ps-reason min-w-0 truncate">
            {capacity ? (
              <Trans>Live video in use by another viewer — showing stills</Trans>
            ) : (
              <Trans>Fallback — still preview (~1 fps)</Trans>
            )}
          </span>
          {onRetry !== undefined ? (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-6 shrink-0 gap-1 px-1.5"
              aria-label={t`Retry live preview`}
              onClick={onRetry}
            >
              <RotateCw className="size-3.5" aria-hidden="true" />
              <span className="ps-retry-label">{t`Retry live preview`}</span>
            </Button>
          ) : null}
        </div>
      ) : (
        <p data-testid="still-preview-label" className="px-3 py-2 text-xs text-muted-foreground">
          <Trans>Still preview (~1 fps) — live video is unavailable for this preview.</Trans>
        </p>
      )}
    </div>
  );
}

/**
 * A preview surface for one scope. Picks WHEP vs JPEG from the capabilities
 * document, and degrades WHEP→JPEG (with a badge) on a fatal session error.
 */
/** The transport rung a surface is using. The ladder is MoQ, then WebRTC, then stills. */
export type PreviewTransport = 'moq' | 'webrtc' | 'jpeg';

/**
 * After an unacknowledged MoQ release the server may hold the lease until its
 * deadline (the server clamps a lease to 60 s). The surface shows stills for
 * this long before it opens WebRTC, so two live sessions never overlap.
 */
export const MOQ_UNACKNOWLEDGED_HOLD_MS = 60_000;

export function PreviewSurface({
  scope,
  whepEndpoint,
  jpegPath,
  label,
  capabilities,
  audio = false,
  className,
  pcFactory,
  rtcConfig,
  fetchImpl,
  onSessionReleased,
  onLeaseState,
  onTransport,
  diagnosticsHost,
  onTransportUnavailable,
}: {
  readonly scope: PreviewScope;
  /** The WHEP endpoint for this scope (e.g. `/api/v1/preview/program/whep`). */
  readonly whepEndpoint: string;
  /** The JPEG fallback path for this scope. */
  readonly jpegPath: string;
  readonly label: string;
  /** The probed capabilities (undefined while loading / on a probe failure). */
  readonly capabilities: PreviewCapabilities | undefined;
  readonly audio?: boolean;
  readonly className?: string | undefined;
  readonly pcFactory?: PeerConnectionFactory | undefined;
  readonly rtcConfig?: RTCConfiguration | undefined;
  readonly fetchImpl?: typeof fetch | undefined;
  readonly onSessionReleased?: (() => void) | undefined;
  /** Reports an unavailable MoQ path so a parent can label the resulting JPEG honestly. */
  readonly onTransportUnavailable?: (() => void) | undefined;
  readonly onLeaseState?: ((state: 'negotiating' | 'active' | 'released') => void) | undefined;
  /** Reports the rung in use (`moq`, `webrtc` or `jpeg`) and, after a MoQ fallback, why MoQ is not in use. */
  readonly onTransport?: ((transport: PreviewTransport, moqFallbackReason?: string) => void) | undefined;
  /** When set, the fallback diagnostics + busy note render into this element (below the picture) behind one disclosure. */
  readonly diagnosticsHost?: HTMLElement | null | undefined;
}): JSX.Element {
  // A monotonically-increasing attempt id: bumping it remounts <WhepPlayer> for
  // a fresh negotiation (the "retry live preview" affordance). `degraded` flips
  // to JPEG-with-badge on a fatal error and clears on a retry.
  const [attempt, setAttempt] = useState(0);
  const [degraded, setDegraded] = useState(false);
  const [diagnostics, setDiagnostics] = useState<Diagnostics>();
  const [busy, setBusy] = useState(false);
  const [capacity, setCapacity] = useState(false);
  const refusals = useRef(0);
  const [released, setReleased] = useState(true);

  // Why MoQ is not in use for THIS scope and endpoint (undefined = not tried or
  // playing). The record carries its key, so a surface that is reused for a
  // different source starts at MoQ again and never reports a stale reason.
  const surfaceKey = `${scope}:${whepEndpoint}`;
  const [moqFallback, setMoqFallback] = useState<{ readonly key: string; readonly reason: string; readonly held: boolean }>();
  const moqReason = moqFallback?.key === surfaceKey ? moqFallback.reason : undefined;
  const moqHeld = moqFallback?.key === surfaceKey && moqFallback.held;
  const leaseRef = useRef({ onLeaseState, onSessionReleased });
  useEffect(() => { leaseRef.current = { onLeaseState, onSessionReleased }; });
  useEffect(() => {
    if (!moqHeld) return;
    const timer = window.setTimeout(() => {
      setMoqFallback((current) => (current?.key === surfaceKey ? { ...current, held: false } : current));
      leaseRef.current.onLeaseState?.('released');
      leaseRef.current.onSessionReleased?.();
    }, MOQ_UNACKNOWLEDGED_HOLD_MS);
    return (): void => { window.clearTimeout(timer); };
  }, [moqHeld, surfaceKey]);
  const moqApi = useMemo(() => createMoqLeaseApi(scope, whepEndpoint), [scope, whepEndpoint]);
  const canWhep = whepAvailable(capabilities, scope);
  const retryNow = (): void => {
    setBusy(false);
    setCapacity(false);
    setDegraded(false);
    setAttempt((n) => n + 1);
  };
  const retryRef = useRef(retryNow);
  useEffect(() => { retryRef.current = retryNow; });

  // After a `whep-capacity` refusal retry on a bounded backoff, once our own
  // previous session is released, and never while the tab is hidden.
  const autoRetry = degraded && capacity && released;
  useEffect(() => {
    if (!autoRetry) return;
    let timer: number | undefined;
    const arm = (): void => {
      timer = window.setTimeout(() => {
        if (document.visibilityState === 'hidden') return; // resume on visibilitychange
        retryRef.current();
      }, capacityRetryDelayMs(Math.max(refusals.current - 1, 0)));
    };
    const onVisible = (): void => {
      if (document.visibilityState === 'visible' && timer !== undefined) {
        window.clearTimeout(timer);
        retryRef.current();
      }
    };
    arm();
    document.addEventListener('visibilitychange', onVisible);
    return (): void => {
      window.clearTimeout(timer);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [autoRetry, attempt]);

  const transport: PreviewTransport = capabilities?.moq && moqReason === undefined ? 'moq'
    : canWhep && !degraded && !moqHeld ? 'webrtc' : 'jpeg';
  const transportRef = useRef(onTransport);
  useEffect(() => { transportRef.current = onTransport; });
  useEffect(() => { transportRef.current?.(transport, moqReason); }, [transport, moqReason]);

  if (capabilities?.moq && moqReason === undefined) {
    return <MoqPlayer key={surfaceKey} api={moqApi} label={label} audio={audio} className={className}
      onUnavailable={(reason) => {
        setMoqFallback({ key: surfaceKey, reason, held: reason === 'release-unacknowledged' });
        onTransportUnavailable?.();
      }}
      onLeaseState={(state) => { onLeaseState?.(state); if (state === 'released') onSessionReleased?.(); }} />;
  }
  // The user sees why MoQ is not in use. Two reasons are the normal state and
  // get no note: a browser without MoQ support, and a server that does not
  // offer MoQ for this scope.
  const moqNote = moqReason === undefined || moqReason === 'unsupported' || moqReason === 'moq-unavailable' ? null : (
    <p role="status" className="px-3 py-1 text-xs text-muted-foreground" data-testid="moq-fallback-note" data-reason={moqReason}>
      {moqReason === 'forbidden'
        ? <Trans>MoQ video was refused: this account has no access to it. Showing the next available preview.</Trans>
        : <Trans>MoQ video is not in use ({moqReason}). Showing the next available preview.</Trans>}
    </p>
  );
  const withNote = (rung: JSX.Element): JSX.Element => {
    if (moqNote === null) return rung;
    return <>{rung}{diagnosticsHost ? createPortal(moqNote, diagnosticsHost) : moqNote}</>;
  };
  // No WebRTC for this scope/build: JPEG is the honest primary — NO badge.
  // This applies to every scope, including `input`: a build with no WebRTC at
  // all (the LGPL-clean/GPU-free default, `webrtc: false`) must still offer
  // the always-available ~1 fps still preview, not an "unavailable" dead end
  // (capabilities.ts's `whepAvailable` doc, and the module header above, both
  // say this is the deployment's honest primary — not a degradation). Only
  // the `degraded` branch below (a WHEP attempt that was actually made and
  // failed/was busy) keeps the explicit "Live input preview failed" state for
  // `input`, per the real WHEP-failure case this distinction exists for.
  if (!canWhep) {
    return withNote(<JpegRung jpegPath={jpegPath} label={label} degraded={false} className={className} />);
  }
  // The MoQ lease may still be held: stills only, until the hold ends.
  if (moqHeld) {
    return withNote(<JpegRung jpegPath={jpegPath} label={label} degraded className={className} />);
  }

  // WHEP was attempted and failed: degrade to JPEG with the honest badge + a
  // retry that re-arms a fresh WHEP attempt.
  if (degraded) {
    if (scope === 'input') {
      return withNote(
        <>
          <JpegRung jpegPath={jpegPath} label={label} degraded={false} className={className} />
          <div role="alert" className="px-3 py-2 text-sm" data-testid="input-preview-unavailable">{busy ? <Trans>Live input preview is busy. Another viewer has the session.</Trans> : <Trans>Live input preview failed.</Trans>}{released ? <Button type="button" onClick={retryNow}>Retry live preview</Button> : null}{diagnostics ? <PreviewDiagnostics diagnostics={diagnostics} /> : null}</div>
        </>,
      );
    }
    const busyNote = busy && !capacity ? <p role="alert"><Trans>Live preview is busy. Another viewer has the session.</Trans></p> : null;
    const details = diagnostics ? <PreviewDiagnostics diagnostics={diagnostics} flat={diagnosticsHost != null} /> : null;
    return withNote(
      <>
        <JpegRung
          jpegPath={jpegPath}
          label={label}
          degraded
          capacity={capacity}
          className={className}
          {...(released ? { onRetry: retryNow } : {})}
        />
        {diagnosticsHost ? (
          createPortal(
            <>
              {busyNote}
              {details ? (
                <details className="ps-diagnostics" data-testid="fallback-diagnostics">
                  <summary><Trans>Why live preview failed</Trans></summary>
                  {details}
                </details>
              ) : null}
            </>,
            diagnosticsHost,
          )
        ) : (
          <>{busyNote}{details}</>
        )}
      </>,
    );
  }

  return withNote(
    <WhepPlayer
      key={attempt}
      endpoint={whepEndpoint}
      label={label}
      audio={audio}
      className={className}
      {...(pcFactory !== undefined ? { pcFactory } : {})}
      {...(rtcConfig !== undefined ? { rtcConfig } : {})}
      {...(fetchImpl !== undefined ? { fetchImpl } : {})}
      onSessionReleased={onSessionReleased}
      onLeaseState={(state): void => {
        setReleased(state === 'released');
        onLeaseState?.(state);
      }}
      onStatus={(status): void => { if (status === 'playing') refusals.current = 0; }}
      onBusy={(isCapacity): void => {
        setBusy(true);
        if (isCapacity) { refusals.current += 1; setCapacity(true); }
      }}
      onFatal={(details): void => {
        setDiagnostics(details);
        setDegraded(true);
      }}
    />,
  );
}
