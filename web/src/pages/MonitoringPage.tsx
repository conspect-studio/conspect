// Monitoring — the live preview wall.
//
// The PROGRAM card and the focus dialogs use <PreviewSurface>, the
// capability-driven WHEP→JPEG ladder (ADR-W023): on a webrtc-native build with
// the program scope advertised, the program plays sub-second WHEP with an
// honest fidelity label; otherwise the ~1 fps JPEG poll is the primary path (no
// "degraded" badge — it is the deployment's honest best). Input thumbnails stay
// JPEG (cheap, many); clicking one opens a dialog with the live WHEP player for
// that input. A new outputs section previews served `webrtc` outputs. Alongside
// the pixels, the real per-tile lifecycle state streams over the WebSocket
// (`useEngineEvents`). All reads are best-effort and never block the engine
// (invariant #10).
import { useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { HlsPlayer } from '../preview/HlsPlayer';
import { hlsPlaybackPaths } from '../preview/outputPlayback';
import { whepAvailable } from '../preview/capabilities';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQuery } from '@tanstack/react-query';
import { Maximize2 } from 'lucide-react';

import { getStoredToken } from '../api/token';
import { ConnectionStatus } from '../components/ConnectionStatus';
import { HelpLink } from '../components/HelpLink';
import { PageHeader } from '../components/PageHeader';
import { TileStateBadge } from '../components/TileStateBadge';
import { Button } from '../components/ui/button';
import { Card, CardContent } from '../components/ui/card';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { PreviewSurface } from '../preview/PreviewSurface';
import { programFidelity, usePreviewCapabilities } from '../preview/capabilities';
import type { ProgramFidelity } from '../preview/capabilities';
import { useJpegPreview } from '../preview/useJpegPreview';
import { useOutputs } from '../resources/queries';
import { useLiveTiles } from '../resources/useLiveTiles';
import { useEngineEvents } from '../realtime/useEngineEvents';
import type { LiveTile } from '../realtime/useEngineEvents';

/** The ids of inputs that can be previewed (GET /api/v1/preview/inputs). */
function usePreviewInputIds(): readonly string[] {
  const query = useQuery<readonly string[]>({
    queryKey: ['preview', 'inputs'],
    queryFn: async (): Promise<readonly string[]> => {
      const headers: Record<string, string> = {};
      const token = getStoredToken();
      if (token !== undefined) {
        headers.Authorization = `Bearer ${token}`;
      }
      const resp = await fetch('/api/v1/preview/inputs', { headers });
      if (!resp.ok) {
        throw new Error(`Preview input query failed (${String(resp.status)})`);
      }
      const data: unknown = await resp.json();
      return Array.isArray(data) ? data.filter((v): v is string => typeof v === 'string') : [];
    },
    refetchInterval: 5000,
  });
  return query.data ?? [];
}

/** A single live preview image with a placeholder until the first frame. */
function PreviewImage(props: {
  readonly src: string | undefined;
  readonly alt: string;
  readonly className?: string;
}): JSX.Element {
  if (props.src === undefined) {
    return (
      <div
        className={`flex items-center justify-center bg-muted text-xs text-muted-foreground ${props.className ?? ''}`}
        role="img"
        aria-label={props.alt}
      >
        <Trans>Waiting for a frame…</Trans>
      </div>
    );
  }
  return <img src={props.src} alt={props.alt} className={props.className} />;
}

/** Show the current program as a still image while its WHEP focus is released. */
function ProgramStillPreview({ label }: { readonly label: string }): JSX.Element {
  const src = useJpegPreview('/api/v1/preview/program.jpg', 1000);
  return <PreviewImage src={src} alt={label} className="aspect-video w-full bg-black object-contain" />;
}

/** The honest program fidelity label (ADR-P006), shown when WHEP is live. */
function FidelityLabel({ fidelity }: { readonly fidelity: ProgramFidelity }): JSX.Element {
  switch (fidelity) {
    case 'real-encoded-output':
      return (
        <span className="text-xs text-muted-foreground" data-testid="program-fidelity">
          <Trans>Exact output rendition</Trans>
        </span>
      );
    case 'pre-encode-canvas-approx':
      return (
        <span className="text-xs text-muted-foreground" data-testid="program-fidelity">
          <Trans>Pre-encode canvas approximation</Trans>
        </span>
      );
  }
}

