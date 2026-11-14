// "Fill from web…" — the Windy VT-fill dialog (one bounded UI slice on top of
// PR 286's artifactRef player loads).
//
// Flow: capture capabilities gate the dialog honestly (an unprovisioned
// server shows a clear disabled message instead of a doomed submit); the
// operator picks overlay/location/zoom/size/frames/then and MUST tick the
// Windy terms checkbox; submit POSTs one bounded `CaptureBody`
// (`POST /api/v1/media/captures`) and polls its `JobView` every ~2s
// (`useCaptureJob`) until terminal; on `succeeded` it POSTs the artifact load
// (`POST /api/v1/media/players/{id}/load`, body `{ artifactRef, then }`) with
// a fresh `Idempotency-Key` per ATTEMPT (reused on a retry of that same
// attempt, never on a brand-new fill) and polls the native load operation
// (`useMediaPlayerLoadOperation`) to its terminal state, showing the native
// ack (duration/geometry/audio) once committed.
//
// `fill_id` is `vt-<playerId>-windy`: authorize_object (auth.rs) allow-lists
// by exact object id per scoped principal, and the unscoped (default)
// principal permits any id — a stable, player-derived id that an operator
// could also explicitly allow-list per player, same as any other object id.
//
// LOOP (capture mode / timing): optional `View` fields (`capture:
// 'steps'|'realtime'`, `timing: 'equal'|'proportional'`, `frame_ms`,
// `end_hold_ms`, `max_steps`). Radar defaults to steps-mode (a short loop of
// the most recent radar steps); Wind only supports realtime (the server
// refuses `steps` capture combined with the wind overlay) so the UI hides
// and forces realtime for Wind. In steps mode `frames` is ignored
// server-side, so the frames field is hidden; the nominal total frame count
// (`max_steps * round(frame_ms * fps / 1000) + round(end_hold_ms * fps /
// 1000)`) must stay <= 600 (the server's hard cap), validated client-side
// below.
import { useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { CheckCircle2, Loader2, XCircle } from 'lucide-react';

import type { MediaPlayer } from '../api/media-playersQueries';
import { useLoadMediaPlayerArtifact, useMediaPlayerLoadOperation } from '../api/media-playersQueries';
import { newIdempotencyKey } from '../api/operations';
import {
  useCancelCapture,
  useCaptureCapabilities,
  useCaptureJob,
  useCreateCapture,
} from '../api/web-captureQueries';
import type { CaptureBody, CaptureView } from '../api/web-captureQueries';
import { WINDY_TERMS_VERSION, windyFillId } from '../api/web-capture';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../components/ui/select';

/** `View.capture` — steps-mode (radar) or realtime (wind, or operator choice). */
type CaptureMode = NonNullable<CaptureView['capture']>;

/** `View.timing` — steps-mode per-step duration rule. */
type StepTiming = NonNullable<CaptureView['timing']>;

/** The server's hard cap on nominal total frames for any one capture. */
const MAX_NOMINAL_FRAMES = 600;

/** Default steps-mode per-step duration, ms (`View.frame_ms`). */
const DEFAULT_FRAME_MS = 600;

/** Default steps-mode newest-step hold, ms (`View.end_hold_ms`). */
const DEFAULT_END_HOLD_MS = 1000;

/** Default steps-mode step count (`View.max_steps`). */
const DEFAULT_MAX_STEPS = 12;

/**
 * The nominal total frame count the server computes for a steps-mode
 * capture: `max_steps * round(frame_ms * fps / 1000) + round(end_hold_ms *
 * fps / 1000)`. Mirrors the server-side cap check so the client can refuse
 * a doomed submit before it round-trips.
 */
function nominalStepsFrames(fps: number, frameMs: number, endHoldMs: number, maxSteps: number): number {
  const perStep = Math.round((frameMs * fps) / 1000);
  const hold = Math.round((endHoldMs * fps) / 1000);
  return maxSteps * perStep + hold;
}

/** A named preset location (lat/lon) offered as a shortcut. */
interface LocationPreset {
  readonly label: string;
  readonly lat: number;
  readonly lon: number;
}

const LOCATION_PRESETS: readonly LocationPreset[] = [
  { label: 'Sydney', lat: -33.87, lon: 151.21 },
  { label: 'London', lat: 51.51, lon: -0.13 },
  { label: 'New York', lat: 40.71, lon: -74.01 },
  { label: 'Tokyo', lat: 35.68, lon: 139.69 },
];

/** A width×height preset. Windy requires `width >= 1024` (desktop geometry). */
interface SizePreset {
  readonly label: string;
  readonly width: number;
  readonly height: number;
}

const SIZE_PRESETS: readonly SizePreset[] = [
  { label: '1920×1080', width: 1920, height: 1080 },
  { label: '1280×720', width: 1280, height: 720 },
];

/** Persisted per-player draft so a re-capture is one click. */
interface FillDraft {
  readonly overlay: 'radar' | 'wind';
  readonly lat: number;
  readonly lon: number;
  readonly zoom: number;
  readonly width: number;
  readonly height: number;
  readonly fps: number;
  readonly frames: number;
  readonly then: 'cue' | 'resume';
  readonly premium: boolean;
  /** Steps (radar default) or realtime (forced for wind). */
  readonly capture: CaptureMode;
  readonly timing: StepTiming;
  readonly frameMs: number;
  readonly endHoldMs: number;
  readonly maxSteps: number;
}

const DEFAULT_DRAFT: FillDraft = {
  overlay: 'radar',
  lat: -33.87,
  lon: 151.21,
  zoom: 10,
  width: 1920,
  height: 1080,
  fps: 10,
  frames: 60,
  then: 'resume',
  premium: false,
  capture: 'steps',
  timing: 'equal',
  frameMs: DEFAULT_FRAME_MS,
  endHoldMs: DEFAULT_END_HOLD_MS,
  maxSteps: DEFAULT_MAX_STEPS,
};

function draftStorageKey(playerId: string): string {
  return `conspect.vtfill.${playerId}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** The base (pre-loop) draft fields; a draft saved before the loop fields landed still matches this. */
type BaseFillDraft = Pick<
  FillDraft,
  'overlay' | 'lat' | 'lon' | 'zoom' | 'width' | 'height' | 'fps' | 'frames' | 'then' | 'premium'
>;

function isBaseFillDraft(value: unknown): value is BaseFillDraft {
  if (!isRecord(value)) return false;
  const v = value;
  return (
    (v.overlay === 'radar' || v.overlay === 'wind') &&
    typeof v.lat === 'number' &&
    typeof v.lon === 'number' &&
    typeof v.zoom === 'number' &&
    typeof v.width === 'number' &&
    typeof v.height === 'number' &&
    typeof v.fps === 'number' &&
    typeof v.frames === 'number' &&
    (v.then === 'cue' || v.then === 'resume') &&
    typeof v.premium === 'boolean'
  );
}

/** Fills in the loop fields from `DEFAULT_DRAFT` when a stored draft predates them. */
function withLoopDefaults(base: BaseFillDraft, loose: Record<string, unknown>): FillDraft {
  const capture: CaptureMode =
    loose.capture === 'steps' || loose.capture === 'realtime' ? loose.capture : DEFAULT_DRAFT.capture;
  const timing: StepTiming =
    loose.timing === 'equal' || loose.timing === 'proportional' ? loose.timing : DEFAULT_DRAFT.timing;
  const frameMs = typeof loose.frameMs === 'number' ? loose.frameMs : DEFAULT_DRAFT.frameMs;
  const endHoldMs = typeof loose.endHoldMs === 'number' ? loose.endHoldMs : DEFAULT_DRAFT.endHoldMs;
  const maxSteps = typeof loose.maxSteps === 'number' ? loose.maxSteps : DEFAULT_DRAFT.maxSteps;
  return { ...base, capture, timing, frameMs, endHoldMs, maxSteps };
}

/** Read a player's last fill draft from `localStorage`, or the default. */
function loadDraft(playerId: string): FillDraft {
  try {
    const raw = window.localStorage.getItem(draftStorageKey(playerId));
    if (raw === null) return DEFAULT_DRAFT;
    const parsed: unknown = JSON.parse(raw);
    return isBaseFillDraft(parsed) ? withLoopDefaults(parsed, parsed) : DEFAULT_DRAFT;
  } catch {
    // localStorage can throw (disabled / private mode); fall back silently.
    return DEFAULT_DRAFT;
  }
}

/** Persist a player's fill draft so the next fill (or re-capture) prefills it. */
function saveDraft(playerId: string, draft: FillDraft): void {
  try {
    window.localStorage.setItem(draftStorageKey(playerId), JSON.stringify(draft));
  } catch {
    // Best-effort only; a failed write never blocks the fill itself.
  }
}

/** Problem detail (or message) text from a thrown API error, for display. */
function errorText(error: unknown): string {
  if (error instanceof Error) {
    const detail: unknown = 'detail' in error ? error.detail : undefined;
    if (typeof detail === 'string' && detail !== '') return detail;
    return error.message;
  }
  return String(error);
}

/** Props for {@link WindyFillDialog}. */
export interface WindyFillDialogProps {
  readonly player: MediaPlayer | null;
  readonly onOpenChange: (open: boolean) => void;
}

/**
 * The "Fill from web…" dialog for one player: capture a bounded Windy view,
 * then load the finished artifact into that player.
 */
export function WindyFillDialog({ player, onOpenChange }: WindyFillDialogProps): JSX.Element {
  const { t } = useLingui();
  const capabilities = useCaptureCapabilities();
  const createCapture = useCreateCapture();
  const cancelCapture = useCancelCapture();
  const loadArtifact = useLoadMediaPlayerArtifact();

  // The parent mounts this component with `key={player.id}`, so a fresh
  // mount per player is the reset mechanism — no effect needs to clear
  // state when the player prop changes (there is exactly one player per
  // mount's lifetime).
  const [draft, setDraft] = useState<FillDraft>(() => (player !== null ? loadDraft(player.id) : DEFAULT_DRAFT));
  const [termsAccepted, setTermsAccepted] = useState(false);
  const [jobId, setJobId] = useState<number | undefined>(undefined);
  const [loadKey, setLoadKey] = useState<string | undefined>(undefined);
  const [loadOperation, setLoadOperation] = useState<number | undefined>(undefined);
  const [loadError, setLoadError] = useState<string | undefined>(undefined);
  const [loadStarted, setLoadStarted] = useState(false);
  // Avoids re-triggering the artifact load on every re-render once the job
  // has succeeded for THIS attempt; reset whenever a new fill starts.
  const loadTriggeredRef = useRef(false);

  const job = useCaptureJob(jobId);
  const loadOp = useMediaPlayerLoadOperation(player?.id, loadOperation);

  const jobState = job.data?.state;
  const terminalCaptureState =
    jobState === 'succeeded' || jobState === 'failed' || jobState === 'cancelled' || jobState === 'uncertain';

  // On a fresh success, submit the artifact load exactly once per attempt.
  useEffect(() => {
    if (
      player === null
      || jobState !== 'succeeded'
      || job.data?.artifact_id === undefined
      || job.data.artifact_id === null
      || jobId === undefined
      || loadTriggeredRef.current
    ) {
      return;
    }
    loadTriggeredRef.current = true;
    setLoadStarted(true);
    setLoadError(undefined);
    const key = loadKey ?? newIdempotencyKey();
    setLoadKey(key);
    loadArtifact.mutate(
      {
        id: player.id,
        artifactRef: { captureJobId: jobId, artifactId: job.data.artifact_id },
        then: draft.then,
        idempotencyKey: key,
      },
      {
        onSuccess: (response): void => {
          if ('operation' in response) {
            setLoadOperation(response.operation);
          } else {
            // Legacy `AcceptedBody`: no native-load operation to poll;
            // nothing more to observe here.
            setLoadOperation(undefined);
          }
        },
        onError: (error): void => {
          setLoadError(errorText(error));
        },
      },
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps -- loadTriggeredRef gates re-entry; draft/loadArtifact/loadKey are stable enough for this one-shot trigger.
  }, [player, jobState, job.data, jobId]);

  if (player === null) {
    return <></>;
  }

  const playerId = player.id;
  const open = true;

  const close = (): void => {
    onOpenChange(false);
  };

  const submitting = createCapture.isPending;
  const inFlight = jobId !== undefined && !terminalCaptureState;
  const capsAvailable = capabilities.data?.capture_available === true;
  const premiumAvailable = capabilities.data?.premium_available === true;
  // Default to public when Premium is not configured; only offer Premium
  // when the server actually supports it (409 otherwise — provider.rs).
  const effectivePremium = draft.premium && premiumAvailable;

  const applyPreset = (preset: LocationPreset): void => {
    setDraft((d) => ({ ...d, lat: preset.lat, lon: preset.lon }));
  };
  const applySize = (preset: SizePreset): void => {
    setDraft((d) => ({ ...d, width: preset.width, height: preset.height }));
  };

  // Wind only supports realtime capture (the server refuses `steps` +
  // `wind`); radar defaults to — and may choose — steps.
  const isWind = draft.overlay === 'wind';
  const effectiveCapture: CaptureMode = isWind ? 'realtime' : draft.capture;
  const isStepsMode = effectiveCapture === 'steps';
  const nominalFrames = isStepsMode
    ? nominalStepsFrames(draft.fps, draft.frameMs, draft.endHoldMs, draft.maxSteps)
    : draft.frames;
  const frameCapExceeded = isStepsMode && nominalFrames > MAX_NOMINAL_FRAMES;

  const submit = (): void => {
    if (!termsAccepted || !capsAvailable || frameCapExceeded) return;
    const view: CaptureView = {
      lat: draft.lat,
      lon: draft.lon,
      zoom: draft.zoom,
      overlay: draft.overlay,
      width: draft.width,
      height: draft.height,
      fps: draft.fps,
      frames: draft.frames,
      public: !effectivePremium,
      capture: effectiveCapture,
      ...(isStepsMode
        ? { timing: draft.timing, frame_ms: draft.frameMs, end_hold_ms: draft.endHoldMs, max_steps: draft.maxSteps }
        : {}),
    };
    const body: CaptureBody = {
      fill_id: windyFillId(playerId),
      target_player: playerId,
      view,
      terms: { accepted: true, version: WINDY_TERMS_VERSION },
    };
    saveDraft(playerId, draft);
    const key = newIdempotencyKey();
    setJobId(undefined);
    setLoadOperation(undefined);
    setLoadError(undefined);
    setLoadStarted(false);
    loadTriggeredRef.current = false;
    createCapture.mutate(
      { body, idempotencyKey: key },
      {
        onSuccess: (accepted): void => {
          setJobId(accepted.job_id);
        },
      },
    );
  };

  const recapture = (): void => {
    setJobId(undefined);
    setLoadOperation(undefined);
    setLoadError(undefined);
    setLoadStarted(false);
    loadTriggeredRef.current = false;
    submit();
  };

  const captureJobLabel = (state: string | undefined): string => {
    switch (state) {
      case 'queued':
        return t`Queued`;
      case 'running':
        return t`Running`;
      case 'succeeded':
        return t`Succeeded`;
      case 'failed':
        return t`Failed`;
      case 'cancelled':
        return t`Cancelled`;
      case 'uncertain':
        return t`Uncertain`;
      default:
        return t`Unknown`;
    }
  };

  const loadOperationLabel = (state: string | undefined): string => {
    switch (state) {
      case 'pending':
      case 'queued':
      case 'in_progress':
        return t`Committing`;
      case 'committed':
        return t`Committed`;
      case 'failed':
        return t`Failed`;
      case 'superseded':
        return t`Superseded`;
      default:
        return state ?? t`Unknown`;
    }
  };

  const eofPolicy = (() => {
    const body: unknown = player.body;
    if (typeof body === 'object' && body !== null && 'eof_policy' in body) {
      const value = (body as Record<string, unknown>).eof_policy;
      if (typeof value === 'string') return value;
    }
    return 'hold_last_frame';
  })();

  return (
    <Dialog
      open={open}
      onOpenChange={(next: boolean): void => {
        if (!next) close();
      }}
    >
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>
            <Trans>Fill from web…</Trans>
          </DialogTitle>
          <DialogDescription>
            <Trans>
              Capture a bounded Windy weather view and load it into this
              player.
            </Trans>
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-4">
          <p className="text-sm">
            <Trans>Player</Trans>: <code>{playerId}</code>
          </p>

          {capabilities.isPending ? (
            <p role="status" className="text-sm text-muted-foreground">
              <Trans>Checking capture capabilities…</Trans>
            </p>
          ) : !capsAvailable ? (
            <p role="alert" className="text-sm text-destructive">
              <Trans>Web capture is not provisioned on this server.</Trans>
            </p>
          ) : null}

          <fieldset disabled={!capsAvailable || inFlight} className="grid gap-4">
            <div className="grid gap-1.5">
              <Label htmlFor="windy-template">
                <Trans>Template</Trans>
              </Label>
              <Input id="windy-template" value="Windy" disabled readOnly />
            </div>

            <div className="grid gap-1.5">
              <Label htmlFor="windy-overlay">
                <Trans>Overlay</Trans>
              </Label>
              <Select
                value={draft.overlay}
                onValueChange={(value): void => {
                  setDraft((d) => ({ ...d, overlay: value === 'wind' ? 'wind' : 'radar' }));
                }}
              >
                <SelectTrigger id="windy-overlay" className="w-56">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="radar">{t`Radar / storm`}</SelectItem>
                  <SelectItem value="wind">{t`Wind`}</SelectItem>
                </SelectContent>
              </Select>
            </div>

            <div className="grid gap-1.5">
              <Label>
                <Trans>Location</Trans>
              </Label>
              <div className="flex flex-wrap gap-2">
                {LOCATION_PRESETS.map((preset) => (
                  <Button
                    key={preset.label}
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={(): void => {
                      applyPreset(preset);
                    }}
                  >
                    {preset.label}
                  </Button>
                ))}
              </div>
              <div className="flex gap-2">
                <div className="grid gap-1.5">
                  <Label htmlFor="windy-lat">
                    <Trans>Latitude</Trans>
                  </Label>
                  <Input
                    id="windy-lat"
                    type="number"
                    min={-85}
                    max={85}
                    step="0.01"
                    value={draft.lat}
                    onChange={(e): void => {
                      setDraft((d) => ({ ...d, lat: Number(e.target.value) }));
                    }}
                  />
                </div>
                <div className="grid gap-1.5">
                  <Label htmlFor="windy-lon">
                    <Trans>Longitude</Trans>
                  </Label>
                  <Input
                    id="windy-lon"
                    type="number"
                    min={-180}
                    max={180}
                    step="0.01"
                    value={draft.lon}
                    onChange={(e): void => {
                      setDraft((d) => ({ ...d, lon: Number(e.target.value) }));
                    }}
                  />
                </div>
              </div>
            </div>

            <div className="grid gap-1.5">
              <Label htmlFor="windy-zoom">
                <Trans>Zoom</Trans>
              </Label>
              <Input
                id="windy-zoom"
                type="number"
                min={1}
                max={18}
                step={1}
                value={draft.zoom}
                onChange={(e): void => {
                  setDraft((d) => ({ ...d, zoom: Number(e.target.value) }));
                }}
              />
            </div>

            <div className="grid gap-1.5">
              <Label>
                <Trans>Size</Trans>
              </Label>
              <div className="flex flex-wrap gap-2">
                {SIZE_PRESETS.map((preset) => (
                  <Button
                    key={preset.label}
                    type="button"
                    variant={draft.width === preset.width && draft.height === preset.height ? 'default' : 'outline'}
                    size="sm"
                    disabled={preset.width < 1024}
                    onClick={(): void => {
                      applySize(preset);
                    }}
                  >
                    {preset.label}
                  </Button>
                ))}
              </div>
              <p className="text-xs text-muted-foreground">
                <Trans>Windy requires a minimum width of 1024px (desktop geometry).</Trans>
              </p>
            </div>

            <div className="flex gap-2">
              <div className="grid gap-1.5">
                <Label htmlFor="windy-fps">
                  <Trans>FPS</Trans>
                </Label>
                <Input
                  id="windy-fps"
                  type="number"
                  min={1}
                  max={30}
                  step={1}
                  value={draft.fps}
                  onChange={(e): void => {
                    setDraft((d) => ({ ...d, fps: Number(e.target.value) }));
                  }}
                />
              </div>
              {isStepsMode ? null : (
                <div className="grid gap-1.5">
                  <Label htmlFor="windy-frames">
                    <Trans>Frames</Trans>
                  </Label>
                  <Input
                    id="windy-frames"
                    type="number"
                    min={2}
                    max={600}
                    step={1}
                    value={draft.frames}
                    onChange={(e): void => {
                      setDraft((d) => ({ ...d, frames: Number(e.target.value) }));
                    }}
                  />
                </div>
              )}
            </div>

            {premiumAvailable ? (
              <div className="flex items-center gap-2">
                <input
                  id="windy-premium"
                  type="checkbox"
                  className="size-4"
                  checked={draft.premium}
                  onChange={(e): void => {
                    setDraft((d) => ({ ...d, premium: e.target.checked }));
                  }}
                />
                <Label htmlFor="windy-premium">
                  <Trans>Premium (authenticated) capture</Trans>
                </Label>
              </div>
            ) : (
              <p className="text-xs text-muted-foreground">
                <Trans>
                  Premium Windy capture is not configured on this server; the
                  public (unauthenticated) view is used.
                </Trans>
              </p>
            )}

            <div className="grid gap-1.5">
              <Label>
                <Trans>Refresh</Trans>
              </Label>
              <div className="flex items-center gap-2">
                <input id="windy-refresh-once" type="radio" name="windy-refresh" checked readOnly className="size-4" />
                <Label htmlFor="windy-refresh-once">
                  <Trans>Once</Trans>
                </Label>
              </div>
              <div className="flex items-center gap-2">
                <input
                  id="windy-refresh-scheduled"
                  type="radio"
                  name="windy-refresh"
                  disabled
                  className="size-4"
                />
                <Label htmlFor="windy-refresh-scheduled" className="text-muted-foreground">
                  <Trans>Every N minutes (coming soon — no scheduler yet)</Trans>
                </Label>
              </div>
            </div>

            <div className="grid gap-1.5 rounded-md border p-3">
              <Label>
                <Trans>Loop</Trans>
              </Label>

              <div className="grid gap-1.5">
                <Label>
                  <Trans>Capture mode</Trans>
                </Label>
                <div className="flex items-center gap-2">
                  <input
                    id="windy-capture-steps"
                    type="radio"
                    name="windy-capture"
                    className="size-4"
                    checked={effectiveCapture === 'steps'}
                    disabled={isWind}
                    onChange={(): void => {
                      setDraft((d) => ({ ...d, capture: 'steps' }));
                    }}
                  />
                  <Label htmlFor="windy-capture-steps" className={isWind ? 'text-muted-foreground' : undefined}>
                    <Trans>Radar steps</Trans>
                  </Label>
                </div>
                <div className="flex items-center gap-2">
                  <input
                    id="windy-capture-realtime"
                    type="radio"
                    name="windy-capture"
                    className="size-4"
                    checked={effectiveCapture === 'realtime'}
                    disabled={isWind}
                    onChange={(): void => {
                      setDraft((d) => ({ ...d, capture: 'realtime' }));
                    }}
                  />
                  <Label htmlFor="windy-capture-realtime">
                    <Trans>Realtime</Trans>
                  </Label>
                </div>
                {isWind ? (
                  <p className="text-xs text-muted-foreground">
                    <Trans>Wind only supports realtime capture.</Trans>
                  </p>
                ) : null}
              </div>

              {isStepsMode ? (
                <>
                  <div className="grid gap-1.5">
                    <Label htmlFor="windy-timing">
                      <Trans>Timing</Trans>
                    </Label>
                    <Select
                      value={draft.timing}
                      onValueChange={(value): void => {
                        setDraft((d) => ({ ...d, timing: value === 'proportional' ? 'proportional' : 'equal' }));
                      }}
                    >
                      <SelectTrigger id="windy-timing" className="w-56">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="equal">{t`Equal`}</SelectItem>
                        <SelectItem value="proportional">{t`Proportional to radar time`}</SelectItem>
                      </SelectContent>
                    </Select>
                  </div>

                  <div className="flex gap-2">
                    <div className="grid gap-1.5">
                      <Label htmlFor="windy-frame-ms">
                        <Trans>Frame duration (ms)</Trans>
                      </Label>
                      <Input
                        id="windy-frame-ms"
                        type="number"
                        min={100}
                        max={5000}
                        step={1}
                        value={draft.frameMs}
                        onChange={(e): void => {
                          setDraft((d) => ({ ...d, frameMs: Number(e.target.value) }));
                        }}
                      />
                    </div>
                    <div className="grid gap-1.5">
                      <Label htmlFor="windy-end-hold-ms">
                        <Trans>End hold (ms)</Trans>
                      </Label>
                      <Input
                        id="windy-end-hold-ms"
                        type="number"
                        min={0}
                        max={10000}
                        step={1}
                        value={draft.endHoldMs}
                        onChange={(e): void => {
                          setDraft((d) => ({ ...d, endHoldMs: Number(e.target.value) }));
                        }}
                      />
                    </div>
                    <div className="grid gap-1.5">
                      <Label htmlFor="windy-max-steps">
                        <Trans>Max steps</Trans>
                      </Label>
                      <Input
                        id="windy-max-steps"
                        type="number"
                        min={2}
                        max={48}
                        step={1}
                        value={draft.maxSteps}
                        onChange={(e): void => {
                          setDraft((d) => ({ ...d, maxSteps: Number(e.target.value) }));
                        }}
                      />
                    </div>
                  </div>

                  {frameCapExceeded ? (
                    <p role="alert" className="text-xs text-destructive">
                      <Trans>
                        This would capture {nominalFrames} frames, above the
                        server's cap of {MAX_NOMINAL_FRAMES}. Lower max
                        steps, frame duration or end hold.
                      </Trans>
                    </p>
                  ) : (
                    <p className="text-xs text-muted-foreground">
                      <Trans>Nominal total frames: {nominalFrames} (cap {MAX_NOMINAL_FRAMES}).</Trans>
                    </p>
                  )}
                </>
              ) : null}
            </div>

            <div className="grid gap-1.5">
              <Label htmlFor="windy-then">
                <Trans>Then</Trans>
              </Label>
              <Select
                value={draft.then}
                onValueChange={(value): void => {
                  setDraft((d) => ({ ...d, then: value === 'cue' ? 'cue' : 'resume' }));
                }}
              >
                <SelectTrigger id="windy-then" className="w-56">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="resume">{t`Play`}</SelectItem>
                  <SelectItem value="cue">{t`Cue`}</SelectItem>
                </SelectContent>
              </Select>
            </div>

            <p className="text-xs text-muted-foreground">
              <Trans>
                Current end-of-clip behaviour for this player:{' '}
              </Trans>
              <code>{eofPolicy}</code>.{' '}
              {eofPolicy === 'loop' ? null : (
                <Trans>
                  Set this player to loop at end of clip in its configuration
                  for a continuously-refreshed fill to play seamlessly.
                </Trans>
              )}
            </p>

            <div className="flex items-start gap-2">
              <input
                id="windy-terms"
                type="checkbox"
                className="mt-1 size-4"
                checked={termsAccepted}
                onChange={(e): void => {
                  setTermsAccepted(e.target.checked);
                }}
              />
              <Label htmlFor="windy-terms" className="font-normal">
                <Trans>
                  I accept the{' '}
                  <a
                    href="https://account.windy.com/agreements/windy-terms-of-use"
                    target="_blank"
                    rel="noreferrer noopener"
                    className="underline"
                  >
                    Windy terms of use
                  </a>{' '}
                  ({WINDY_TERMS_VERSION})
                </Trans>
              </Label>
            </div>
          </fieldset>

          {createCapture.isError ? (
            <p role="alert" className="text-sm text-destructive">
              {errorText(createCapture.error)}
            </p>
          ) : null}

          {jobId !== undefined ? (
            <div role="status" className="rounded-md border p-3 text-sm">
              <p className="flex items-center gap-2">
                {terminalCaptureState ? (
                  jobState === 'succeeded' ? (
                    <CheckCircle2 className="size-4 text-green-600" aria-hidden="true" />
                  ) : (
                    <XCircle className="size-4 text-destructive" aria-hidden="true" />
                  )
                ) : (
                  <Loader2 className="size-4 animate-spin" aria-hidden="true" />
                )}
                <Trans>Capture job</Trans> <code>{jobId}</code>:{' '}
                {captureJobLabel(jobState)}
              </p>
              {job.isError ? (
                <p role="alert" className="mt-1 text-destructive">
                  {errorText(job.error)}
                </p>
              ) : null}

              {loadStarted ? (
                <div className="mt-2 border-t pt-2">
                  <p className="flex items-center gap-2">
                    {loadOp.data?.state === 'committed' ? (
                      <CheckCircle2 className="size-4 text-green-600" aria-hidden="true" />
                    ) : loadOp.data?.state === 'failed' ? (
                      <XCircle className="size-4 text-destructive" aria-hidden="true" />
                    ) : (
                      <Loader2 className="size-4 animate-spin" aria-hidden="true" />
                    )}
                    <Trans>Load</Trans>:{' '}
                    {loadOperation !== undefined
                      ? loadOperationLabel(loadOp.data?.state)
                      : t`Accepted`}
                  </p>
                  {loadError !== undefined ? (
                    <p role="alert" className="mt-1 text-destructive">
                      {loadError}
                    </p>
                  ) : null}
                  {loadOp.isError ? (
                    <p role="alert" className="mt-1 text-destructive">
                      {errorText(loadOp.error)}
                    </p>
                  ) : null}
                  {loadOp.data?.native !== undefined && loadOp.data.native !== null ? (
                    <p className="mt-1 text-xs text-muted-foreground">
                      <Trans>Duration</Trans>:{' '}
                      {loadOp.data.native.duration_frames ?? '—'} {t`frames`} ·{' '}
                      {loadOp.data.native.width}×{loadOp.data.native.height} ·{' '}
                      {loadOp.data.native.has_audio ? t`audio present` : t`silent`}
                    </p>
                  ) : null}
                </div>
              ) : null}
            </div>
          ) : null}
        </div>

        <DialogFooter>
          {jobId !== undefined && !terminalCaptureState ? (
            <Button
              type="button"
              variant="outline"
              disabled={cancelCapture.isPending}
              onClick={(): void => {
                cancelCapture.mutate({ jobId, idempotencyKey: newIdempotencyKey() });
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
          ) : null}
          <Button type="button" variant="outline" onClick={close}>
            <Trans>Close</Trans>
          </Button>
          {jobId === undefined || (terminalCaptureState && jobState !== 'succeeded') ? (
            <Button
              type="button"
              disabled={!termsAccepted || !capsAvailable || submitting || frameCapExceeded}
              onClick={jobId === undefined ? submit : recapture}
            >
              {submitting ? <Loader2 className="mr-2 size-4 animate-spin" aria-hidden="true" /> : null}
              <Trans>Capture</Trans>
            </Button>
          ) : terminalCaptureState ? (
            <Button type="button" onClick={recapture}>
              <Trans>Re-capture</Trans>
            </Button>
          ) : null}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
