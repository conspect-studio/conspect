// audioPresentation — the pure derivations behind the Audio page's broadcast
// console (the adopted designer direction, honest to the REAL API):
//
//   * ONE linear fader mapping — 0 dB at the top, −60 dB at the bottom —
//     shared by the ticks, the knob position and the numeric readout, so a
//     scale tick always sits exactly where the knob reads that value;
//   * exact ±1 / ±6 dB nudges that STAGE the draft and never silently clamp
//     (the routing document accepts any finite gain; the fader range is a
//     display bound, not a document bound — the typed-gain field stays the
//     source of truth);
//   * the draft-vs-saved document diff the console's "Draft / Saved" honesty
//     rides on — positional, like the document's rows, and semantic (blank
//     gain is unity; "-3.0" is -3; whitespace never counts as an edit).
//
// The API this models is DOCUMENT-LEVEL: gain/mute/include edits live in the
// local draft until the whole document is saved with `If-Match` (see
// AudioPage). Nothing here touches the network or the engine path.
import type { AudioFormState, AudioRouteRow } from '../resources/audioForms';
import type { AudioMeterSample } from '../realtime/useAudioMeter';

/** Fader bounds in dB — the approved broadcast direction (0 dB at the TOP). */
export const GAIN_FADER_MIN_DB = -60;
export const GAIN_FADER_MAX_DB = 0;

/** The tap-nudge / PageUp-PageDown steps in dB (designer round 4: ±1 / ±6). */
export const GAIN_NUDGE_STEPS: readonly number[] = [1, 6];

/** The scale ticks, rendered beside the fader (all inside the fader range). */
export const GAIN_SCALE_TICKS_DB: readonly number[] = [0, -5, -10, -20, -30, -40, -60];

/** Parse a draft gain exactly like the form's validator: blank is unity (0),
 * otherwise a finite number within the wire's f32 magnitude. */
export function parseGainDb(value: string): number | undefined {
  const trimmed = value.trim();
  if (trimmed === '') {
    return 0;
  }
  const parsed = Number(trimmed);
  // The wire type is an f32 (see audioForms.parseGain): reject magnitudes
  // that would overflow server-side even though they stay finite in JS.
  const F32_MAX = 3.4028235e38;
  return Number.isFinite(parsed) && Math.abs(parsed) <= F32_MAX ? parsed : undefined;
}

/** Format a dB number minimally for readouts ("-3", "1.5", "0"). */
export function formatDb(value: number): string {
  return String(Number(value.toFixed(2)));
}

/** Nudge the DRAFT gain by an exact delta. No silent clamping: the readout
 * and the saved document carry the exact value; the fader only pins
 * visually at its ends for out-of-range drafts. */
export function nudgeGainDb(current: string, delta: number): string {
  const base = parseGainDb(current) ?? 0;
  return formatDb(base + delta);
}

/** The fader's displayed value: the draft clamped into the fader's bounds. */
export function faderDisplayDb(db: number): number {
  return Math.min(GAIN_FADER_MAX_DB, Math.max(GAIN_FADER_MIN_DB, db));
}

/** The knob/tick/readout position (0..1) for a dB value — ONE mapping shared
 * by everything that renders the fader (min at the BOTTOM, max at the TOP). */
export function faderPos(db: number): number {
  const clamped = faderDisplayDb(db);
  return (clamped - GAIN_FADER_MIN_DB) / (GAIN_FADER_MAX_DB - GAIN_FADER_MIN_DB);
}

/** Trimmed-string equality for text fields (whitespace is never an edit). */
function textEqual(a: string, b: string): boolean {
  return a.trim() === b.trim();
}