/** The monitoring page. */
export function MonitoringPage(): JSX.Element {
  const { t } = useLingui();
  const { status } = useEngineEvents();
  const capabilities = usePreviewCapabilities();
  const inputIds = usePreviewInputIds();
  const tiles = useLiveTiles();
  const outputs = useOutputs();
  const fidelity = programFidelity(capabilities.data);
  const playbackPaths = hlsPlaybackPaths(outputs.data ?? []);
  const [hlsOutput, setHlsOutput] = useState<string>('');
  // One WHEP focus is admitted server-wide. Release the automatic program
  // focus before a user-requested input focus negotiates its session.
  const [focusedInput, setFocusedInput] = useState<string | undefined>();
  const [activeInput, setActiveInput] = useState<string | undefined>();
  const [programLease, setProgramLease] = useState<'negotiating' | 'active' | 'released'>('released');
  const [inputLease, setInputLease] = useState<'negotiating' | 'active' | 'released'>('released');
  const desiredInput = useRef<string | undefined>(undefined);
  const hlsPath = playbackPaths.get(hlsOutput);
  const inputWhep = whepAvailable(capabilities.data, 'input');
  const paused = focusedInput !== undefined || activeInput !== undefined;
  // The mounted player (not the capability advertisement) owns the lease.
  // Its release callback fires only after any in-flight POST and DELETE settle.
  const readyInput = focusedInput !== undefined && activeInput === focusedInput
    ? focusedInput
    : focusedInput !== undefined && activeInput === undefined && programLease === 'released' && inputLease === 'released'
      ? focusedInput : undefined;
  const changeFocus = (id: string, open: boolean): void => {
    const next = open ? id : undefined;
    desiredInput.current = next;
    setFocusedInput(next);
    if (!inputWhep) {
      setActiveInput(next);
    } else if (activeInput !== undefined && next !== activeInput && inputLease === 'released') {
      setActiveInput(undefined);
    }
  };

  // The served WebRTC (WHEP) outputs — previewable via their OUTPUT scope tap.
  const webrtcOutputs = (outputs.data ?? []).filter((output) => output.kind === 'webrtc');

  return (
    <>
      <PageHeader
        title={<Trans>Monitoring</Trans>}
        description={
          <Trans>
            Live program output and per-input previews, with each tile&apos;s
            lifecycle state from the engine stream.
          </Trans>
        }
        actions={<ConnectionStatus status={status} />}
      />

      <section aria-labelledby="program-heading">
        <div className="mb-3 flex items-center justify-between gap-2">
          <h2 id="program-heading" className="text-lg font-semibold">
            <Trans>Program</Trans>
          </h2>
          {hlsPath === undefined && fidelity !== undefined && whepAvailable(capabilities.data, 'program') ? <FidelityLabel fidelity={fidelity} /> : null}
        </div>
        <Card className="overflow-hidden">
          {hlsPath !== undefined ? <HlsPlayer key={hlsPath} url={hlsPath} label={t`Program HLS output`} /> : paused ? <ProgramStillPreview label={t`Program preview paused while input focus is open`} /> : <PreviewSurface
            scope="program"
            whepEndpoint="/api/v1/preview/program/whep"
            jpegPath="/api/v1/preview/program.jpg"
            label={t`Live program output`}
            capabilities={capabilities.data}
            audio
            className="aspect-video w-full bg-black object-contain"
            onLeaseState={setProgramLease}
            onSessionReleased={(): void => { setProgramLease('released'); setActiveInput(desiredInput.current); }}
          />}
          <CardContent className="space-y-2 py-3">
            <p className="text-sm text-muted-foreground"><Trans>This is the current engine program. Saved and unsaved layouts do not have separate live outputs.</Trans></p>
            {playbackPaths.size > 0 ? <label className="flex flex-wrap items-center gap-2 text-sm">
              <Trans>Playback transport</Trans>
              <select className="rounded-md border bg-background p-2" value={hlsOutput} onChange={(event) => { setHlsOutput(event.target.value); }}>
                <option value="">{whepAvailable(capabilities.data, 'program') ? t`WebRTC preview (low latency)` : t`Still preview (~1 fps)`}</option>
                {(outputs.data ?? []).filter((output) => playbackPaths.has(output.id)).map((output) => <option key={output.id} value={output.id}>{t`HLS video (buffered)`}: {output.name}</option>)}
              </select>
            </label> : null}
            <div className="flex flex-wrap gap-3 text-sm underline"><Link to="/layouts"><Trans>Manage layouts</Trans></Link><Link to="/sources"><Trans>Manage sources</Trans></Link><Link to="/outputs"><Trans>Manage outputs</Trans></Link></div>
          </CardContent>
        </Card>
      </section>

      <section aria-labelledby="inputs-heading" className="mt-8">
        <h2 id="inputs-heading" className="mb-3 flex items-center gap-2 text-lg font-semibold">
          <Trans>Inputs — still thumbnails (~1 fps)</Trans>
          <HelpLink
            to="/help/concepts/resilience#tile-lifecycle"
            label={t`About tile lifecycle states`}
            compact
          />
        </h2>
        {inputIds.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            <Trans>No inputs are available to preview.</Trans>
          </p>
        ) : (
          <ul className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
            {inputIds.map((id) => (
              <InputThumbnail
                key={id}
                id={id}
                tile={tiles.get(id)}
                capabilities={capabilities.data}
                open={focusedInput === id}
                focusReady={readyInput === id}
                onLeaseState={setInputLease}
                onSessionReleased={(): void => {
                  setInputLease('released');
                  setActiveInput(desiredInput.current);
                }}
                onOpenChange={(open): void => { changeFocus(id, open); }}
              />
            ))}
          </ul>
        )}
      </section>

      {webrtcOutputs.length > 0 ? (
        <section aria-labelledby="outputs-heading" className="mt-8">
          <h2 id="outputs-heading" className="mb-3 flex items-center gap-2 text-lg font-semibold">
            <Trans>WebRTC outputs</Trans>
            <HelpLink to="/help/concepts/glossary#whep" label={t`What is WHEP?`} compact />
          </h2>
          <ul className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {webrtcOutputs.map((output) => (
              <li key={output.id}>
                <Card className="overflow-hidden">
                  <ProgramStillPreview label={t`Preview of WebRTC output ${output.name}`} />
                  <CardContent className="py-2">
                    <code className="truncate text-xs" lang="" dir="auto">
                      {output.name}
                    </code>
                  </CardContent>
                </Card>
              </li>
            ))}
          </ul>
        </section>
      ) : null}
    </>
  );
}

