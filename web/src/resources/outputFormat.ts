// Pure helpers of the output picture format and slots editor (#342 B3): the
// stored `format` / `slots` live in the output form's `extra` (round-tripped
// verbatim, so an unknown key is never dropped).
import type { OutputFormState } from './forms';
import type { OutputKind } from './types';

/** The fit modes of an output format. */
export const FIT_MODES = ['contain', 'cover', 'fill'] as const;
/** A fit mode. */
export type FitMode = (typeof FIT_MODES)[number];
/** The anchors a contain box or a cover crop is placed at. */
export const ANCHORS = [
  'center',
  'top',
  'bottom',
  'left',
  'right',
  'top_left',
  'top_right',
  'bottom_left',
  'bottom_right',
] as const;
/** An anchor. */
export type Anchor = (typeof ANCHORS)[number];

/** A format preset. */
export interface FormatPreset {
  readonly id: string;
  readonly width: number;
  readonly height: number;
  readonly mode: FitMode;
  readonly anchor: Anchor;
}

/** The presets the editor offers. */
export const FORMAT_PRESETS: readonly FormatPreset[] = [
  { id: '1920x1080', width: 1920, height: 1080, mode: 'contain', anchor: 'center' },
  { id: '1280x720', width: 1280, height: 720, mode: 'contain', anchor: 'center' },
  { id: '1080x1920-cover', width: 1080, height: 1920, mode: 'cover', anchor: 'center' },
];

/**
 * Output kinds whose format/bitrate change resets the output live: HLS with
 * TS segments only (its segment muxer is rebuilt at the discontinuity).
 * Everything else — SRT, RIST included — applies on the next restart (a
 * live reset there is follow-up #380).
 */
export function resetsLive(kind: OutputKind): boolean {
  return kind === 'hls';
}

/** Kinds with an encoded video slot (not raw frames, not audio only). */
export function hasEncodedVideo(kind: OutputKind): boolean {
  // NDI and the display head carry raw frames: no encoded video slot. AES67
  // (audio only) is not an `OutputKind` at all: it has no form in this UI
  // (Edit is refused), so the format editor can never show for it.
  return kind !== 'ndi' && kind !== 'display';
}

/** The largest output picture area (the config's `MAX_FORMAT_AREA`, 4K UHD). */
export const MAX_FORMAT_AREA = 3840 * 2160;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** The authored format (`extra.format`), if any. */
export function formatOf(form: OutputFormState): {
  readonly width: string;
  readonly height: string;
  readonly mode: FitMode;
  readonly anchor: Anchor;
} | undefined {
  const format = form.extra.format;
  if (!isRecord(format)) return undefined;
  const fit = isRecord(format.fit) ? format.fit : {};
  const mode = FIT_MODES.find((m) => m === fit.mode) ?? 'contain';
  const anchor = ANCHORS.find((a) => a === fit.anchor) ?? 'center';
  return {
    width:
      typeof format.width === 'number' || typeof format.width === 'string' ? String(format.width) : '',
    height:
      typeof format.height === 'number' || typeof format.height === 'string'
        ? String(format.height)
        : '',
    mode,
    anchor,
  };
}

/** Video slot 0's bitrate (kbit/s) as a field string ('' = default). */
export function bitrateOf(form: OutputFormState): string {
  const slots = form.extra.slots;
  if (!isRecord(slots) || !Array.isArray(slots.video)) return '';
  const slot0: unknown = slots.video[0];
  if (!isRecord(slot0)) return '';
  const kbps = slot0.bitrate_kbps;
  // A number as stored, or the raw text while it is not a valid number yet
  // (so validation sees it).
  return typeof kbps === 'number' || typeof kbps === 'string' ? String(kbps) : '';
}

/**
 * Subtitle slot formats (`[[outputs.slots.subtitle]]`, slice F #345), mirrors
 * `multiview_config::output_format::SubtitleFormat`
 * (`crates/multiview-config/src/output_format.rs`).
 */
export const SUBTITLE_FORMATS = ['webvtt', 'cea608', 'dvb'] as const;
/** A subtitle slot format. */
export type SubtitleFormat = (typeof SUBTITLE_FORMATS)[number];

