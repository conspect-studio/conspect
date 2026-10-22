// Pure form-state <-> config-body mapping for the Sources / Outputs / Overlays
// management forms.
//
// This module is framework-free (no React, no Lingui) so the mapping and the
// validation can be unit-tested in isolation. The body shapes produced here
// mirror the Rust config schema EXACTLY:
//   * `Source` / `SourceKind` / `RtspOptions` / `SourceAuth` / `ColorOverride`
//     / `CaptionSelector` / `SourceWallClock`
//     (crates/multiview-config/src/schema.rs),
//   * `Output` and its per-kind fields (schema.rs), `OutputAudio` (audio.rs),
//   * `DevicePin` (placement.rs),
//   * `Overlay` (schema.rs) with the kind params the Rust side actually
//     consumes/documents: the clock face/tz/placement read by
//     multiview-cli's `analog_clock_from_config`, and the tally_border
//     width_px/color/binding from the shipped examples + template reference.
// The control plane validates POST/PUT bodies against those types with 422
// (ADR-W015), so a wrong body is a rejected body. DO NOT add fields that do
// not exist in schema.rs.
//
// Validation returns machine codes per field; the pages translate codes into
// localized messages (Lingui stays in the components).

import type { OutputKind, OverlayKind, ProbeKind, ResourceRecord, SourceKind } from './types';

// --- shared helpers ----------------------------------------------------------

/** The GPU vendor families a `DevicePin` names (placement.rs `PinVendor`). */
export type PinVendor = 'nvidia' | 'intel' | 'amd' | 'apple';

/** All pin vendors, for building selectors. */
export const PIN_VENDORS: readonly PinVendor[] = ['nvidia', 'intel', 'amd', 'apple'];

/** `#RGB` or `#RRGGBB` hex colour (schema `Solid.color` / border colour). */
const HEX_COLOR_RE = /^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;

/** A whole-number string (optionally signed), with no float/exponent forms. */
const INT_RE = /^-?\d+$/;

/** Minimum/maximum clock timezone offset in minutes: −12:00 … +14:00. */
export const CLOCK_TZ_MIN_MINUTES = -720;
export const CLOCK_TZ_MAX_MINUTES = 840;

/** The machine validation codes a field can fail with. */
export type FormErrorCode =
  | 'required'
  | 'url-invalid'
  | 'scheme-rtsp'
  | 'scheme-srt'
  | 'scheme-rist'
  | 'scheme-rtmp'
  | 'scheme-http'
  | 'hex-color'
  | 'int'
  | 'int-range'
  | 'positive-int'
  | 'number'
  | 'zone-extent'
  | 'mount-slash'
  | 'tracks-required'
  | 'finite-number'
  | 'duplicate-track'
  | 'duplicate-input'
  | 'reserved-track'
  | 'program-bus-muted'
  | 'rational-fps'
  | 'timezone'
  | 'time-of-day'
  | 'date-time'
  | 'members-required'
  | 'duplicate-member';

/** Per-field validation errors keyed by form-state field name. */
export type FieldErrors<Field extends string> = Partial<Record<Field, FormErrorCode>>;

/** Parse a strict integer string, or `undefined` when not a whole number. */
export function parseIntStrict(value: string): number | undefined {
  const trimmed = value.trim();
  if (!INT_RE.test(trimmed)) {
    return undefined;
  }
  const parsed = Number.parseInt(trimmed, 10);
  return Number.isSafeInteger(parsed) ? parsed : undefined;
}

/** A plain decimal number string (optionally signed; no exponent forms). */
const NUMBER_RE = /^-?\d+(\.\d+)?$/;

/** Parse a strict finite decimal string, or `undefined` when not a number. */
function parseNumberStrict(value: string): number | undefined {
  const trimmed = value.trim();
  if (!NUMBER_RE.test(trimmed)) {
    return undefined;
  }
  const parsed = Number.parseFloat(trimmed);
  return Number.isFinite(parsed) ? parsed : undefined;
}

/**
 * Whether `value` is a well-formed URL whose scheme is in `schemes` (any
 * parseable scheme when `schemes` is `undefined`) and that carries a host.
 * Bracketed IPv6 literal hosts (`rtsp://[2001:db8::1]:8554/…`) parse fine; an
 * UNbracketed IPv6 literal fails (its colons read as an invalid port), which is
 * exactly the rejection we want (conventions §10 — bracket IPv6 URL literals).
 */
export function isValidUrl(value: string, schemes?: readonly string[]): boolean {
  return urlErrorCode(value, schemes, 'url-invalid') === undefined;
}

/**
 * Validate a URL field: `required` when blank, `url-invalid` when unparseable
 * or host-less, `schemeCode` when parseable but on the wrong scheme.
 */
export function urlErrorCode(
  value: string,
  schemes: readonly string[] | undefined,
  schemeCode: FormErrorCode,
): FormErrorCode | undefined {
  const trimmed = value.trim();
  if (trimmed === '') {
    return 'required';
  }
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return 'url-invalid';
  }
  const scheme = parsed.protocol.endsWith(':')
    ? parsed.protocol.slice(0, -1).toLowerCase()
    : parsed.protocol.toLowerCase();
  if (schemes !== undefined && !schemes.includes(scheme)) {
    return schemeCode;
  }
  if (parsed.host === '') {
    return 'url-invalid';
  }
  return undefined;
}

/**
 * The IANA timezone ids offered to the timezone pickers as `<datalist>`
 * suggestions, sorted. `UTC` always leads (the universal, DST-free default, an
 * alias `Intl.supportedValuesOf` omits but `chrono-tz` accepts — ADR-0047), then
 * the host-discovered zones via `Intl.supportedValuesOf('timeZone')` (Node 18+ /
 * modern browsers). Empty when the runtime exposes no tz database; the field
 * still works as free text validated by {@link isKnownTimezone} + the server.
 */
export const IANA_TIMEZONES: readonly string[] = ((): readonly string[] => {
  // `Intl.supportedValuesOf` is ES2022; older runtimes (or a polyfilled jsdom)
  // may not expose it, so probe before calling and degrade to free text.
  if (typeof Intl.supportedValuesOf !== 'function') {
    return [];
  }
  try {
    const zones = Intl.supportedValuesOf('timeZone')
      .filter((zone) => zone !== 'UTC')
      .sort((a, b) => a.localeCompare(b));
    return ['UTC', ...zones];
  } catch {
    return [];
  }
})();

/**
 * Whether `id` is a timezone this runtime recognizes. Probes the platform tz
 * database via `Intl.DateTimeFormat` (which canonicalizes — it accepts both
 * `UTC` and every IANA id, throwing `RangeError` for an unknown zone), matching
 * the set `chrono-tz` resolves server-side. This is a best-effort early catch:
 * the server remains the authority (an unknown zone is a typed
 * `ConfigError`/422, surfaced inline — ADR-0047 §5.2).
 */
export function isKnownTimezone(id: string): boolean {
  const trimmed = id.trim();
  if (trimmed === '') {
    return false;
  }
  try {
    // Throws RangeError for an invalid `timeZone` option; canonicalizes valid
    // ids (including the `UTC` alias the supported-values list omits).
    new Intl.DateTimeFormat(undefined, { timeZone: trimmed });
    return true;
  } catch {
    return false;
  }
}

/** `HH:MM:SS` (24-hour) time-of-day, the shape the timer `at` field carries. */
const TIME_OF_DAY_RE = /^([01]\d|2[0-3]):[0-5]\d:[0-5]\d$/;

/** Whether `value` is a valid `HH:MM:SS` 24-hour time-of-day (timer.rs `parse_hms`). */
export function isValidTimeOfDay(value: string): boolean {
  return TIME_OF_DAY_RE.test(value.trim());
}

/**
 * Whether `value` is a valid `YYYY-MM-DDTHH:MM:SS` local date+time — the shape
 * `TimerTarget::DateTime` parses (timer.rs `parse_naive_datetime`). The calendar
 * is checked (month length / leap years) by reconstructing the date, so an
 * impossible day (e.g. `2026-02-30`) is rejected, never silently accepted.
 */
export function isValidLocalDateTime(value: string): boolean {
  const m = /^(\d{4})-(\d{2})-(\d{2})[T ]([01]\d|2[0-3]):([0-5]\d):([0-5]\d)$/.exec(value.trim());
  if (m === null) {
    return false;
  }
  const [year, month, day, hour, minute, second] = [
    Number(m[1]),
    Number(m[2]),
    Number(m[3]),
    Number(m[4]),
    Number(m[5]),
    Number(m[6]),
  ];
  if (month < 1 || month > 12 || day < 1 || day > 31) {
    return false;
  }
  // Reconstruct in UTC and confirm the calendar fields survive — rejects
  // overflow days like 2026-02-30 / 2026-04-31 that the regex alone allows.
  const date = new Date(Date.UTC(year, month - 1, day, hour, minute, second));
  return (
    date.getUTCFullYear() === year &&
    date.getUTCMonth() === month - 1 &&
    date.getUTCDate() === day
  );
}

/** Type guard: a non-null, non-array object (a plain record). */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Narrow an unknown to a plain record without an unsafe assertion. */
function asRecord(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}

function asFiniteNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

/** A finite number rendered back to its form-input string ('' when absent). */
function numberToField(value: number | undefined): string {
  return value === undefined ? '' : String(value);
}

function asPinVendor(value: unknown): PinVendor {
  return PIN_VENDORS.find((vendor) => vendor === value) ?? 'nvidia';
}

/**
 * The body keys NOT managed by a form, preserved verbatim across an edit.
 *
 * Keys land via `Object.defineProperty`, never plain assignment: a stored body
 * can carry an OWN `__proto__` key (JSON allows it), and `extra[key] = value`
 * for that key would swap the accumulator's prototype and silently drop the
 * key. `defineProperty` always creates an own data property, and the later
 * body spreads (`{ ...form.extra }`) copy it back as an own data property too
 * (spread uses CreateDataProperty, never the inherited setter).
 */