/** One input's live thumbnail (JPEG) + lifecycle badge + a focus dialog. */
function InputThumbnail(props: {
  readonly id: string;
  readonly tile: LiveTile | undefined;
  readonly capabilities: ReturnType<typeof usePreviewCapabilities>['data'];
  readonly open: boolean;
  readonly focusReady: boolean;
  readonly onSessionReleased: () => void;
  readonly onLeaseState: (state: 'negotiating' | 'active' | 'released') => void;
  readonly onOpenChange: (open: boolean) => void;
}): JSX.Element {
  const { t } = useLingui();
  const src = useJpegPreview(`/api/v1/preview/inputs/${encodeURIComponent(props.id)}.jpg`, 1000, !props.open);
  return (
    <li>
      <Card className="overflow-hidden">
        <div className="relative">
          <PreviewImage
            src={src}
            alt={t`Preview of input ${props.id}`}
            className="aspect-video w-full bg-black object-contain"
          />
          <Button
            type="button"
            variant="secondary"
            size="icon"
            className="absolute right-2 top-2"
            aria-label={t`Open preview of input ${props.id}`}
            onClick={(): void => {
              props.onOpenChange(true);
            }}
          >
            <Maximize2 className="size-4" aria-hidden="true" />
          </Button>
        </div>
        <CardContent className="flex items-center justify-between gap-2 py-2">
          <code className="truncate text-xs" lang="" dir="auto">
            {props.id}
          </code>
          {props.tile !== undefined ? <TileStateBadge state={props.tile.state} /> : null}
        </CardContent>
      </Card>

      <Dialog open={props.open} onOpenChange={props.onOpenChange}>
        <DialogContent className="max-w-3xl">
          <DialogHeader>
            <DialogTitle>
              <Trans>Preview — input {props.id}</Trans>
            </DialogTitle>
          </DialogHeader>
          {props.open && props.focusReady ? (
            <PreviewSurface
              scope="input"
              whepEndpoint={`/api/v1/preview/inputs/${encodeURIComponent(props.id)}/whep`}
              jpegPath={`/api/v1/preview/inputs/${encodeURIComponent(props.id)}.jpg`}
              label={t`Live preview of input ${props.id}`}
              capabilities={props.capabilities}
              onSessionReleased={props.onSessionReleased}
              onLeaseState={props.onLeaseState}
              className="aspect-video w-full bg-black object-contain"
            />
          ) : null}
        </DialogContent>
      </Dialog>
    </li>
  );
}