/** Most subtitle slots an output may declare (`MAX_SUBTITLE_SLOTS`). */
export const MAX_SUBTITLE_SLOTS = 8;
/** Longest accepted subtitle slot language tag, in characters. */
export const MAX_SUBTITLE_LANGUAGE_LEN = 35;
/** Longest accepted subtitle slot menu name, in characters. */
export const MAX_SUBTITLE_NAME_LEN = 64;

/**
 * One editable subtitle slot row (`[[outputs.slots.subtitle]]`): `source`
 * `undefined` means unpatched.
 */
export interface SubtitleSlotRow {
  readonly format: SubtitleFormat;
  readonly language: string;
  readonly name: string;
  readonly source: string | undefined;
}

/**
 * Whether an output kind's medium carries subtitles at all, and which
 * formats it builds vs. only carries (mirrors `Output::slot_capability` /
 * `SlotCapability.subtitle_medium_formats` / `subtitle_formats`,
 * `crates/multiview-config/src/output_format.rs`). `ll_hls`, `srt`, `rist`
 * and `rtmp` carry a non-empty `mediumFormats` with an EMPTY `builtFormats`
 * (the medium supports it; the engine does not build it yet) — the editor
 * renders those as "not supported yet" with a follow-up issue, distinct from
 * a medium that carries no subtitles at all (`slot-medium-mismatch`).
 */
export interface SubtitleCapability {
  readonly max: number;
  readonly mediumFormats: readonly SubtitleFormat[];
  readonly builtFormats: readonly SubtitleFormat[];
}

const NO_SUBTITLES: readonly SubtitleFormat[] = [];
const WEBVTT_ONLY: readonly SubtitleFormat[] = ['webvtt'];
const TS_SUBTITLES: readonly SubtitleFormat[] = ['cea608', 'dvb'];

/**
 * The subtitle capability of an output kind. Follow-up issues tracking the
 * "not supported yet" gaps: LL-HLS WebVTT (fMP4 parts) #398, CEA-608 in A53
 * SEI for TS/RTMP #399, DVB subtitles #400 (ADR-0110 §12.3, as built in
 * slice F #345).
 */
export function subtitleCapabilityOf(kind: OutputKind): SubtitleCapability {
  switch (kind) {
    case 'hls':
      return { max: MAX_SUBTITLE_SLOTS, mediumFormats: WEBVTT_ONLY, builtFormats: WEBVTT_ONLY };
    case 'll-hls':
      return { max: MAX_SUBTITLE_SLOTS, mediumFormats: WEBVTT_ONLY, builtFormats: NO_SUBTITLES };
    case 'srt':
    case 'rist':
    case 'rtmp':
      return { max: MAX_SUBTITLE_SLOTS, mediumFormats: TS_SUBTITLES, builtFormats: NO_SUBTITLES };
    default:
      // rtsp, ndi, display, webrtc, whip-push: any subtitle slot is
      // slot-medium-mismatch.
      return { max: 0, mediumFormats: NO_SUBTITLES, builtFormats: NO_SUBTITLES };
  }
}

/**
 * The follow-up issue number for a format the medium carries but the engine
 * does not build yet, if any (shown next to the "not supported yet" text).
 */
export function subtitleFollowUpIssue(kind: OutputKind, format: SubtitleFormat): number | undefined {
  if (kind === 'll-hls' && format === 'webvtt') return 398;
  if ((kind === 'srt' || kind === 'rist' || kind === 'rtmp') && format === 'cea608') return 399;
  if ((kind === 'srt' || kind === 'rist' || kind === 'rtmp') && format === 'dvb') return 400;
  return undefined;
}

function asSubtitleFormat(value: unknown): SubtitleFormat {
  return SUBTITLE_FORMATS.find((f) => f === value) ?? 'webvtt';
}

/** The authored subtitle slots (`extra.slots.subtitle`). */
export function subtitleSlotsOf(form: OutputFormState): readonly SubtitleSlotRow[] {
  const slots = form.extra.slots;
  if (!isRecord(slots) || !Array.isArray(slots.subtitle)) return [];
  return slots.subtitle.filter(isRecord).map((slot) => ({
    format: asSubtitleFormat(slot.format),
    language: typeof slot.language === 'string' ? slot.language : '',
    name: typeof slot.name === 'string' ? slot.name : '',
    source: typeof slot.source === 'string' ? slot.source : undefined,
  }));
}

