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
// (`max_steps * max(1, framesAt(frame_ms, fps)) + framesAt(end_hold_ms,
// fps)`, half-up integer rounding exactly like the server) must stay <= 600
// (the server's hard cap), validated client-side below.
//
// IDENTITY (capture attempt / load key): one capture ATTEMPT is an immutable
// {Idempotency-Key, CaptureBody} pair, minted fresh only on an explicit new
// attempt (first submit, Re-capture, or a changed parameter) and otherwise
// kept verbatim so a lost-response Retry resends the identical pair — the
// server replays the same job rather than starting a second one. Likewise
// the artifact-load Idempotency-Key is minted fresh once per NEW succeeded
// capture job and reused only for a retry of that same load; reusing it
// across two different jobs would send two different `artifactRef`s under
// one key, which the server refuses with "load key reused with different
// request" (`crates/multiview-cli/src/player/provider.rs`).
import { useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { CheckCircle2, Loader2, XCircle } from 'lucide-react';

import type { ArtifactRef, LoadFailureCode, LoadThen, MediaPlayer } from '../api/media-playersQueries';
import { useLoadMediaPlayerArtifact, useMediaPlayerLoadOperation } from '../api/media-playersQueries';
import { newIdempotencyKey, OperationApiError } from '../api/operations';
import {
  MAX_REFRESH_MINUTES,
  MIN_REFRESH_MINUTES,
  useCancelCapture,
  useCaptureCapabilities,
  useCaptureJob,
  useCreateCapture,
  useDeleteFillRefresh,
  useFillRefresh,
  usePutFillRefresh,
} from '../api/web-captureQueries';
import type { CaptureBody, CaptureView } from '../api/web-captureQueries';
import { WINDY_TERMS_VERSION, windyFillId } from '../api/web-capture';
import { FillRefreshSummary } from './FillRefreshStatus';
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

/**
 * Stable id of the frame-cap error, linked from every contributing input's
 * `aria-describedby` (fps, frame duration, end hold, max steps) while it is
 * `aria-invalid` (finding 5), so a screen reader announces the one error at
 * each field it is caused by.
 */
const FRAME_CAP_ERROR_ID = 'windy-frame-cap-error';

/** Default steps-mode per-step duration, ms (`View.frame_ms`). */
const DEFAULT_FRAME_MS = 600;

/** Default steps-mode newest-step hold, ms (`View.end_hold_ms`). */
const DEFAULT_END_HOLD_MS = 1000;

/** Default steps-mode step count (`View.max_steps`). */
const DEFAULT_MAX_STEPS = 12;

/**
 * Half-up integer frame count for a duration at `fps`, bit-for-bit identical
 * to the server's `frames_for_ms` (`crates/multiview-webengine/src/capture.rs`)
 * and its JS mirror `framesAt` (`crates/multiview-webengine/src/step_plan.cjs`):
 * `floor((ms * fps + 500) / 1000)`. Every input here is a small safe integer
 * (`frame_ms` <= 5000, `fps` <= 30, `end_hold_ms` <= 10000), so this integer
 * formula is exact — never `Math.round`, which is float-based and not what
 * either server implementation computes.
 */
function framesAtHalfUp(ms: number, fps: number): number {
  return Math.floor((ms * fps + 500) / 1000);
}

/**
 * The nominal total frame count the server computes for a steps-mode
 * capture: `max_steps * max(1, framesAt(frame_ms, fps)) + framesAt(end_hold_ms,
 * fps)` (`capture.rs` ~226, `step_plan.cjs` `validateOptions`). The `max(1,
 * …)` matters: a short `frame_ms` at a low `fps` can round to zero steps,
 * which the server still counts as one. Mirrors the server-side cap check so
 * the client can refuse a doomed submit before it round-trips.
 */
function nominalStepsFrames(fps: number, frameMs: number, endHoldMs: number, maxSteps: number): number {
  const perStep = Math.max(1, framesAtHalfUp(frameMs, fps));
  const hold = framesAtHalfUp(endHoldMs, fps);
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
  // "Use Premium" defaults on; it only takes effect when the server reports
  // premium_available (a verified Windy account), else the public view is used.
  premium: true,
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
  // Drafts saved before Premium existed carry a never-chosen `premium: false`;
  // only an explicit `usePremium` choice overrides the default-on.
  const premium = typeof loose.usePremium === 'boolean' ? loose.usePremium : DEFAULT_DRAFT.premium;
  return { ...base, premium, capture, timing, frameMs, endHoldMs, maxSteps };
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
    window.localStorage.setItem(
      draftStorageKey(playerId),
      JSON.stringify({ ...draft, usePremium: draft.premium }),
    );
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
 * One immutable capture attempt: the `Idempotency-Key` minted for it and the
 * exact `CaptureBody` sent with that key. Kept until a brand-new attempt
 * (submit from scratch, or Re-capture) replaces it — never discarded on a
 * failed/unknown POST outcome, so "Retry" can resend the IDENTICAL key+body
 * and let the server replay the same job rather than starting a second one
 * (`crates/multiview-control/src/routes/web_capture.rs`: "same key and body
 * replays the same job").
 */
interface CaptureAttempt {
  readonly key: string;
  readonly body: CaptureBody;
}

/**
 * One immutable artifact-load attempt: the `Idempotency-Key` minted for it
 * and the EXACT body (`artifactRef` + `then`) sent with that key. "Retry
 * load" must resend this snapshot verbatim — never rebuild `then` from the
 * live draft, which stays editable after a succeeded capture. The server's
 * load-key reuse check compares the stored `then` too
 * (`crates/multiview-cli/src/player/provider.rs`: `old.then != then` ->
 * "load key reused with different request"), so a Then changed after
 * capture but before Retry load would otherwise be rejected.
 */
interface LoadAttempt {
  readonly key: string;
  readonly artifactRef: ArtifactRef;
  readonly then: LoadThen | undefined;
}

/**
 * True for a definite (non-network) rejection: the server answered with a
 * structured 4xx and did NOT admit the job, so a retry of the identical
 * body cannot succeed — only a corrected "Start new capture" can (finding
 * 3). A network error (lost response) carries no such status: the prior
 * POST's outcome is genuinely unknown, so "Retry same request" (same
 * key+body) remains the safe default.
 */
function isDefiniteRejection(error: unknown): boolean {
  return (
    error instanceof OperationApiError
    && error.status !== undefined
    && error.status >= 400
    && error.status < 500
  );
}

/**
 * Client-side mirror of the server's `View` bounds (schema.ts) so an
 * obviously invalid value never round-trips to a 422 (finding 3): fps
 * 1..30, frames 2..600 (realtime mode only — steps mode ignores `frames`),
 * zoom 1..18, lat -85..85, lon -180..180 (matching the Latitude/Longitude
 * inputs' own `min`/`max`).
 */
function isDraftValid(draft: FillDraft, isStepsMode: boolean): boolean {
  if (!Number.isFinite(draft.fps) || draft.fps < 1 || draft.fps > 30) return false;
  if (!isStepsMode && (!Number.isFinite(draft.frames) || draft.frames < 2 || draft.frames > 600)) return false;
  if (!Number.isFinite(draft.zoom) || draft.zoom < 1 || draft.zoom > 18) return false;
  if (!Number.isFinite(draft.lat) || draft.lat < -85 || draft.lat > 85) return false;
  if (!Number.isFinite(draft.lon) || draft.lon < -180 || draft.lon > 180) return false;
  return true;
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
  const refreshStatus = useFillRefresh(player !== null ? windyFillId(player.id) : undefined);
  const putRefresh = usePutFillRefresh();
  const deleteRefresh = useDeleteFillRefresh();
  // Scheduled refresh (issue #472): "once" is the plain capture+load; "every"
  // additionally registers a server-side schedule that re-captures this same
  // view every N minutes and reloads the player (keeping the last good clip
  // on air until each new one is natively adopted).
  const [refreshMode, setRefreshMode] = useState<'once' | 'every'>('once');
  const [refreshMinutes, setRefreshMinutes] = useState(5);

  // The parent mounts this component with `key={player.id}`, so a fresh
  // mount per player is the reset mechanism — no effect needs to clear
  // state when the player prop changes (there is exactly one player per
  // mount's lifetime).
  const [draft, setDraft] = useState<FillDraft>(() => (player !== null ? loadDraft(player.id) : DEFAULT_DRAFT));
  const [termsAccepted, setTermsAccepted] = useState(false);
  const [jobId, setJobId] = useState<number | undefined>(undefined);
  // The immutable {key, body} of the current capture attempt (finding 2):
  // kept across a lost-response retry, replaced only by a brand-new attempt.
  const [captureAttempt, setCaptureAttempt] = useState<CaptureAttempt | undefined>(undefined);
  // The immutable {key, artifactRef, then} snapshot for THIS capture
  // attempt's artifact load (finding 1): minted once per jobId (i.e. once
  // per new succeeded capture) by snapshotting `draft.then` at that instant,
  // then kept verbatim — "Retry load" resends this exact snapshot, never a
  // `then` rebuilt from the live (still-editable) draft. A brand-new capture
  // (Re-capture, or a changed-params submit) always gets a brand-new jobId
  // and therefore a brand-new load attempt.
  const [loadAttempt, setLoadAttempt] = useState<LoadAttempt | undefined>(undefined);
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

  /**
   * POST the artifact load for an EXPLICIT, already-snapshotted attempt
   * (finding 1): the caller decides the key/artifactRef/then and this
   * function never rebuilds any of them from the live draft. Used both by
   * the one-shot auto-trigger below and by "Retry load" — both pass the
   * SAME `LoadAttempt` object, so a retry resends the identical body.
   */
  const submitLoad = (attempt: LoadAttempt): void => {
    if (player === null) return;
    setLoadStarted(true);
    setLoadError(undefined);
    loadArtifact.mutate(
      {
        id: player.id,
        artifactRef: attempt.artifactRef,
        ...(attempt.then !== undefined ? { then: attempt.then } : {}),
        idempotencyKey: attempt.key,
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
  };

  // On a fresh success, submit the artifact load exactly once per attempt,
  // snapshotting a BRAND-NEW {key, artifactRef, then} for this (job,
  // artifact) pair — never the previous attempt's snapshot, which was bound
  // to a different artifactRef and would be refused by the server ("load
  // key reused with different request", finding 1). `draft.then` is read
  // HERE, at the instant the capture succeeds, and frozen into the
  // snapshot: later edits to the Then selector (still possible while the
  // load is pending/failed) never change what a subsequent "Retry load"
  // resends.
  useEffect(() => {
    if (
      jobState !== 'succeeded'
      || job.data?.artifact_id === undefined
      || job.data.artifact_id === null
      || jobId === undefined
      || loadTriggeredRef.current
    ) {
      return;
    }
    loadTriggeredRef.current = true;
    const attempt: LoadAttempt = {
      key: newIdempotencyKey(),
      artifactRef: { captureJobId: jobId, artifactId: job.data.artifact_id },
      then: draft.then,
    };
    setLoadAttempt(attempt);
    submitLoad(attempt);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- loadTriggeredRef gates re-entry; draft.then is intentionally snapshotted once per succeeded job, not re-read on every draft change.
  }, [jobState, job.data, jobId]);

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
  // Premium is on by default when the server reports a verified Windy account;
  // otherwise the public view is used (the server refuses Premium with 409).
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
  // Finding 3: block submit on an obviously invalid field (fps/frames/
  // zoom/lat/lon out of the server's documented range) before it ever
  // round-trips to a 422.
  const minutesInvalid =
    refreshMode === 'every'
    && (!Number.isInteger(refreshMinutes)
      || refreshMinutes < MIN_REFRESH_MINUTES
      || refreshMinutes > MAX_REFRESH_MINUTES);
  const draftInvalid = !isDraftValid(draft, isStepsMode) || minutesInvalid;
  const schedule = refreshStatus.data ?? null;

  /** Build the current draft's `CaptureBody` (never submits by itself). */
  const buildCaptureBody = (): CaptureBody => {
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
    return {
      fill_id: windyFillId(playerId),
      target_player: playerId,
      view,
      terms: { accepted: true, version: WINDY_TERMS_VERSION },
    };
  };

  /**
   * POST one capture attempt's {key, body} — never mints a new pair itself.
   * Both a definite server error AND a lost response (network error: the
   * `fetch` in `createCapture` throws, which `useMutation` surfaces as this
   * same `onError`) land here with NO new key minted (finding 2): the
   * attempt's key+body survive untouched in `captureAttempt`, so "Retry"
   * resends the identical pair and the server either replays the admitted
   * job or runs it for the first time — never a second, differently-keyed
   * capture of the same operator intent.
   */
  const submitAttempt = (attempt: CaptureAttempt): void => {
    createCapture.mutate(
      { body: attempt.body, idempotencyKey: attempt.key },
      {
        onSuccess: (accepted): void => {
          // A NEW succeeded job always gets a NEW load attempt (finding 1):
          // the artifact differs from any prior attempt's, so the previous
          // load snapshot — bound to a different artifactRef — must never
          // be reused.
          setLoadAttempt(undefined);
          setJobId(accepted.job_id);
        },
      },
    );
  };

  /** A brand-new capture attempt: mint a fresh key+body pair and submit it. */
  const submit = (): void => {
    if (!termsAccepted || !capsAvailable || frameCapExceeded || draftInvalid) return;
    saveDraft(playerId, draft);
    const attempt: CaptureAttempt = { key: newIdempotencyKey(), body: buildCaptureBody() };
    setCaptureAttempt(attempt);
    setJobId(undefined);
    setLoadOperation(undefined);
    setLoadError(undefined);
    setLoadStarted(false);
    loadTriggeredRef.current = false;
    submitAttempt(attempt);
    if (refreshMode === 'every') {
      // The schedule starts at the NEXT slot: this capture is the first clip.
      putRefresh.mutate({
        fillId: attempt.body.fill_id,
        body: { minutes: refreshMinutes, then: draft.then, capture: attempt.body, start_now: false },
      });
    }
  };

  /** Re-capture: always a brand-new attempt (new key, new body, new job). */
  const recapture = (): void => {
    submit();
  };

  /**
   * Retry the CURRENT attempt's POST with its IDENTICAL key+body (finding
   * 2): used only when the previous POST's outcome is unknown or failed —
   * never mints a new idempotency key, so an admitted-but-lost first attempt
   * replays the same job rather than starting a second capture. Gated on
   * terms still being accepted RIGHT NOW (finding 2): the stored attempt
   * body always carries `terms.accepted: true` from when it was built, so
   * unchecking the box after an error and then hitting Retry must not
   * silently resend that stale acceptance — it is a no-op instead.
   */
  const retrySubmit = (): void => {
    if (captureAttempt === undefined || !termsAccepted) return;
    submitAttempt(captureAttempt);
  };

  /**
   * "Start new capture" (finding 3): mints a brand-new key+body pair from
   * the CURRENT (possibly just-corrected) draft, for use after a definite
   * 4xx rejection where resending the same invalid body cannot succeed.
   * Identical to `submit`, kept as a distinct named action so the two
   * footer buttons read as separate operator intents.
   */
  const startNewCapture = (): void => {
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

  /**
   * Labels for the native load operation's ACTUAL state set — `pending`
   * (in flight), `completed`, `failed`, `superseded`
   * (`crates/multiview-cli/src/player/provider.rs::classify`; finding 3). No
   * other provider state (`queued`/`in_progress`/`committed`) is ever
   * reported by this server; an unrecognised value still renders (its raw
   * name) rather than silently becoming "Unknown".
   */
  const loadOperationLabel = (state: string | undefined): string => {
    switch (state) {
      case 'pending':
        return t`Committing`;
      case 'completed':
        return t`Completed`;
      case 'failed':
        return t`Failed`;
      case 'superseded':
        return t`Superseded`;
      default:
        return state ?? t`Unknown`;
    }
  };

  /**
   * Operator labels for the typed load failure reason (issue #474). An
   * unrecognised code still renders its raw name rather than disappearing.
   */
  const loadFailureLabel = (reason: LoadFailureCode | null | undefined): string => {
    switch (reason) {
      case 'wrong_cadence':
        return t`Clip frame rate does not match the canvas`;
      case 'vfr':
        return t`Clip is variable frame rate`;
      case 'no_cadence':
        return t`Clip declares no frame rate`;
      case 'alpha':
        return t`Clip carries an alpha plane`;
      case 'not_found':
        return t`Clip not found`;
      case 'invalid_data':
        return t`Clip could not be parsed`;
      case 'timeout':
        return t`Clip open timed out`;
      case 'no_video_stream':
        return t`Clip has no video stream`;
      case 'unknown_length':
        return t`Clip length is unknown`;
      case 'invalid_window':
        return t`Clip playout window is invalid`;
      case 'decoder_open':
        return t`Video decoder could not be opened`;
      case 'no_frames':
        return t`No decodable frame`;
      case 'audio_unavailable':
        return t`Player audio rail unavailable`;
      case 'journal_failed':
        return t`Player journal refused the load`;
      case 'reference_missing':
        return t`Load has no durable identity`;
      case 'other':
      case null:
      case undefined:
        return t`Load failed`;
      default:
        return String(reason);
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
                  aria-invalid={frameCapExceeded}
                  {...(frameCapExceeded ? { 'aria-describedby': FRAME_CAP_ERROR_ID } : {})}
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
                  <Trans>Use Premium (authenticated) capture</Trans>
                </Label>
              </div>
            ) : (
              <p className="text-xs text-muted-foreground">
                <Trans>
                  Premium Windy capture is not configured on this server; the
                  public (unauthenticated) view is used. An administrator can
                  set and test the Windy account under Settings → Integrations.
                </Trans>
              </p>
            )}

            <div className="grid gap-1.5">
              <Label>
                <Trans>Refresh</Trans>
              </Label>
              <div className="flex items-center gap-2">
                <input
                  id="windy-refresh-once"
                  type="radio"
                  name="windy-refresh"
                  checked={refreshMode === 'once'}
                  onChange={(): void => {
                    setRefreshMode('once');
                  }}
                  className="size-4"
                />
                <Label htmlFor="windy-refresh-once">
                  <Trans>Once</Trans>
                </Label>
              </div>
              <div className="flex items-center gap-2">
                <input
                  id="windy-refresh-scheduled"
                  type="radio"
                  name="windy-refresh"
                  checked={refreshMode === 'every'}
                  onChange={(): void => {
                    setRefreshMode('every');
                  }}
                  className="size-4"
                />
                <Label htmlFor="windy-refresh-scheduled">
                  <Trans>Every N minutes</Trans>
                </Label>
                <Input
                  id="windy-refresh-minutes"
                  type="number"
                  aria-label={t`Refresh interval (minutes)`}
                  className="w-24"
                  min={MIN_REFRESH_MINUTES}
                  max={MAX_REFRESH_MINUTES}
                  step={1}
                  disabled={refreshMode !== 'every'}
                  value={Number.isFinite(refreshMinutes) ? refreshMinutes : ''}
                  onChange={(e): void => {
                    setRefreshMinutes(e.target.valueAsNumber);
                  }}
                />
              </div>
              {minutesInvalid ? (
                <p role="alert" className="text-xs text-destructive">
                  <Trans>
                    The refresh interval must be a whole number of minutes from {MIN_REFRESH_MINUTES} to{' '}
                    {MAX_REFRESH_MINUTES}.
                  </Trans>
                </p>
              ) : null}
              {refreshMode === 'every' ? (
                <p className="text-xs text-muted-foreground">
                  <Trans>
                    The server re-captures this view on schedule as your key and reloads the player;
                    the current clip keeps playing until each new one is ready, and a failed refresh
                    keeps the previous clip.
                  </Trans>
                </p>
              ) : null}
              {putRefresh.isError ? (
                <p role="alert" className="text-xs text-destructive">
                  <Trans>Schedule not saved</Trans>: {errorText(putRefresh.error)}
                </p>
              ) : null}
              {schedule !== null ? (
                <div className="grid gap-1">
                  <FillRefreshSummary status={schedule} />
                  {schedule.source === 'api' ? (
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      className="justify-self-start"
                      disabled={deleteRefresh.isPending}
                      onClick={(): void => {
                        deleteRefresh.mutate({ fillId: schedule.fill_id });
                      }}
                    >
                      <Trans>Stop scheduled refresh</Trans>
                    </Button>
                  ) : (
                    <p className="text-xs text-muted-foreground">
                      <Trans>This schedule is declared in the configuration ([[media_fill]]).</Trans>
                    </p>
                  )}
                </div>
              ) : null}
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
                        aria-invalid={frameCapExceeded}
                        {...(frameCapExceeded ? { 'aria-describedby': FRAME_CAP_ERROR_ID } : {})}
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
                        aria-invalid={frameCapExceeded}
                        {...(frameCapExceeded ? { 'aria-describedby': FRAME_CAP_ERROR_ID } : {})}
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
                        aria-invalid={frameCapExceeded}
                        {...(frameCapExceeded ? { 'aria-describedby': FRAME_CAP_ERROR_ID } : {})}
                        onChange={(e): void => {
                          setDraft((d) => ({ ...d, maxSteps: Number(e.target.value) }));
                        }}
                      />
                    </div>
                  </div>

                  {frameCapExceeded ? (
                    <p id={FRAME_CAP_ERROR_ID} role="alert" className="text-xs text-destructive">
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

          {createCapture.isError && jobId === undefined ? (
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
                    {
                      // The real terminal states this provider reports are
                      // `completed`/`failed`/`superseded` — never `committed`
                      // (finding 3). No state renders a spinner once terminal.
                      loadOp.data?.state === 'completed' ? (
                        <CheckCircle2 className="size-4 text-green-600" aria-hidden="true" />
                      ) : loadOp.data?.state === 'failed' || loadOp.data?.state === 'superseded' ? (
                        <XCircle className="size-4 text-destructive" aria-hidden="true" />
                      ) : (
                        <Loader2 className="size-4 animate-spin" aria-hidden="true" />
                      )
                    }
                    <Trans>Load</Trans>:{' '}
                    {loadOperation !== undefined
                      ? loadOperationLabel(loadOp.data?.state)
                      : t`Accepted`}
                  </p>
                  {loadError !== undefined ? (
                    <div className="mt-1 flex items-center gap-2">
                      <p role="alert" className="text-destructive">
                        {loadError}
                      </p>
                      <Button
                        type="button"
                        variant="outline"
                        size="sm"
                        disabled={loadArtifact.isPending}
                        onClick={(): void => {
                          // Resend this SAME {key, artifactRef, then}
                          // snapshot verbatim (never rebuilt from the live
                          // draft): the server replays the same operation
                          // rather than starting a second commit, and never
                          // rejects it for a `then` that no longer matches
                          // (finding 1).
                          if (loadAttempt !== undefined) {
                            submitLoad(loadAttempt);
                          }
                        }}
                      >
                        <Trans>Retry load</Trans>
                      </Button>
                    </div>
                  ) : null}
                  {loadOp.isError ? (
                    <p role="alert" className="mt-1 text-destructive">
                      {errorText(loadOp.error)}
                    </p>
                  ) : null}
                  {loadOp.data?.state === 'failed' ? (
                    <p role="alert" className="mt-1 text-destructive" data-testid="load-failure-reason">
                      {loadFailureLabel(loadOp.data.reason)}
                      {loadOp.data.detail !== undefined && loadOp.data.detail !== null
                        ? ` — ${loadOp.data.detail}`
                        : ''}
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
          {jobId === undefined && createCapture.isError ? (
            // The POST failed or its response was lost (finding 2 & 3): a
            // definite 4xx (e.g. a 422 on an invalid field) means the
            // server did NOT admit the job, so resending the identical
            // body can only fail again — "Start new capture" with the
            // corrected draft is the one that can succeed, and is offered
            // first/emphasised for that case. "Retry same request" resends
            // the IDENTICAL {key, body} pair (never a fresh key, which
            // would risk a duplicate job for a lost-but-admitted POST) and
            // is gated on terms still being accepted right now (finding 2:
            // unchecking terms after the error must not let a stale
            // `terms.accepted: true` go out via Retry).
            <>
              {!isDefiniteRejection(createCapture.error) ? (
                <p className="text-xs text-muted-foreground">
                  <Trans>
                    The previous request's outcome may be unknown (a network
                    error) — "Retry same request" resends it unchanged.
                  </Trans>
                </p>
              ) : null}
              <Button
                type="button"
                variant="outline"
                disabled={submitting || !termsAccepted}
                onClick={retrySubmit}
              >
                {submitting ? <Loader2 className="mr-2 size-4 animate-spin" aria-hidden="true" /> : null}
                <Trans>Retry same request</Trans>
              </Button>
              <Button
                type="button"
                disabled={!termsAccepted || !capsAvailable || submitting || frameCapExceeded || draftInvalid}
                onClick={startNewCapture}
              >
                {submitting ? <Loader2 className="mr-2 size-4 animate-spin" aria-hidden="true" /> : null}
                <Trans>Start new capture</Trans>
              </Button>
            </>
          ) : jobId === undefined || (terminalCaptureState && jobState !== 'succeeded') ? (
            <Button
              type="button"
              disabled={!termsAccepted || !capsAvailable || submitting || frameCapExceeded || draftInvalid}
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
