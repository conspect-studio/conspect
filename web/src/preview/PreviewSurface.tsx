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
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { RotateCw, Wifi } from 'lucide-react';

import { Button } from '../components/ui/button';
import { whepAvailable } from './capabilities';
import type { PreviewCapabilities, PreviewScope } from './capabilities';
import { useJpegPreview } from './useJpegPreview';
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
  onRetry,
}: {
  readonly jpegPath: string;
  readonly label: string;
  readonly className?: string | undefined;
  /** True when this rung is a DEGRADATION from WHEP (shows the badge). */
  readonly degraded: boolean;
  readonly onRetry?: (() => void) | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const src = useJpegPreview(jpegPath, 1000);
  return (
    <div className="relative" data-testid="jpeg-rung">
      <JpegImage src={src} label={label} className={className} />
      {degraded ? (
        <div
          className="absolute left-2 top-2 flex items-center gap-2 rounded-md bg-background/85 px-2 py-1 text-xs"
          role="status"
          data-testid="fallback-badge"
        >
          <Wifi className="size-3.5 text-muted-foreground" aria-hidden="true" />
          <span>
            <Trans>Fallback — still preview (~1 fps)</Trans>
          </span>
          {onRetry !== undefined ? (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-6 gap-1 px-1.5"
              onClick={onRetry}
            >
              <RotateCw className="size-3.5" aria-hidden="true" />
              <span>{t`Retry live preview`}</span>
            </Button>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

/**
 * A preview surface for one scope. Picks WHEP vs JPEG from the capabilities
 * document, and degrades WHEP→JPEG (with a badge) on a fatal session error.
 */
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
}): JSX.Element {
  // A monotonically-increasing attempt id: bumping it remounts <WhepPlayer> for
  // a fresh negotiation (the "retry live preview" affordance). `degraded` flips
  // to JPEG-with-badge on a fatal error and clears on a retry.
  const [attempt, setAttempt] = useState(0);
  const [degraded, setDegraded] = useState(false);

  const canWhep = whepAvailable(capabilities, scope);

  // No WebRTC for this scope/build: JPEG is the honest primary — NO badge.
  if (!canWhep) {
    return <JpegRung jpegPath={jpegPath} label={label} degraded={false} className={className} />;
  }

  // WHEP was attempted and failed: degrade to JPEG with the honest badge + a
  // retry that re-arms a fresh WHEP attempt.
  if (degraded) {
    return (
      <JpegRung
        jpegPath={jpegPath}
        label={label}
        degraded
        className={className}
        onRetry={(): void => {
          setDegraded(false);
          setAttempt((n) => n + 1);
        }}
      />
    );
  }

  return (
    <WhepPlayer
      key={attempt}
      endpoint={whepEndpoint}
      label={label}
      audio={audio}
      className={className}
      {...(pcFactory !== undefined ? { pcFactory } : {})}
      {...(rtcConfig !== undefined ? { rtcConfig } : {})}
      {...(fetchImpl !== undefined ? { fetchImpl } : {})}
      onFatal={(): void => {
        setDegraded(true);
      }}
    />
  );
}