/** The form with `extra.slots.subtitle` replaced by `rows` (empty removes it). */
export function withSubtitleSlots(
  form: OutputFormState,
  rows: readonly SubtitleSlotRow[],
): OutputFormState {
  const slots = isRecord(form.extra.slots) ? { ...form.extra.slots } : {};
  if (rows.length === 0) {
    delete slots.subtitle;
  } else {
    // Keep the raw authored value: trimming here on every keystroke (via
    // updateRow → withSubtitleSlots → this) would eat a space the instant
    // it was typed (review UI-2) and would silently strip a server-valid
    // name/language that legitimately carries leading/trailing or interior
    // whitespace. Submission-time normalization, if ever wanted, belongs at
    // the save boundary (`outputFormToBody`), not in every edit.
    slots.subtitle = rows.map((row) => {
      const slot: Record<string, unknown> = { format: row.format };
      if (row.language !== '') slot.language = row.language;
      if (row.name !== '') slot.name = row.name;
      if (row.source !== undefined) slot.source = row.source;
      return slot;
    });
  }
  const { slots: _dropped, ...rest } = form.extra;
  void _dropped;
  return Object.keys(slots).length === 0
    ? { ...form, extra: rest }
    : { ...form, extra: { ...rest, slots } };
}

/**
 * A subtitle slot row's effective menu label (name, else language, else the
 * default). Mirrors the server's `SubtitleSlot::effective_name`
 * byte-for-byte: it never trims (`crates/multiview-config/src/
 * output_format.rs`), so this does not either — a stored name/language
 * that legitimately carries leading/trailing whitespace must compare equal
 * to what the server computes.
 */
export function subtitleEffectiveName(row: SubtitleSlotRow, index: number): string {
  if (row.name !== '') return row.name;
  if (row.language !== '') return row.language;
  return `Subtitles ${String(index + 1)}`;
}

/**
 * Whether a Unicode scalar value is a Rust `char::is_control()` control
 * character: C0 (U+0000–U+001F), DEL (U+007F) and C1 (U+0080–U+009F).
 * `crates/multiview-config/src/output_format.rs:833–834` rejects a name
 * containing one of these (plus `"`), so the editor must refuse the exact
 * same set — not just C0 as a naive `\u0000-\u001f` regex would.
 */
function isControlScalar(codePoint: number): boolean {
  return (codePoint >= 0x00 && codePoint <= 0x1f) || codePoint === 0x7f || (codePoint >= 0x80 && codePoint <= 0x9f);
}

/**
 * One row's validation code, mirroring the server's checks
 * (`validate_output_format` / `MAX_SUBTITLE_LANGUAGE_LEN` /
 * `MAX_SUBTITLE_NAME_LEN`): `'language'`, `'name'`, `'format'` (the medium
 * cannot carry it at all, or carries but does not build it yet) or
 * `'duplicate-name'` (another slot's effective name repeats). `undefined` ⇒
 * valid. Never trims: the server validates (and stores) the raw authored
 * value.
 */
export function subtitleSlotError(
  kind: OutputKind,
  rows: readonly SubtitleSlotRow[],
  index: number,
): 'language' | 'name' | 'format' | 'duplicate-name' | undefined {
  const row = rows[index];
  if (row === undefined) return undefined;
  const cap = subtitleCapabilityOf(kind);
  if (!cap.mediumFormats.includes(row.format) || !cap.builtFormats.includes(row.format)) {
    return 'format';
  }
  if (row.language !== '') {
    const language = row.language;
    const ok =
      language.length >= 1 &&
      language.length <= MAX_SUBTITLE_LANGUAGE_LEN &&
      /^[A-Za-z0-9-]+$/.test(language);
    if (!ok) return 'language';
  }
  if (row.name !== '') {
    const name = row.name;
    // Count Unicode scalar values (code points), not UTF-16 units: a
    // spread/Array.from over a string iterates by code point, so a
    // supplementary-plane character (e.g. an emoji outside the BMP) counts
    // as 1, matching Rust's `chars().count()`.
    const scalars = Array.from(name);
    const hasControlOrQuote = scalars.some((c) => {
      const codePoint = c.codePointAt(0);
      return c === '"' || codePoint === undefined || isControlScalar(codePoint);
    });
    const ok = scalars.length <= MAX_SUBTITLE_NAME_LEN && !hasControlOrQuote;
    if (!ok) return 'name';
  }
  const effective = subtitleEffectiveName(row, index);
  const duplicate = rows.some(
    (other, j) => j !== index && subtitleEffectiveName(other, j) === effective,
  );
  if (duplicate) return 'duplicate-name';
  return undefined;
}