export function extraOf(
  body: Record<string, unknown>,
  managedKeys: readonly string[],
): Readonly<Record<string, unknown>> {
  const extra: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(body)) {
    if (!managedKeys.includes(key)) {
      Object.defineProperty(extra, key, {
        value,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    }
  }
  return extra;
}

// --- Sources ------------------------------------------------------------------

/** The user-pickable source kinds (no legacy `test` alias). */
export type SourceFormKind = Exclude<SourceKind, 'test'>;

/** The clock-face styles (schema `ClockFaceConfig`, snake_case). */
export type ClockFace = 'analog' | 'digital' | 'dual';

/**
 * All clock faces, for building selectors. `dual` (ADR-0047) is an analogue
 * face with a digital readout beneath it.
 */
export const CLOCK_FACES: readonly ClockFace[] = ['analog', 'digital', 'dual'];

/**
 * The clock faces a `[[overlays]]` clock overlay supports — `analog`/`digital`
 * only (the overlay run path renders a single face; `dual` is a source-only
 * placement). Kept distinct from {@link CLOCK_FACES} so the source picker can
 * offer `dual` without leaking it into the overlay form.
 */
export const OVERLAY_CLOCK_FACES: readonly ClockFace[] = ['analog', 'digital'];

/** The timer target kinds (schema `TimerTarget`, tagged on `target`). */
export type TimerTargetType = 'time_of_day' | 'date_time';

/** Both timer target kinds, for building selectors. */
export const TIMER_TARGET_TYPES: readonly TimerTargetType[] = ['time_of_day', 'date_time'];

/** Timer count direction (schema `TimerDirection`, snake_case). */
export type TimerDirection = 'down' | 'up';

/** Both timer directions, for building selectors. */
export const TIMER_DIRECTIONS: readonly TimerDirection[] = ['down', 'up'];

/** At/after-target behaviour (schema `TimerOnTarget`, snake_case). */
export type TimerOnTarget = 'hold' | 'continue' | 'zero_then_up' | 'recur';

/** All at-target behaviours, for building selectors. */
export const TIMER_ON_TARGETS: readonly TimerOnTarget[] = [
  'hold',
  'continue',
  'zero_then_up',
  'recur',
];

/** Timer display format (schema `TimerFormat`, snake_case). */
export type TimerFormat = 'd_hh_mm_ss' | 'hh_mm_ss' | 'mm_ss' | 'hh_mm_ss_ff' | 'auto';

/** All timer formats, for building selectors. */
export const TIMER_FORMATS: readonly TimerFormat[] = [
  'd_hh_mm_ss',
  'hh_mm_ss',
  'mm_ss',
  'hh_mm_ss_ff',
  'auto',
];

/** RTSP lower-transport options (schema `RtspOptions.transport`); '' = default. */
export type RtspTransport = '' | 'tcp' | 'udp';

/** The caption-selector modes (schema `CaptionSelector`), plus 'none' = omit. */
export type CaptionsMode =
  | 'none'
  | 'auto'
  | 'off'
  | 'teletext_page'
  | 'track'
  | 'embedded_cc'
  | 'sidecar';

/** All caption modes in display order. */
export const CAPTION_MODES: readonly CaptionsMode[] = [
  'none',
  'auto',
  'off',
  'teletext_page',
  'track',
  'embedded_cc',
  'sidecar',
];

/** The wall-clock verb (schema `WallClockUse`), plus 'default' = omit. */
export type WallClockChoice = 'default' | 'use' | 'discard';

/** The editable state behind the source form (numbers kept as input strings). */
export interface SourceFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: SourceFormKind;
  /** `url` for rtsp/hls/youtube/ts/srt/rtmp. */
  readonly url: string;
  /** NDI source name (`ndi`). */
  readonly ndiName: string;
  /** Filesystem path (`file`). */
  readonly path: string;
  /** Fill colour (`solid`). */
  readonly color: string;
  /** Clock face (`clock`): analog / digital / dual. */
  readonly clockFace: ClockFace;
  /** 12-hour mode (`clock`). */
  readonly clockTwelveHour: boolean;
  /** IANA timezone id (`clock`, '' = use the fixed offset). Wins over offset. */
  readonly clockTimezone: string;
  /** Timezone offset in minutes (`clock`; ignored when `clockTimezone` is set). */
  readonly clockTzMinutes: string;
  /** Location/title label drawn on the face (`clock`, '' = none). */
  readonly clockLabel: string;
  /** Draw a `UTC±HH:MM` offset badge (`clock`). */
  readonly clockShowOffset: boolean;
  /** Draw the disciplined-reference (PTP/NTP/SYS) badge (`clock`). */
  readonly clockShowReference: boolean;
  /** Draw hour numerals on the analogue / dual face (`clock`). */
  readonly clockNumerals: boolean;
  /** Timer target kind (`timer`): time-of-day vs date+time. */
  readonly timerTargetType: TimerTargetType;
  /** Target `at` string (`timer`): `HH:MM:SS` (time-of-day) or `YYYY-MM-DDTHH:MM:SS`. */
  readonly timerAt: string;
  /** IANA timezone id (`timer`, '' = use the fixed offset). Wins over offset. */
  readonly timerTimezone: string;
  /** Timezone offset in minutes (`timer`; ignored when `timerTimezone` is set). */
  readonly timerTzMinutes: string;
  /** Re-arm to the next day each day (`timer`, time-of-day target only). */
  readonly timerRecurDaily: boolean;
  /** Count direction (`timer`): down to / up from the target. */
  readonly timerDirection: TimerDirection;
  /** At/after-target behaviour (`timer`). */
  readonly timerOnTarget: TimerOnTarget;
  /** Display format (`timer`). */
  readonly timerFormat: TimerFormat;
  /** Operator label drawn with the count (`timer`, '' = none). */
  readonly timerLabel: string;
  /** Overrun prefix override (`timer`, '' = the default `+`). */
  readonly timerOverrunPrefix: string;
  /** Draw the overrun a11y badge (`OVER`/`ELAPSED`) past the target (`timer`). */
  readonly timerOverrunBadge: boolean;
  /** RTSP lower transport ('' = engine default, omitted from the body). */
  readonly rtspTransport: RtspTransport;
  /** Caption selector mode ('none' = no `captions` block). */
  readonly captionsMode: CaptionsMode;
  /** Teletext page (mode `teletext_page`). */
  readonly captionsPage: string;
  /** Track id / language tag (mode `track`). */
  readonly captionsTrack: string;
  /** CEA-608/708 field/service (mode `embedded_cc`). */
  readonly captionsField: string;
  /** Sidecar file path (mode `sidecar`). */
  readonly captionsPath: string;
  /** Whether a `color_override` block is written. */
  readonly colorOverrideEnabled: boolean;
  readonly colorPrimaries: string;
  readonly colorTransfer: string;
  readonly colorMatrix: string;
  readonly colorRange: string;
  /** Whether a `gpu_pin` block is written. */
  readonly gpuPinEnabled: boolean;
  readonly gpuPinVendor: PinVendor;
  readonly gpuPinStableId: string;
  /** Wall-clock verb ('default' = no `wallclock` block). */
  readonly wallclock: WallClockChoice;
  /** `auth.secret_ref` ('' = no `auth` block; never a plaintext secret). */
  readonly authSecretRef: string;
  /**
   * WHIP publisher bearer token (`webrtc` source; '' = omit ⇒ publishing needs
   * a Write API key, ADR-T014). A config-secret: masked in the UI, never echoed.
   */
  readonly webrtcToken: string;
  /**
   * Whether the `webrtc` source's SDP answer accepts the publisher's Opus audio
   * (schema default true). `false` answers the audio m-line `inactive`.
   */
  readonly webrtcAudio: boolean;
  /** Unmanaged body fields preserved verbatim across an edit. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The source-form fields that can carry a validation error. */
export type SourceField =
  | 'id'
  | 'name'
  | 'url'
  | 'ndiName'
  | 'path'
  | 'color'
  | 'clockTzMinutes'
  | 'clockTimezone'
  | 'timerAt'
  | 'timerTimezone'
  | 'timerTzMinutes'
  | 'captionsPage'
  | 'captionsTrack'
  | 'captionsField'
  | 'captionsPath'
  | 'gpuPinStableId';

/** Default fill colour for a fresh `solid` source. */
export const SOLID_DEFAULT_COLOR = '#101014';

/** A fresh, empty source form. */
export function emptySourceForm(): SourceFormState {
  return {
    id: '',
    name: '',
    kind: 'rtsp',
    url: '',
    ndiName: '',
    path: '',
    color: SOLID_DEFAULT_COLOR,
    clockFace: 'analog',
    clockTwelveHour: false,
    clockTimezone: '',
    clockTzMinutes: '0',
    clockLabel: '',
    clockShowOffset: false,
    clockShowReference: false,
    clockNumerals: false,
    timerTargetType: 'time_of_day',
    timerAt: '',
    timerTimezone: '',
    timerTzMinutes: '0',
    timerRecurDaily: false,
    timerDirection: 'down',
    timerOnTarget: 'hold',
    timerFormat: 'd_hh_mm_ss',
    timerLabel: '',
    timerOverrunPrefix: '',
    timerOverrunBadge: true,
    rtspTransport: '',
    captionsMode: 'none',
    captionsPage: '',
    captionsTrack: '',
    captionsField: '',
    captionsPath: '',
    colorOverrideEnabled: false,
    colorPrimaries: 'auto',
    colorTransfer: 'auto',
    colorMatrix: 'auto',
    colorRange: 'auto',
    gpuPinEnabled: false,
    gpuPinVendor: 'nvidia',
    gpuPinStableId: '',
    wallclock: 'default',
    authSecretRef: '',
    webrtcToken: '',
    webrtcAudio: true,
    extra: {},
  };
}

/** The body keys the source form manages (everything else is preserved). */
const SOURCE_MANAGED_KEYS: readonly string[] = [
  'id',
  'display_name',
  'kind',
  'url',
  'name',
  'path',
  'color',
  'face',
  'twelve_hour',
  'timezone',
  'tz_offset_minutes',
  'label',
  'show_offset',
  'show_reference',
  'numerals',
  // Timer keys: the `target` discriminator + the flattened `TimerTarget`
  // payload (`at` / `recur_daily`) and the timer's own fields.
  'target',
  'at',
  'recur_daily',
  'direction',
  'on_target',
  'format',
  'overrun_prefix',
  'overrun_badge',
  'rtsp',
  'auth',
  'color_override',
  'captions',
  'gpu_pin',
  'wallclock',
  // `webrtc` (WHIP ingest) source keys.
  'token',
  'audio',
];

/** Whether a source kind's locator is its `url`. */
export function sourceKindHasUrl(kind: SourceFormKind): boolean {
  return (
    kind === 'rtsp' ||
    kind === 'hls' ||
    kind === 'youtube' ||
    kind === 'ts' ||
    kind === 'srt' ||
    kind === 'rist' ||
    kind === 'rtmp'
  );
}

/**
 * Switch a source form to a new kind. The shared locator value is kept when
 * moving between url kinds (a UX nicety); kind-scoped options reset so a stale
 * payload can never leak into the body (the body writer is kind-exact anyway).
 */
export function withSourceKind(form: SourceFormState, kind: SourceFormKind): SourceFormState {
  if (kind === form.kind) {
    return form;
  }
  return {
    ...form,
    kind,
    rtspTransport: '',
    color: form.color.trim() === '' ? SOLID_DEFAULT_COLOR : form.color,
  };
}

/**
 * Switch a timer form's target kind. Moving to a `date_time` target drops the
 * time-of-day-only state (`recur_daily`, and the `recur` at-target policy that
 * the schema only honours for a recurring time-of-day target — ADR-0047) so the
 * body can never carry an inert/invalid combination. The `at` string resets
 * because the two target shapes are different (`HH:MM:SS` vs a full date+time).
 */
export function withTimerTargetType(
  form: SourceFormState,
  targetType: TimerTargetType,
): SourceFormState {
  if (targetType === form.timerTargetType) {
    return form;
  }
  return {
    ...form,
    timerTargetType: targetType,
    timerAt: '',
    timerRecurDaily: targetType === 'time_of_day' ? form.timerRecurDaily : false,
    timerOnTarget:
      targetType === 'date_time' && form.timerOnTarget === 'recur' ? 'hold' : form.timerOnTarget,
  };
}

/** Build the exact config `Source` body from a valid form. */
export function sourceFormToBody(form: SourceFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.id = form.id.trim();
  const displayName = form.name.trim();
  if (displayName !== '') {
    body.display_name = displayName;
  }
  body.kind = form.kind;
  switch (form.kind) {
    case 'bars':
      break;
    case 'solid':
      body.color = form.color.trim();
      break;
    case 'clock': {
      body.face = form.clockFace;
      body.twelve_hour = form.clockTwelveHour;
      const tz = form.clockTimezone.trim();
      if (tz !== '') {
        // An IANA zone wins over the fixed offset (DST-correct, ADR-0047 §5.2);
        // the offset is then omitted so the body reflects the chosen mode.
        body.timezone = tz;
      } else {
        body.tz_offset_minutes = parseIntStrict(form.clockTzMinutes) ?? 0;
      }
      const label = form.clockLabel.trim();
      if (label !== '') {
        body.label = label;
      }
      body.show_offset = form.clockShowOffset;
      body.show_reference = form.clockShowReference;
      body.numerals = form.clockNumerals;
      break;
    }
    case 'timer': {
      body.target = form.timerTargetType;
      body.at = form.timerAt.trim();
      const tz = form.timerTimezone.trim();
      if (tz !== '') {
        body.timezone = tz;
      } else {
        body.tz_offset_minutes = parseIntStrict(form.timerTzMinutes) ?? 0;
      }
      // `recur_daily` is a time-of-day-only field (the schema ignores it on a
      // date_time target); write it only there so the body stays kind-exact.
      if (form.timerTargetType === 'time_of_day') {
        body.recur_daily = form.timerRecurDaily;
      }
      body.direction = form.timerDirection;
      body.on_target = form.timerOnTarget;
      body.format = form.timerFormat;
      const label = form.timerLabel.trim();
      if (label !== '') {
        body.label = label;
      }
      const prefix = form.timerOverrunPrefix;
      if (prefix.trim() !== '') {
        body.overrun_prefix = prefix;
      }
      body.overrun_badge = form.timerOverrunBadge;
      break;
    }
    case 'rtsp':
      body.url = form.url.trim();
      if (form.rtspTransport !== '') {
        body.rtsp = { transport: form.rtspTransport };
      }
      break;
    // The plain URL kinds. RIST is a URL kind like SRT; its optional typed
    // `rist` options block (when present in the stored body) round-trips through
    // `form.extra` until a dedicated options sub-form lands (ADR-0095).
    case 'hls':
    case 'youtube':
    case 'ts':
    case 'srt':
    case 'rist':
    case 'rtmp':
      body.url = form.url.trim();
      break;
    case 'ndi':
      body.name = form.ndiName.trim();
      break;
    case 'file':
      body.path = form.path.trim();
      break;
    case 'webrtc': {
      // WHIP ingest (ADR-T014): no `url` to author — the publish endpoint is
      // derived from the id. A non-empty token gates publishing; an empty one
      // is omitted (publishing then needs a Write API key). `audio` defaults to
      // true (accept the publisher's Opus), so only an explicit `false` is
      // written — matching the schema's `skip_serializing_if = is_true`.
      const token = form.webrtcToken.trim();
      if (token !== '') {
        body.token = token;
      }
      if (!form.webrtcAudio) {
        body.audio = false;
      }
      break;
    }
  }
  if (form.authSecretRef.trim() !== '') {
    body.auth = { secret_ref: form.authSecretRef.trim() };
  }
  if (form.colorOverrideEnabled) {
    body.color_override = {
      primaries: form.colorPrimaries.trim() === '' ? 'auto' : form.colorPrimaries.trim(),
      transfer: form.colorTransfer.trim() === '' ? 'auto' : form.colorTransfer.trim(),
      matrix: form.colorMatrix.trim() === '' ? 'auto' : form.colorMatrix.trim(),
      range: form.colorRange.trim() === '' ? 'auto' : form.colorRange.trim(),
    };
  }
  const captions = captionsBody(form);
  if (captions !== undefined) {
    body.captions = captions;
  }
  if (form.gpuPinEnabled) {
    body.gpu_pin = { vendor: form.gpuPinVendor, stable_id: form.gpuPinStableId.trim() };
  }
  if (form.wallclock !== 'default') {
    body.wallclock = { use: form.wallclock };
  }
  return body;
}

/** The internally-tagged `CaptionSelector` payload, or `undefined` to omit. */
function captionsBody(form: SourceFormState): Record<string, unknown> | undefined {
  switch (form.captionsMode) {
    case 'none':
      return undefined;
    case 'auto':
    case 'off':
      return { mode: form.captionsMode };
    case 'teletext_page':
      return { mode: 'teletext_page', page: parseIntStrict(form.captionsPage) ?? 0 };
    case 'track':
      return { mode: 'track', id: form.captionsTrack.trim() };
    case 'embedded_cc':
      return { mode: 'embedded_cc', field: form.captionsField.trim() };
    case 'sidecar':
      return { mode: 'sidecar', path: form.captionsPath.trim() };
  }
}

/**
 * Parse a stored source kind tag onto the editable form kind. The legacy
 * `test` alias canonicalizes to `bars` (the schema does the same); any OTHER
 * unknown/absent tag returns `undefined` — an explicit refusal, never a fold
 * (folding would silently rewrite the authored document on the next save).
 */
export function parseSourceFormKind(tag: string | undefined): SourceFormKind | undefined {
  if (tag === 'test') {
    return 'bars';
  }
  return (
    [
      'bars',
      'solid',
      'clock',
      'timer',
      'rtsp',
      'hls',
      'youtube',
      'ts',
      'srt',
      'rist',
      'rtmp',
      'ndi',
      'file',
      'webrtc',
    ] as const
  ).find((k) => k === tag);
}

/**
 * Project a stored record back onto the editable source form, or `undefined`
 * when the body's kind is not one this UI can edit (the page disables Edit;
 * the document is preserved as authored).
 */
export function sourceFormFromRecord(record: ResourceRecord): SourceFormState | undefined {
  const body = record.body;
  const empty = emptySourceForm();
  const kind = parseSourceFormKind(asString(body.kind));
  if (kind === undefined) {
    return undefined;
  }

  const rtsp = asRecord(body.rtsp);
  const transport = asString(rtsp?.transport);
  const auth = asRecord(body.auth);
  const colorOverride = asRecord(body.color_override);
  const captions = asRecord(body.captions);
  const gpuPin = asRecord(body.gpu_pin);
  const wallclock = asRecord(body.wallclock);
  const wallclockUse = asString(wallclock?.use);

  const captionsMode: CaptionsMode =
    captions === undefined
      ? 'none'
      : (CAPTION_MODES.find((mode) => mode === asString(captions.mode)) ?? 'none');

  return {
    ...empty,
    id: record.id,
    // An authored `display_name` wins over the store name: the form writes its
    // Name back as `display_name`, so seeding from the store name would
    // clobber a differing authored value on the first save.
    name: asString(body.display_name) ?? record.name,
    kind,
    url: asString(body.url) ?? '',
    ndiName: kind === 'ndi' ? (asString(body.name) ?? '') : '',
    path: asString(body.path) ?? '',
    color: asString(body.color) ?? SOLID_DEFAULT_COLOR,
    clockFace: CLOCK_FACES.find((face) => face === asString(body.face)) ?? 'analog',
    clockTwelveHour: body.twelve_hour === true,
    clockTimezone: kind === 'clock' ? (asString(body.timezone) ?? '') : '',
    clockTzMinutes: numberToField(asFiniteNumber(body.tz_offset_minutes)) || '0',
    clockLabel: kind === 'clock' ? (asString(body.label) ?? '') : '',
    clockShowOffset: kind === 'clock' && body.show_offset === true,
    clockShowReference: kind === 'clock' && body.show_reference === true,
    clockNumerals: kind === 'clock' && body.numerals === true,
    timerTargetType:
      TIMER_TARGET_TYPES.find((tt) => tt === asString(body.target)) ?? 'time_of_day',
    timerAt: kind === 'timer' ? (asString(body.at) ?? '') : '',
    timerTimezone: kind === 'timer' ? (asString(body.timezone) ?? '') : '',
    timerTzMinutes:
      kind === 'timer' ? numberToField(asFiniteNumber(body.tz_offset_minutes)) || '0' : '0',
    timerRecurDaily: kind === 'timer' && body.recur_daily === true,
    timerDirection: TIMER_DIRECTIONS.find((d) => d === asString(body.direction)) ?? 'down',
    timerOnTarget: TIMER_ON_TARGETS.find((o) => o === asString(body.on_target)) ?? 'hold',
    timerFormat: TIMER_FORMATS.find((f) => f === asString(body.format)) ?? 'd_hh_mm_ss',
    timerLabel: kind === 'timer' ? (asString(body.label) ?? '') : '',
    timerOverrunPrefix: kind === 'timer' ? (asString(body.overrun_prefix) ?? '') : '',
    // `overrun_badge` defaults to true on the schema (`default_true`); an absent
    // field means "draw it", so only an explicit `false` turns it off.
    timerOverrunBadge: kind === 'timer' ? body.overrun_badge !== false : true,
    rtspTransport: transport === 'tcp' || transport === 'udp' ? transport : '',
    captionsMode,
    captionsPage: numberToField(asFiniteNumber(captions?.page)),
    captionsTrack: asString(captions?.id) ?? '',
    captionsField: asString(captions?.field) ?? '',
    captionsPath: asString(captions?.path) ?? '',
    colorOverrideEnabled: colorOverride !== undefined,
    colorPrimaries: asString(colorOverride?.primaries) ?? 'auto',
    colorTransfer: asString(colorOverride?.transfer) ?? 'auto',
    colorMatrix: asString(colorOverride?.matrix) ?? 'auto',
    colorRange: asString(colorOverride?.range) ?? 'auto',
    gpuPinEnabled: gpuPin !== undefined,
    gpuPinVendor: asPinVendor(gpuPin?.vendor),
    gpuPinStableId: asString(gpuPin?.stable_id) ?? '',
    wallclock: wallclockUse === 'use' || wallclockUse === 'discard' ? wallclockUse : 'default',
    authSecretRef: asString(auth?.secret_ref) ?? '',
    webrtcToken: kind === 'webrtc' ? (asString(body.token) ?? '') : '',
    // The schema default accepts publisher audio (`audio: true`); an absent key
    // means "accept", so only an explicit `false` clears the toggle.
    webrtcAudio: kind === 'webrtc' ? body.audio !== false : true,
    extra: extraOf(body, SOURCE_MANAGED_KEYS),
  };
}

/** Validate a source form, returning per-field machine codes. */
export function validateSourceForm(
  form: SourceFormState,
  creating: boolean,
): FieldErrors<SourceField> {
  const errors: FieldErrors<SourceField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  switch (form.kind) {
    case 'rtsp': {
      const code = urlErrorCode(form.url, ['rtsp', 'rtsps'], 'scheme-rtsp');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'srt': {
      const code = urlErrorCode(form.url, ['srt'], 'scheme-srt');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'rist': {
      const code = urlErrorCode(form.url, ['rist'], 'scheme-rist');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'rtmp': {
      const code = urlErrorCode(form.url, ['rtmp', 'rtmps'], 'scheme-rtmp');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'hls':
    case 'youtube': {
      const code = urlErrorCode(form.url, ['http', 'https'], 'scheme-http');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'ts': {
      // MPEG-TS arrives over many transports (udp/rtp/http/…): any parseable
      // URL with a host is accepted — the schema field is an open url string.
      const code = urlErrorCode(form.url, undefined, 'url-invalid');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'ndi':
      if (form.ndiName.trim() === '') {
        errors.ndiName = 'required';
      }
      break;
    case 'file':
      if (form.path.trim() === '') {
        errors.path = 'required';
      }
      break;
    case 'solid':
      if (!HEX_COLOR_RE.test(form.color.trim())) {
        errors.color = 'hex-color';
      }
      break;
    case 'clock': {
      if (form.clockTimezone.trim() !== '') {
        // IANA mode: the zone id is validated; the fixed offset is ignored.
        if (!isKnownTimezone(form.clockTimezone)) {
          errors.clockTimezone = 'timezone';
        }
      } else {
        const tz = parseIntStrict(form.clockTzMinutes);
        if (tz === undefined || tz < CLOCK_TZ_MIN_MINUTES || tz > CLOCK_TZ_MAX_MINUTES) {
          errors.clockTzMinutes = 'int-range';
        }
      }
      break;
    }
    case 'timer': {
      const at = form.timerAt.trim();
      if (at === '') {
        errors.timerAt = 'required';
      } else if (form.timerTargetType === 'time_of_day') {
        if (!isValidTimeOfDay(at)) {
          errors.timerAt = 'time-of-day';
        }
      } else if (!isValidLocalDateTime(at)) {
        errors.timerAt = 'date-time';
      }
      if (form.timerTimezone.trim() !== '') {
        if (!isKnownTimezone(form.timerTimezone)) {
          errors.timerTimezone = 'timezone';
        }
      } else {
        const tz = parseIntStrict(form.timerTzMinutes);
        if (tz === undefined || tz < CLOCK_TZ_MIN_MINUTES || tz > CLOCK_TZ_MAX_MINUTES) {
          errors.timerTzMinutes = 'int-range';
        }
      }
      break;
    }
    case 'webrtc':
      // WHIP ingest authors no locator (the publish endpoint is derived from
      // the id) and the token is optional — nothing kind-specific to validate.
      break;
    case 'bars':
      break;
  }
  switch (form.captionsMode) {
    case 'teletext_page': {
      const page = parseIntStrict(form.captionsPage);
      if (page === undefined || page < 100 || page > 899) {
        errors.captionsPage = 'int-range';
      }
      break;
    }
    case 'track':
      if (form.captionsTrack.trim() === '') {
        errors.captionsTrack = 'required';
      }
      break;
    case 'embedded_cc':
      if (form.captionsField.trim() === '') {
        errors.captionsField = 'required';
      }
      break;
    case 'sidecar':
      if (form.captionsPath.trim() === '') {
        errors.captionsPath = 'required';
      }
      break;
    default:
      break;
  }
  if (form.gpuPinEnabled && form.gpuPinStableId.trim() === '') {
    errors.gpuPinStableId = 'required';
  }
  return errors;
}

// --- Outputs --------------------------------------------------------------------

/** Per-output audio selection ('default' = no `audio` block). */
export type OutputAudioChoice = 'default' | 'program' | 'tracks';

/**
 * The schema default for a `webrtc` (WHEP serve) output's `max_viewers`
 * (ADR-0049). An unchanged value is omitted from the body (absent ≠
 * default-valued).
 */
export const DEFAULT_WEBRTC_MAX_VIEWERS = 8;

/**
 * How a display output's mode is chosen: automatic (EDID preferred +
 * exact-rational cadence match), an explicit EDID `mode` override, or a
 * CVT-RB `forced_mode` for an EDID-less head. Mirrors the config schema's
 * mutually-exclusive `mode` / `forced_mode` fields.
 */
export type DisplayModeChoice = 'auto' | 'override' | 'forced';

/** The editable state behind the output form. */
export interface OutputFormState {
  readonly id: string;
  readonly name: string;
  /** Display kind ('rtsp' ⇒ wire `rtsp_server`, 'll-hls' ⇒ `ll_hls`). */
  readonly kind: OutputKind;
  /** Mount point (`rtsp_server`). */
  readonly mount: string;
  /** Output path (`hls` / `ll_hls`). */
  readonly path: string;
  /** Destination URL (`rtmp` / `srt`). */
  readonly url: string;
  /** Advertised NDI source name (`ndi`). */
  readonly ndiName: string;
  /** KMS connector name, or `auto` for the first connected (`display`). */
  readonly connector: string;
  /** Display mode strategy (`display`): auto / EDID override / CVT-RB forced. */
  readonly displayModeChoice: DisplayModeChoice;
  /** Mode width in pixels (`display`, override/forced). */
  readonly displayModeWidth: string;
  /** Mode height in pixels (`display`, override/forced). */
  readonly displayModeHeight: string;
  /**
   * Mode refresh as an exact rational (`60000/1001`) or a bare integer
   * (`display`, override/forced). Never a float (invariant #3).
   */
  readonly displayModeRefresh: string;
  /** Video codec (all kinds except `ndi`/`display`). An open schema string. */
  readonly codec: string;
  /** Latency profile hint (`rtsp_server`, optional). */
  readonly latencyProfile: string;
  /** LL-HLS part target in ms ('' = omit). */
  readonly partTargetMs: string;
  /** Segment duration in ms ('' = omit; `hls` + `ll_hls`). */
  readonly segmentMs: string;
  /** GOP duration in ms ('' = omit; `ll_hls`). */
  readonly gopMs: string;
  /** Audio selection ('default' = the engine's program-bus default). */
  readonly audioMode: OutputAudioChoice;
  /** Comma-separated selectable track names (mode `tracks`). */
  readonly audioTracks: string;
  /**
   * WHEP viewer cap (`webrtc` output; '' = the schema default 8). A non-default
   * positive integer is written as `max_viewers` (ADR-0049).
   */
  readonly webrtcMaxViewers: string;
  /**
   * WHEP viewer bearer token (`webrtc` output; '' = omit ⇒ viewing needs a View
   * API key, ADR-0049). A config-secret: masked in the UI, never echoed.
   */
  readonly webrtcToken: string;
  /**
   * WHIP-push origin bearer token (`whip-push` output; '' = omit). A
   * config-secret: masked in the UI, never echoed.
   */
  readonly whipPushToken: string;
  /** Whether a `gpu_pin` block is written. */
  readonly gpuPinEnabled: boolean;
  readonly gpuPinVendor: PinVendor;
  readonly gpuPinStableId: string;
  /** Unmanaged body fields preserved verbatim across an edit. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The output-form fields that can carry a validation error. */
export type OutputField =
  | 'id'
  | 'name'
  | 'mount'
  | 'path'
  | 'url'
  | 'ndiName'
  | 'connector'
  | 'displayModeWidth'
  | 'displayModeHeight'
  | 'displayModeRefresh'
  | 'codec'
  | 'partTargetMs'
  | 'segmentMs'
  | 'gopMs'
  | 'audioTracks'
  | 'webrtcMaxViewers'
  | 'gpuPinStableId';

/**
 * Whether (and how) an output kind runs in this build of the engine:
 * `'runnable'` always runs; `'requires-feature'` runs only in a binary built
 * with the named opt-in feature (a default build FAILS the run with a clear
 * error rather than skipping); `'unbuilt'` is accepted by the config schema
 * but warned + skipped by `build_outputs` ("not yet runnable in this build").
 */
export type OutputRunnability = 'runnable' | 'requires-feature' | 'unbuilt';

/**
 * Mirrors `build_outputs` in crates/multiview-cli/src/pipeline.rs: hls /
 * ll_hls / rtmp / srt build runnable sinks; display builds a raw-frame
 * DRM/KMS sink only in a `display-kms` build (and hard-fails elsewhere —
 * DEV-B1/ADR-0044); rtsp_server and ndi are accepted but warned + skipped.
 */
export const OUTPUT_RUNNABLE: Readonly<Record<OutputKind, OutputRunnability>> = {
  rtsp: 'unbuilt',
  hls: 'runnable',
  'll-hls': 'runnable',
  ndi: 'unbuilt',
  rtmp: 'runnable',
  srt: 'runnable',
  // RIST runs only in a binary built with the off-by-default `rist` feature
  // (a default build fails the run with a clear error, never a silent skip).
  rist: 'requires-feature',
  display: 'requires-feature',
  // WHEP serve + WHIP push run only in a `webrtc-native` build (ADR-0049): a
  // default build accepts the config but cannot serve/push WebRTC.
  webrtc: 'requires-feature',
  'whip-push': 'requires-feature',
};

/** Map a display kind onto the config wire tag. */
export function outputWireKind(kind: OutputKind): string {
  switch (kind) {
    case 'rtsp':
      return 'rtsp_server';
    case 'll-hls':
      return 'll_hls';
    case 'whip-push':
      return 'whip_push';
    default:
      return kind;
  }
}

/** A fresh, empty output form. */
export function emptyOutputForm(): OutputFormState {
  return {
    id: '',
    name: '',
    kind: 'hls',
    mount: '',
    path: '',
    url: '',
    ndiName: '',
    connector: 'auto',
    displayModeChoice: 'auto',
    displayModeWidth: '',
    displayModeHeight: '',
    displayModeRefresh: '',
    codec: 'h264',
    latencyProfile: '',
    partTargetMs: '',
    segmentMs: '',
    gopMs: '',
    audioMode: 'default',
    audioTracks: '',
    webrtcMaxViewers: '',
    webrtcToken: '',
    whipPushToken: '',
    gpuPinEnabled: false,
    gpuPinVendor: 'nvidia',
    gpuPinStableId: '',
    extra: {},
  };
}

/**
 * The body keys the output form manages (everything else is preserved).
 *
 * Deliberately NOT here: `id`. The output config-level `id` is OPTIONAL,
 * label-derived when absent (ADR-0034), and lives in a DIFFERENT namespace
 * from the resource/store id (seeded stores use `output-0..n`): crosspoints /
 * `OutputRef`s bind to the config id, so the form never writes it — an
 * authored `id` rides the extra-preservation path verbatim instead.
 */
const OUTPUT_MANAGED_KEYS: readonly string[] = [
  'kind',
  'mount',
  'path',
  'url',
  'name',
  'connector',
  'mode',
  'forced_mode',
  'codec',
  'latency_profile',
  'part_target_ms',
  'segment_ms',
  'gop_ms',
  'gpu_pin',
  'audio',
  // `webrtc` (WHEP serve) + `whip_push` keys. `label` is the display name for
  // the label-bearing kinds (webrtc), written from the form's Name.
  'label',
  'max_viewers',
  'token',
];

/**
 * Build the exact config `Output` body from a valid form. The routable
 * config-level `id` is never derived from the form/store id (see
 * {@link OUTPUT_MANAGED_KEYS}); an authored one is re-emitted via `extra`.
 */
export function outputFormToBody(form: OutputFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.kind = outputWireKind(form.kind);
  switch (form.kind) {
    case 'rtsp':
      body.mount = form.mount.trim();
      body.codec = form.codec.trim();
      if (form.latencyProfile.trim() !== '') {
        body.latency_profile = form.latencyProfile.trim();
      }
      break;
    case 'll-hls': {
      body.path = form.path.trim();
      body.codec = form.codec.trim();
      const part = parseIntStrict(form.partTargetMs);
      if (part !== undefined) {
        body.part_target_ms = part;
      }
      const segment = parseIntStrict(form.segmentMs);
      if (segment !== undefined) {
        body.segment_ms = segment;
      }
      const gop = parseIntStrict(form.gopMs);
      if (gop !== undefined) {
        body.gop_ms = gop;
      }
      break;
    }
    case 'hls': {
      body.path = form.path.trim();
      body.codec = form.codec.trim();
      const segment = parseIntStrict(form.segmentMs);
      if (segment !== undefined) {
        body.segment_ms = segment;
      }
      break;
    }
    case 'ndi':
      body.name = form.ndiName.trim();
      break;
    case 'display': {
      // Raw-frame DRM/KMS head (DEV-B1/ADR-0044): connector + at most one of
      // the mutually-exclusive mode tables; no codec (pre-encode canvas).
      body.connector = form.connector.trim();
      if (form.displayModeChoice !== 'auto') {
        const width = parseIntStrict(form.displayModeWidth);
        const height = parseIntStrict(form.displayModeHeight);
        const refresh = parseRationalFps(form.displayModeRefresh);
        if (width !== undefined && height !== undefined && refresh !== undefined) {
          const spec = { width, height, refresh: `${String(refresh.num)}/${String(refresh.den)}` };
          if (form.displayModeChoice === 'override') {
            body.mode = spec;
          } else {
            body.forced_mode = spec;
          }
        }
      }
      break;
    }
    // The push URL kinds. RIST push is a URL kind like SRT; its optional typed
    // `rist` options block round-trips through `form.extra` until a dedicated
    // options sub-form lands (ADR-0095).
    case 'rtmp':
    case 'srt':
    case 'rist':
      body.url = form.url.trim();
      body.codec = form.codec.trim();
      break;
    case 'webrtc': {
      // WHEP serve (ADR-0049): label-named, never encodes (consumes the H.264
      // program rendition). `max_viewers` default 8 is omitted; a non-empty
      // token gates viewing.
      body.label = form.name.trim();
      body.codec = form.codec.trim();
      const viewers = parseIntStrict(form.webrtcMaxViewers);
      if (viewers !== undefined && viewers !== DEFAULT_WEBRTC_MAX_VIEWERS) {
        body.max_viewers = viewers;
      }
      const token = form.webrtcToken.trim();
      if (token !== '') {
        body.token = token;
      }
      break;
    }
    case 'whip-push': {
      // WHIP push client (ADR-0049): publishes the program to a remote origin.
      body.url = form.url.trim();
      body.codec = form.codec.trim();
      const token = form.whipPushToken.trim();
      if (token !== '') {
        body.token = token;
      }
      break;
    }
  }
  if (form.audioMode !== 'default') {
    body.audio = {
      mode: form.audioMode,
      tracks: form.audioMode === 'tracks' ? parseTrackList(form.audioTracks) : [],
    };
  }
  if (form.gpuPinEnabled) {
    body.gpu_pin = { vendor: form.gpuPinVendor, stable_id: form.gpuPinStableId.trim() };
  }
  return body;
}

/**
 * Parse an exact-rational refresh entry: `num/den` (`60000/1001`) or a bare
 * positive integer (`50` ⇒ `50/1`). Floats are rejected — frame rates are
 * exact rationals, never floats (invariant #3).
 */
export function parseRationalFps(value: string): { num: number; den: number } | undefined {
  const trimmed = value.trim();
  const ratio = /^(\d+)\s*\/\s*(\d+)$/.exec(trimmed);
  if (ratio) {
    const num = Number.parseInt(ratio[1] ?? '', 10);
    const den = Number.parseInt(ratio[2] ?? '', 10);
    if (!Number.isSafeInteger(num) || !Number.isSafeInteger(den) || num <= 0 || den <= 0) {
      return undefined;
    }
    return { num, den };
  }
  if (/^\d+$/.test(trimmed)) {
    const num = Number.parseInt(trimmed, 10);
    if (!Number.isSafeInteger(num) || num <= 0) {
      return undefined;
    }
    return { num, den: 1 };
  }
  return undefined;
}

/** Split a comma-separated track list into trimmed, non-empty names. */
export function parseTrackList(value: string): string[] {
  return value
    .split(',')
    .map((track) => track.trim())
    .filter((track) => track !== '');
}

/**
 * Parse a stored output wire kind onto the display kind, or `undefined` for a
 * kind this UI cannot edit — an explicit refusal, never a fold.
 */
export function parseOutputFormKind(tag: string | undefined): OutputKind | undefined {
  if (tag === 'rtsp_server') {
    return 'rtsp';
  }
  if (tag === 'll_hls') {
    return 'll-hls';
  }
  if (tag === 'whip_push') {
    return 'whip-push';
  }
  return (['hls', 'ndi', 'rtmp', 'srt', 'rist', 'display', 'webrtc'] as const).find(
    (k) => k === tag,
  );
}

/**
 * Project a stored record back onto the editable output form, or `undefined`
 * when the body's kind is not one this UI can edit (the page disables Edit;
 * the document is preserved as authored).
 */
export function outputFormFromRecord(record: ResourceRecord): OutputFormState | undefined {
  const body = record.body;
  const empty = emptyOutputForm();
  const kind = parseOutputFormKind(asString(body.kind));
  if (kind === undefined) {
    return undefined;
  }
  const audio = asRecord(body.audio);
  const audioMode = asString(audio?.mode);
  const rawTracks = audio?.tracks;
  const tracks = Array.isArray(rawTracks)
    ? rawTracks.filter((track): track is string => typeof track === 'string')
    : [];
  const gpuPin = asRecord(body.gpu_pin);
  // The display mode tables (`mode` overrides among EDID modes; `forced_mode`
  // is the EDID-less CVT-RB timing). At most one is authored (config-validated).
  const overrideSpec = asRecord(body.mode);
  const forcedSpec = asRecord(body.forced_mode);
  const modeSpec = overrideSpec ?? forcedSpec;
  let modeChoice: DisplayModeChoice = 'auto';
  if (overrideSpec !== undefined) {
    modeChoice = 'override';
  } else if (forcedSpec !== undefined) {
    modeChoice = 'forced';
  }
  return {
    ...empty,
    id: record.id,
    // The label-bearing kinds (webrtc) author their display name as `label`;
    // prefer it over the store name so the round-trip is stable (the writer
    // emits `label` from the form's Name).
    name: asString(body.label) ?? record.name,
    kind,
    mount: asString(body.mount) ?? '',
    path: asString(body.path) ?? '',
    url: asString(body.url) ?? '',
    ndiName: kind === 'ndi' ? (asString(body.name) ?? '') : '',
    connector: asString(body.connector) ?? 'auto',
    displayModeChoice: kind === 'display' ? modeChoice : 'auto',
    displayModeWidth: numberToField(asFiniteNumber(modeSpec?.width)),
    displayModeHeight: numberToField(asFiniteNumber(modeSpec?.height)),
    displayModeRefresh: asString(modeSpec?.refresh) ?? '',
    codec: asString(body.codec) ?? (kind === 'ndi' || kind === 'display' ? '' : 'h264'),
    latencyProfile: asString(body.latency_profile) ?? '',
    partTargetMs: numberToField(asFiniteNumber(body.part_target_ms)),
    segmentMs: numberToField(asFiniteNumber(body.segment_ms)),
    gopMs: numberToField(asFiniteNumber(body.gop_ms)),
    audioMode:
      audioMode === 'program' || audioMode === 'tracks' ? audioMode : 'default',
    audioTracks: tracks.join(', '),
    webrtcMaxViewers: kind === 'webrtc' ? numberToField(asFiniteNumber(body.max_viewers)) : '',
    webrtcToken: kind === 'webrtc' ? (asString(body.token) ?? '') : '',
    whipPushToken: kind === 'whip-push' ? (asString(body.token) ?? '') : '',
    gpuPinEnabled: gpuPin !== undefined,
    gpuPinVendor: asPinVendor(gpuPin?.vendor),
    gpuPinStableId: asString(gpuPin?.stable_id) ?? '',
    extra: extraOf(body, OUTPUT_MANAGED_KEYS),
  };
}

/** Validate a positive-integer-or-blank duration field. */
function durationError(value: string): FormErrorCode | undefined {
  if (value.trim() === '') {
    return undefined;
  }
  const parsed = parseIntStrict(value);
  return parsed === undefined || parsed <= 0 ? 'positive-int' : undefined;
}

/** Validate an output form, returning per-field machine codes. */
export function validateOutputForm(
  form: OutputFormState,
  creating: boolean,
): FieldErrors<OutputField> {
  const errors: FieldErrors<OutputField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  switch (form.kind) {
    case 'rtsp':
      if (form.mount.trim() === '') {
        errors.mount = 'required';
      } else if (!form.mount.trim().startsWith('/')) {
        errors.mount = 'mount-slash';
      }
      break;
    case 'hls':
    case 'll-hls':
      if (form.path.trim() === '') {
        errors.path = 'required';
      }
      break;
    case 'ndi':
      if (form.ndiName.trim() === '') {
        errors.ndiName = 'required';
      }
      break;
    case 'display': {
      if (form.connector.trim() === '') {
        errors.connector = 'required';
      }
      if (form.displayModeChoice !== 'auto') {
        const width = parseIntStrict(form.displayModeWidth);
        if (width === undefined || width <= 0) {
          errors.displayModeWidth = 'positive-int';
        }
        const height = parseIntStrict(form.displayModeHeight);
        if (height === undefined || height <= 0) {
          errors.displayModeHeight = 'positive-int';
        }
        if (parseRationalFps(form.displayModeRefresh) === undefined) {
          errors.displayModeRefresh = 'rational-fps';
        }
      }
      break;
    }
    case 'rtmp': {
      const code = urlErrorCode(form.url, ['rtmp', 'rtmps'], 'scheme-rtmp');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'srt': {
      const code = urlErrorCode(form.url, ['srt'], 'scheme-srt');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'rist': {
      const code = urlErrorCode(form.url, ['rist'], 'scheme-rist');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'whip-push': {
      // The remote WHIP origin is an http(s) URL (https recommended, ADR-0049).
      const code = urlErrorCode(form.url, ['http', 'https'], 'scheme-http');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'webrtc': {
      // WHEP serve authors no URL (derived from the id). `max_viewers` is
      // optional; when present it must be a positive integer (ADR-0049: >= 1).
      const viewers = form.webrtcMaxViewers.trim();
      if (viewers !== '') {
        const parsed = parseIntStrict(viewers);
        if (parsed === undefined || parsed <= 0) {
          errors.webrtcMaxViewers = 'positive-int';
        }
      }
      break;
    }
  }
  // NDI and display carry raw frames, not an encoded rendition — no codec.
  if (form.kind !== 'ndi' && form.kind !== 'display' && form.codec.trim() === '') {
    errors.codec = 'required';
  }
  if (form.kind === 'll-hls') {
    const part = durationError(form.partTargetMs);
    if (part !== undefined) {
      errors.partTargetMs = part;
    }
    const gop = durationError(form.gopMs);
    if (gop !== undefined) {
      errors.gopMs = gop;
    }
  }
  if (form.kind === 'hls' || form.kind === 'll-hls') {
    const segment = durationError(form.segmentMs);
    if (segment !== undefined) {
      errors.segmentMs = segment;
    }
  }
  if (form.audioMode === 'tracks' && parseTrackList(form.audioTracks).length === 0) {
    errors.audioTracks = 'tracks-required';
  }
  if (form.gpuPinEnabled && form.gpuPinStableId.trim() === '') {
    errors.gpuPinStableId = 'required';
  }
  return errors;
}

// --- Overlays -------------------------------------------------------------------

/** The editable state behind the overlay form. */
export interface OverlayFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: OverlayKind;
  /** Attachment target (`canvas` or a cell id). */
  readonly target: string;
  /** Stacking order (an integer string). */
  readonly z: string;
  /** Clock face param (`clock`; the run path renders the analog face). */
  readonly clockFace: ClockFace;
  /** Clock timezone offset minutes (`clock`, '' = omit ⇒ UTC). */
  readonly clockTzMinutes: string;
  /** Clock centre x in canvas pixels (`clock`, '' = auto placement). */
  readonly clockX: string;
  /** Clock centre y in canvas pixels (`clock`, '' = auto placement). */
  readonly clockY: string;
  /** Clock face radius in pixels (`clock`, '' = auto size). */
  readonly clockRadius: string;
  /** Border width in pixels (`tally_border`, '' = default). */
  readonly tallyWidthPx: string;
  /** Border colour hex (`tally_border`, '' = default). */
  readonly tallyColor: string;
  /** Tally binding URI (`tally_border`, e.g. `tally://cell_big`; '' = omit). */
  readonly tallyBinding: string;
  /** Unmanaged params preserved verbatim while the kind is unchanged. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The overlay-form fields that can carry a validation error. */
export type OverlayField =
  | 'id'
  | 'name'
  | 'target'
  | 'z'
  | 'clockTzMinutes'
  | 'clockX'
  | 'clockY'
  | 'clockRadius'
  | 'tallyWidthPx'
  | 'tallyColor';

/** A fresh, empty overlay form. */
export function emptyOverlayForm(): OverlayFormState {
  return {
    id: '',
    name: '',
    kind: 'clock',
    target: 'canvas',
    z: '0',
    clockFace: 'analog',
    clockTzMinutes: '',
    clockX: '',
    clockY: '',
    clockRadius: '',
    tallyWidthPx: '',
    tallyColor: '',
    tallyBinding: '',
    extra: {},
  };
}

/** The body keys every overlay kind's writer re-emits. */
const OVERLAY_BASE_KEYS: readonly string[] = ['id', 'kind', 'target', 'z'];

/**
 * The flattened param keys the overlay form manages for ONE kind — exactly
 * the keys that kind's writer re-emits. Stripping must be kind-scoped: a
 * `label` overlay carrying e.g. `color` or `x` (param names another kind also
 * uses) must keep them via the extra-preservation path, because the label
 * writer would never re-emit them.
 */
function overlayManagedKeys(kind: OverlayKind): readonly string[] {
  switch (kind) {
    case 'clock':
      return [...OVERLAY_BASE_KEYS, 'face', 'tz_minutes', 'x', 'y', 'radius'];
    case 'tally_border':
      return [...OVERLAY_BASE_KEYS, 'width_px', 'color', 'binding'];
    case 'label':
    case 'image':
    case 'subtitle':
      return OVERLAY_BASE_KEYS;
  }
}

/**
 * Switch an overlay form to a new kind. Params are kind-scoped (the config
 * flattens them verbatim), so the previous kind's params — including the
 * preserved unmanaged extras — are dropped rather than leaked into the body.
 */
export function withOverlayKind(form: OverlayFormState, kind: OverlayKind): OverlayFormState {
  if (kind === form.kind) {
    return form;
  }
  const empty = emptyOverlayForm();
  return {
    ...empty,
    id: form.id,
    name: form.name,
    kind,
    target: form.target,
    z: form.z,
  };
}

/** Build the exact config `Overlay` body from a valid form. */
export function overlayFormToBody(form: OverlayFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.id = form.id.trim();
  body.kind = form.kind;
  body.target = form.target.trim();
  body.z = parseIntStrict(form.z) ?? 0;
  if (form.kind === 'clock') {
    body.face = form.clockFace;
    const tz = parseIntStrict(form.clockTzMinutes);
    if (tz !== undefined) {
      body.tz_minutes = tz;
    }
    const x = parseIntStrict(form.clockX);
    if (x !== undefined) {
      body.x = x;
    }
    const y = parseIntStrict(form.clockY);
    if (y !== undefined) {
      body.y = y;
    }
    const radius = parseIntStrict(form.clockRadius);
    if (radius !== undefined) {
      body.radius = radius;
    }
  }
  if (form.kind === 'tally_border') {
    const width = parseIntStrict(form.tallyWidthPx);
    if (width !== undefined) {
      body.width_px = width;
    }
    if (form.tallyColor.trim() !== '') {
      body.color = form.tallyColor.trim();
    }
    if (form.tallyBinding.trim() !== '') {
      body.binding = form.tallyBinding.trim();
    }
  }
  // label / image / subtitle carry no kind-specific params in this build: the
  // Rust side defines none for them (schema.rs `Overlay` keeps params verbatim
  // and the run path consumes only the clock params) — nothing is invented.
  return body;
}

/**
 * Parse a stored overlay kind tag, or `undefined` for a kind this UI cannot
 * edit — an explicit refusal, never a fold.
 */
export function parseOverlayFormKind(tag: string | undefined): OverlayKind | undefined {
  return (['clock', 'label', 'tally_border', 'image', 'subtitle'] as const).find(
    (k) => k === tag,
  );
}

/**
 * Project a stored record back onto the editable overlay form, or `undefined`
 * when the body's kind is not one this UI can edit (the page disables Edit;
 * the document is preserved as authored).
 */
export function overlayFormFromRecord(record: ResourceRecord): OverlayFormState | undefined {
  const body = record.body;
  const empty = emptyOverlayForm();
  const kind = parseOverlayFormKind(asString(body.kind));
  if (kind === undefined) {
    return undefined;
  }
  return {
    ...empty,
    id: record.id,
    name: record.name,
    kind,
    target: asString(body.target) ?? 'canvas',
    z: numberToField(asFiniteNumber(body.z)) || '0',
    clockFace: CLOCK_FACES.find((face) => face === asString(body.face)) ?? 'analog',
    clockTzMinutes: numberToField(asFiniteNumber(body.tz_minutes)),
    clockX: numberToField(asFiniteNumber(body.x)),
    clockY: numberToField(asFiniteNumber(body.y)),
    clockRadius: numberToField(asFiniteNumber(body.radius)),
    tallyWidthPx: numberToField(asFiniteNumber(body.width_px)),
    tallyColor: asString(body.color) ?? '',
    tallyBinding: asString(body.binding) ?? '',
    extra: extraOf(body, overlayManagedKeys(kind)),
  };
}

/** Validate an optional integer field ('' = fine). */
function optionalIntError(value: string): FormErrorCode | undefined {
  if (value.trim() === '') {
    return undefined;
  }
  return parseIntStrict(value) === undefined ? 'int' : undefined;
}

/** Validate an overlay form, returning per-field machine codes. */
export function validateOverlayForm(
  form: OverlayFormState,
  creating: boolean,
): FieldErrors<OverlayField> {
  const errors: FieldErrors<OverlayField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  if (form.target.trim() === '') {
    errors.target = 'required';
  }
  if (parseIntStrict(form.z) === undefined) {
    errors.z = 'int';
  }
  if (form.kind === 'clock') {
    const tz = optionalIntError(form.clockTzMinutes);
    if (tz !== undefined) {
      errors.clockTzMinutes = tz;
    }
    const x = optionalIntError(form.clockX);
    if (x !== undefined) {
      errors.clockX = x;
    }
    const y = optionalIntError(form.clockY);
    if (y !== undefined) {
      errors.clockY = y;
    }
    if (form.clockRadius.trim() !== '') {
      const radius = parseIntStrict(form.clockRadius);
      if (radius === undefined || radius <= 0) {
        errors.clockRadius = 'positive-int';
      }
    }
  }
  if (form.kind === 'tally_border') {
    if (form.tallyWidthPx.trim() !== '') {
      const width = parseIntStrict(form.tallyWidthPx);
      if (width === undefined || width <= 0) {
        errors.tallyWidthPx = 'positive-int';
      }
    }
    if (form.tallyColor.trim() !== '' && !HEX_COLOR_RE.test(form.tallyColor.trim())) {
      errors.tallyColor = 'hex-color';
    }
  }
  return errors;
}

// --- Probes ---------------------------------------------------------------------

/**
 * The X.733 perceived-severity wire values (core `PerceivedSeverity`,
 * PascalCase variant names), in ascending order of urgency.
 */
export type ProbeSeverity =
  | 'Cleared'
  | 'Indeterminate'
  | 'Warning'
  | 'Minor'
  | 'Major'
  | 'Critical';

/** All severities in ascending urgency, for building selectors. */
export const PROBE_SEVERITIES: readonly ProbeSeverity[] = [
  'Cleared',
  'Indeterminate',
  'Warning',
  'Minor',
  'Major',
  'Critical',
];

/** The loudness compliance standards (config `LoudnessTarget` kind tags). */
export type LoudnessStandard = 'r128' | 'a85';

/** Both loudness standards, for building selectors. */
export const LOUDNESS_STANDARDS: readonly LoudnessStandard[] = ['r128', 'a85'];

/** The standard's default integrated-loudness target (LUFS/LKFS, as a field). */
function loudnessTargetDefault(standard: LoudnessStandard): string {
  return standard === 'a85' ? '-24' : '-23';
}

/** The standard's default max true-peak ceiling (dBTP, as a field). */
function loudnessTruePeakDefault(standard: LoudnessStandard): string {
  return standard === 'a85' ? '-2' : '-1';
}

/** The editable state behind the probe form (numbers kept as input strings). */
export interface ProbeFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: ProbeKind;
  /** The cell id this probe watches. */
  readonly cell: string;
  /** Luma ceiling, 8-bit `0..=255` (`black`). */
  readonly lumaThreshold: string;
  /** Inter-frame difference floor, per-mille `0..=1000` (`freeze`). */
  readonly differenceThreshold: string;
  /** Whether a `zone` block is written (`black`/`freeze`; off = full frame). */
  readonly zoneEnabled: boolean;
  /** Zone left edge, fraction of tile width (`0..=1`). */
  readonly zoneX: string;
  /** Zone top edge, fraction of tile height (`0..=1`). */
  readonly zoneY: string;
  /** Zone width, fraction of tile width (`0..=1`). */
  readonly zoneW: string;
  /** Zone height, fraction of tile height (`0..=1`). */
  readonly zoneH: string;
  /** Level ceiling in dBFS at or below which audio is silent (`silence`). */
  readonly levelDbfs: string;
  /** Loudness compliance standard (`loudness`). */
  readonly loudnessStandard: LoudnessStandard;
  /** Integrated-loudness target in LUFS/LKFS (`loudness`). */
  readonly loudnessTarget: string;
  /** Max permitted true-peak in dBTP (`loudness`). */
  readonly loudnessTruePeak: string;
  /** Milliseconds the condition must persist before the alarm raises. */
  readonly dwellUpMs: string;
  /** Milliseconds the condition must clear before the alarm clears. */
  readonly dwellDownMs: string;
  /** The X.733 severity asserted when the probe fires. */
  readonly severity: ProbeSeverity;
  /** Whether the alarm latches (held until explicitly reset). */
  readonly latched: boolean;
  /** Unmanaged body fields preserved verbatim across an edit. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The probe-form fields that can carry a validation error. */
export type ProbeField =
  | 'id'
  | 'name'
  | 'cell'
  | 'lumaThreshold'
  | 'differenceThreshold'
  | 'zoneX'
  | 'zoneY'
  | 'zoneW'
  | 'zoneH'
  | 'levelDbfs'
  | 'loudnessTarget'
  | 'loudnessTruePeak'
  | 'dwellUpMs'
  | 'dwellDownMs';

/** A fresh, empty probe form (schema-aligned defaults). */
export function emptyProbeForm(): ProbeFormState {
  return {
    id: '',
    name: '',
    kind: 'black',
    cell: '',
    lumaThreshold: '16',
    differenceThreshold: '5',
    zoneEnabled: false,
    zoneX: '0',
    zoneY: '0',
    zoneW: '1',
    zoneH: '1',
    levelDbfs: '-60',
    loudnessStandard: 'r128',
    loudnessTarget: loudnessTargetDefault('r128'),
    loudnessTruePeak: loudnessTruePeakDefault('r128'),
    dwellUpMs: '1000',
    dwellDownMs: '1000',
    severity: 'Warning',
    latched: false,
    extra: {},
  };
}

/** The body keys the probe form manages (everything else is preserved). */
const PROBE_MANAGED_KEYS: readonly string[] = [
  'id',
  'cell',
  'kind',
  'luma_threshold',
  'difference_threshold',
  'zone',
  'level_dbfs',
  'target',
  'dwell',
  'severity',
  'latched',
];

/**
 * Switch a probe form to a new kind. The shared identity/policy fields (cell,
 * dwell, severity, latched) are kept; kind-scoped parameters reset to their
 * defaults so a stale payload can never leak into the body.
 */
export function withProbeKind(form: ProbeFormState, kind: ProbeKind): ProbeFormState {
  if (kind === form.kind) {
    return form;
  }
  const empty = emptyProbeForm();
  return {
    ...form,
    kind,
    lumaThreshold: empty.lumaThreshold,
    differenceThreshold: empty.differenceThreshold,
    zoneEnabled: empty.zoneEnabled,
    zoneX: empty.zoneX,
    zoneY: empty.zoneY,
    zoneW: empty.zoneW,
    zoneH: empty.zoneH,
    levelDbfs: empty.levelDbfs,
    loudnessStandard: empty.loudnessStandard,
    loudnessTarget: empty.loudnessTarget,
    loudnessTruePeak: empty.loudnessTruePeak,
  };
}

/**
 * Switch the loudness standard, resetting the target/true-peak fields to the
 * new standard's defaults (R128 −23 LUFS / −1 dBTP; A/85 −24 LKFS / −2 dBTP).
 */
export function withProbeLoudnessStandard(
  form: ProbeFormState,
  standard: LoudnessStandard,
): ProbeFormState {
  if (standard === form.loudnessStandard) {
    return form;
  }
  return {
    ...form,
    loudnessStandard: standard,
    loudnessTarget: loudnessTargetDefault(standard),
    loudnessTruePeak: loudnessTruePeakDefault(standard),
  };
}

/** Build the exact config `Probe` body from a valid form. */
export function probeFormToBody(form: ProbeFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.id = form.id.trim();
  body.cell = form.cell.trim();
  body.kind = form.kind;
  switch (form.kind) {
    case 'black':
      body.luma_threshold = parseIntStrict(form.lumaThreshold) ?? 0;
      break;
    case 'freeze':
      body.difference_threshold = parseIntStrict(form.differenceThreshold) ?? 0;
      break;
    case 'silence':
      body.level_dbfs = parseNumberStrict(form.levelDbfs) ?? 0;
      break;
    case 'loudness': {
      const target = parseNumberStrict(form.loudnessTarget) ?? 0;
      const peak = parseNumberStrict(form.loudnessTruePeak) ?? 0;
      body.target =
        form.loudnessStandard === 'a85'
          ? { kind: 'a85', target_lkfs: target, max_true_peak_dbtp: peak }
          : { kind: 'r128', target_lufs: target, max_true_peak_dbtp: peak };
      break;
    }
  }
  if ((form.kind === 'black' || form.kind === 'freeze') && form.zoneEnabled) {
    body.zone = {
      x: parseNumberStrict(form.zoneX) ?? 0,
      y: parseNumberStrict(form.zoneY) ?? 0,
      w: parseNumberStrict(form.zoneW) ?? 1,
      h: parseNumberStrict(form.zoneH) ?? 1,
    };
  }
  // Dwell, severity, and latching are policy the operator always authors here;
  // the explicit values match the schema defaults on a fresh form.
  body.dwell = {
    up_ms: parseIntStrict(form.dwellUpMs) ?? 1000,
    down_ms: parseIntStrict(form.dwellDownMs) ?? 1000,
  };
  body.severity = form.severity;
  body.latched = form.latched;
  return body;
}

/**
 * Parse a stored probe kind tag, or `undefined` for a kind this UI cannot
 * edit — an explicit refusal, never a fold.
 */
export function parseProbeFormKind(tag: string | undefined): ProbeKind | undefined {
  return (['black', 'freeze', 'silence', 'loudness'] as const).find((k) => k === tag);
}

/**
 * Project a stored record back onto the editable probe form, or `undefined`
 * when the body's kind is not one this UI can edit (the page disables Edit;
 * the document is preserved as authored).
 */
export function probeFormFromRecord(record: ResourceRecord): ProbeFormState | undefined {
  const body = record.body;
  const empty = emptyProbeForm();
  const kind = parseProbeFormKind(asString(body.kind));
  if (kind === undefined) {
    return undefined;
  }
  const zone = asRecord(body.zone);
  const dwell = asRecord(body.dwell);
  const target = asRecord(body.target);
  const standard: LoudnessStandard = asString(target?.kind) === 'a85' ? 'a85' : 'r128';
  const targetValue =
    standard === 'a85' ? asFiniteNumber(target?.target_lkfs) : asFiniteNumber(target?.target_lufs);
  return {
    ...empty,
    id: record.id,
    name: record.name,
    kind,
    cell: asString(body.cell) ?? '',
    lumaThreshold: numberToField(asFiniteNumber(body.luma_threshold)) || empty.lumaThreshold,
    differenceThreshold:
      numberToField(asFiniteNumber(body.difference_threshold)) || empty.differenceThreshold,
    zoneEnabled: zone !== undefined,
    zoneX: zone === undefined ? empty.zoneX : numberToField(asFiniteNumber(zone.x)) || '0',
    zoneY: zone === undefined ? empty.zoneY : numberToField(asFiniteNumber(zone.y)) || '0',
    zoneW: zone === undefined ? empty.zoneW : numberToField(asFiniteNumber(zone.w)) || '1',
    zoneH: zone === undefined ? empty.zoneH : numberToField(asFiniteNumber(zone.h)) || '1',
    levelDbfs: numberToField(asFiniteNumber(body.level_dbfs)) || empty.levelDbfs,
    loudnessStandard: standard,
    loudnessTarget: numberToField(targetValue) || loudnessTargetDefault(standard),
    loudnessTruePeak:
      numberToField(asFiniteNumber(target?.max_true_peak_dbtp)) ||
      loudnessTruePeakDefault(standard),
    dwellUpMs: numberToField(asFiniteNumber(dwell?.up_ms)) || empty.dwellUpMs,
    dwellDownMs: numberToField(asFiniteNumber(dwell?.down_ms)) || empty.dwellDownMs,
    severity: PROBE_SEVERITIES.find((s) => s === asString(body.severity)) ?? 'Cleared',
    latched: body.latched === true,
    extra: extraOf(body, PROBE_MANAGED_KEYS),
  };
}

/** Validate a bounded-integer field (`min..=max`), returning a code or none. */
function intRangeError(value: string, min: number, max: number): FormErrorCode | undefined {
  const parsed = parseIntStrict(value);
  return parsed === undefined || parsed < min || parsed > max ? 'int-range' : undefined;
}

/** Validate the detection-zone geometry into per-field error codes. */
function validateZone(form: ProbeFormState, errors: FieldErrors<ProbeField>): void {
  const x = parseNumberStrict(form.zoneX);
  const y = parseNumberStrict(form.zoneY);
  const w = parseNumberStrict(form.zoneW);
  const h = parseNumberStrict(form.zoneH);
  if (x === undefined) {
    errors.zoneX = 'number';
  }
  if (y === undefined) {
    errors.zoneY = 'number';
  }
  if (w === undefined) {
    errors.zoneW = 'number';
  }
  if (h === undefined) {
    errors.zoneH = 'number';
  }
  if (x === undefined || y === undefined || w === undefined || h === undefined) {
    return;
  }
  // Mirrors DetectionZone::validate: positive extent, origin within the unit
  // square, and no overhang past 1.0 on either axis.
  if (x < 0) {
    errors.zoneX = 'zone-extent';
  }
  if (y < 0) {
    errors.zoneY = 'zone-extent';
  }
  if (w <= 0 || x + w > 1) {
    errors.zoneW = 'zone-extent';
  }
  if (h <= 0 || y + h > 1) {
    errors.zoneH = 'zone-extent';
  }
}

/** Validate a probe form, returning per-field machine codes. */
export function validateProbeForm(
  form: ProbeFormState,
  creating: boolean,
): FieldErrors<ProbeField> {
  const errors: FieldErrors<ProbeField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  if (form.cell.trim() === '') {
    errors.cell = 'required';
  }
  switch (form.kind) {
    case 'black': {
      const code = intRangeError(form.lumaThreshold, 0, 255);
      if (code !== undefined) {
        errors.lumaThreshold = code;
      }
      break;
    }
    case 'freeze': {
      const code = intRangeError(form.differenceThreshold, 0, 1000);
      if (code !== undefined) {
        errors.differenceThreshold = code;
      }
      break;
    }
    case 'silence':
      if (parseNumberStrict(form.levelDbfs) === undefined) {
        errors.levelDbfs = 'number';
      }
      break;
    case 'loudness':
      if (parseNumberStrict(form.loudnessTarget) === undefined) {
        errors.loudnessTarget = 'number';
      }
      if (parseNumberStrict(form.loudnessTruePeak) === undefined) {
        errors.loudnessTruePeak = 'number';
      }
      break;
  }
  if ((form.kind === 'black' || form.kind === 'freeze') && form.zoneEnabled) {
    validateZone(form, errors);
  }
  // Dwell fields are u32 on the wire — bound client-side so an overlarge
  // value fails locally instead of bouncing as a server-side 422.
  const U32_MAX = 4294967295;
  const up = intRangeError(form.dwellUpMs, 0, U32_MAX);
  if (up !== undefined) {
    errors.dwellUpMs = up;
  }
  const down = intRangeError(form.dwellDownMs, 0, U32_MAX);
  if (down !== undefined) {
    errors.dwellDownMs = down;
  }
  return errors;
}

/**
 * Collect the cell ids offered by the layout documents, for the probe form's
 * cell picker: cells of canvas-bearing (working) layouts first, then draft
 * cells, deduplicated in encounter order. Malformed bodies are skipped — the
 * picker degrades to a free-text field when nothing is found.
 */
export function cellIdsFromLayouts(
  layouts: readonly { readonly body: unknown }[],
): readonly string[] {
  // Only canvas-bearing layout documents feed the picker: the export composes
  // from the working layout, so a probe bound to a draft-only cell would fail
  // every `GET /config/export` with an unknown-cell 422. Draft cells are typed
  // manually (the field accepts free text) — deliberate, not discoverable.
  const out: string[] = [];
  for (const layout of layouts) {
    const body = asRecord(layout.body);
    if (body?.canvas === undefined) {
      continue;
    }
    const cells = Array.isArray(body.cells) ? body.cells : [];
    for (const cell of cells) {
      const id = asString(asRecord(cell)?.id);
      if (id !== undefined && id !== '' && !out.includes(id)) {
        out.push(id);
      }
    }
  }
  return out;
}