/** How one route row's draft differs from the saved row at the same position. */
export interface RouteDraftDiff {
  /** The route's position in the document (rows are positional edits). */
  readonly index: number;
  /** True when no saved route exists at this position (a newly added route). */
  readonly isNew: boolean;
  /** Any field differs (isNew implies changed). */
  readonly changed: boolean;
  /** The gain differs (semantically: "-3.0" vs "-3" does not). */
  readonly gainChanged: boolean;
  /** The program-bus mute differs. */
  readonly muteChanged: boolean;
  /** Program-bus membership differs. */
  readonly includeChanged: boolean;
}

/** The whole document's draft-vs-saved diff. */
export interface DocumentDraftDiff {
  /** One entry per DRAFT route, index-aligned with the draft's rows. */
  readonly routes: readonly RouteDraftDiff[];
  /** The sample rate differs (semantically: "048000" does not count). */
  readonly sampleRateChanged: boolean;
  /** How many route rows carry draft edits (new rows included). */
  readonly changedRouteCount: number;
  /** True when anything in the document differs from what is saved. */
  readonly anyChanged: boolean;
}

/** Route-level semantic equality: text trimmed, gain parsed, enums compared. */
function routeDiffAt(saved: AudioRouteRow | undefined, draft: AudioRouteRow, index: number): RouteDraftDiff {
  if (saved === undefined) {
    return { index, isNew: true, changed: true, gainChanged: true, muteChanged: true, includeChanged: true };
  }
  const savedGain = parseGainDb(saved.gainDb);
  const draftGain = parseGainDb(draft.gainDb);
  const gainChanged =
    savedGain === undefined || draftGain === undefined ? saved.gainDb.trim() !== draft.gainDb.trim() : savedGain !== draftGain;
  const muteChanged = saved.mute !== draft.mute;
  const includeChanged = saved.includeInProgramBus !== draft.includeInProgramBus;
  const changed =
    gainChanged ||
    muteChanged ||
    includeChanged ||
    !textEqual(saved.inputId, draft.inputId) ||
    saved.channels !== draft.channels ||
    !textEqual(saved.targetTrack, draft.targetTrack) ||
    !textEqual(saved.language, draft.language) ||
    !textEqual(saved.title, draft.title);
  return { index, isNew: false, changed, gainChanged, muteChanged, includeChanged };
}

/** Parse a positive-integer sample rate like the form does ('' / garbage → NaN). */
function parseSampleRate(value: string): number {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) {
    return Number.NaN;
  }
  return Number.parseInt(trimmed, 10);
}

/** Diff the operator's DRAFT against the document currently saved on the
 * server. Both sides come from the same form model, so the comparison is
 * semantic: whitespace and formatting never count as edits. */
export function documentDraftDiff(saved: AudioFormState, draft: AudioFormState): DocumentDraftDiff {
  const routes = draft.routes.map((route, index) => routeDiffAt(saved.routes[index], route, index));
  const savedRate = parseSampleRate(saved.sampleRateHz);
  const draftRate = parseSampleRate(draft.sampleRateHz);
  const sampleRateChanged =
    Number.isNaN(savedRate) || Number.isNaN(draftRate)
      ? saved.sampleRateHz.trim() !== draft.sampleRateHz.trim()
      : savedRate !== draftRate;
  const changedRouteCount = routes.filter((route) => route.changed).length;
  return {
    routes,
    sampleRateChanged,
    changedRouteCount,
    anyChanged: changedRouteCount > 0 || sampleRateChanged,
  };
}

/** Find the REAL `audio.meter` telemetry sample for a route's input id (the
 * pre-fader input meters). No telemetry is ever synthesized from gain or mute. */
export function inputMeterSample(
  inputs: readonly AudioMeterSample[],
  inputId: string,
): AudioMeterSample | undefined {
  return inputs.find((input) => input.input_id === inputId);
}

/** The loudest per-channel peak of a real input sample — input presence, in
 * dBFS — or `undefined` when the sample carries no channels (never invented). */
export function inputPeakDb(sample: AudioMeterSample): number | undefined {
  if (sample.peak_db.length === 0) {
    return undefined;
  }
  return Math.max(...sample.peak_db);
}