/** Whether the output kind's medium carries no subtitles at all (`max === 0`). */
export function subtitleUnsupportedMedium(kind: OutputKind): boolean {
  return subtitleCapabilityOf(kind).max === 0;
}

/** The read-only audio slots (`extra.slots.audio`). */
export function audioSlotsOf(
  form: OutputFormState,
): readonly {
  readonly track: string | undefined;
  readonly language: string | undefined;
  readonly name: string | undefined;
}[] {
  const slots = form.extra.slots;
  if (!isRecord(slots) || !Array.isArray(slots.audio)) return [];
  return slots.audio.filter(isRecord).map((slot) => ({
    track: typeof slot.track === 'string' ? slot.track : undefined,
    language: typeof slot.language === 'string' ? slot.language : undefined,
    name: typeof slot.name === 'string' ? slot.name : undefined,
  }));
}

function parsePositive(value: string): number | undefined {
  if (!/^\d+$/.test(value.trim())) return undefined;
  const n = Number(value.trim());
  return Number.isSafeInteger(n) && n > 0 ? n : undefined;
}

/**
 * The form with `extra.format` set from the fields; both sizes empty removes
 * the format (the output carries the Program canvas).
 */
export function withFormat(
  form: OutputFormState,
  next: { width: string; height: string; mode: FitMode; anchor: Anchor },
): OutputFormState {
  const { format: _dropped, ...rest } = form.extra;
  void _dropped;
  if (next.width.trim() === '' && next.height.trim() === '') {
    return { ...form, extra: rest };
  }
  const previous = isRecord(form.extra.format) ? form.extra.format : {};
  const width = parsePositive(next.width);
  const height = parsePositive(next.height);
  const format: Record<string, unknown> = {
    ...previous,
    width: width ?? next.width,
    height: height ?? next.height,
    fit: next.anchor === 'center' ? { mode: next.mode } : { mode: next.mode, anchor: next.anchor },
  };
  return { ...form, extra: { ...rest, format } };
}

/**
 * The form with video slot 0's bitrate set ('' removes it; an emptied slot
 * list removes `slots`).
 */
export function withBitrate(form: OutputFormState, value: string): OutputFormState {
  const slots = isRecord(form.extra.slots) ? { ...form.extra.slots } : {};
  const video = Array.isArray(slots.video) ? [...(slots.video as unknown[])] : [];
  const slot0 = isRecord(video[0]) ? { ...video[0] } : {};
  const kbps = parsePositive(value);
  if (value.trim() === '') {
    delete slot0.bitrate_kbps;
  } else {
    slot0.bitrate_kbps = kbps ?? value;
  }
  if (Object.keys(slot0).length === 0) {
    video.splice(0, 1);
  } else {
    video[0] = slot0;
  }
  if (video.length === 0) {
    delete slots.video;
  } else {
    slots.video = video;
  }
  const { slots: _dropped, ...rest } = form.extra;
  void _dropped;
  return Object.keys(slots).length === 0
    ? { ...form, extra: rest }
    : { ...form, extra: { ...rest, slots } };
}

/**
 * Validation of the format fields: each size even and 16..=8192, both or
 * neither, the area at most {@link MAX_FORMAT_AREA}; the bitrate
 * 1..=1000000 kbit/s (the config's own limits).
 */
export function formatError(form: OutputFormState): string | undefined {
  const format = formatOf(form);
  if (format !== undefined) {
    const edge = (v: string): boolean => {
      const n = parsePositive(v);
      return n !== undefined && n % 2 === 0 && n >= 16 && n <= 8192;
    };
    if (!edge(format.width) || !edge(format.height)) return 'size';
    const area = Number(format.width) * Number(format.height);
    if (area > MAX_FORMAT_AREA) return 'size';
  }
  const bitrate = bitrateOf(form);
  if (bitrate !== '') {
    const n = parsePositive(bitrate);
    if (n === undefined || n > 1_000_000) return 'bitrate';
  }
  return undefined;
}

