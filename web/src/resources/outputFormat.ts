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
 * Output kinds whose format/bitrate change resets the output live (in-band
 * parameter sets: TS segments, SRT, RIST). Everything else applies on the
 * next restart.
 */
export function resetsLive(kind: OutputKind): boolean {
  return kind === 'hls' || kind === 'srt' || kind === 'rist';
}

/** Kinds without an encoded video slot (raw frames or audio only). */
export function hasEncodedVideo(kind: OutputKind): boolean {
  // NDI and the display head carry raw frames (no encoded video slot).
  return kind !== 'ndi' && kind !== 'display';
}

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

/** The read-only audio slots (`extra.slots.audio`). */
export function audioSlotsOf(
  form: OutputFormState,
): readonly { readonly track: string | undefined; readonly language: string | undefined }[] {
  const slots = form.extra.slots;
  if (!isRecord(slots) || !Array.isArray(slots.audio)) return [];
  return slots.audio.filter(isRecord).map((slot) => ({
    track: typeof slot.track === 'string' ? slot.track : undefined,
    language: typeof slot.language === 'string' ? slot.language : undefined,
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
 * neither; the bitrate 1..=1000000 kbit/s (the config's own limits).
 */
export function formatError(form: OutputFormState): string | undefined {
  const format = formatOf(form);
  if (format !== undefined) {
    const edge = (v: string): boolean => {
      const n = parsePositive(v);
      return n !== undefined && n % 2 === 0 && n >= 16 && n <= 8192;
    };
    if (!edge(format.width) || !edge(format.height)) return 'size';
  }
  const bitrate = bitrateOf(form);
  if (bitrate !== '') {
    const n = parsePositive(bitrate);
    if (n === undefined || n > 1_000_000) return 'bitrate';
  }
  return undefined;
}

