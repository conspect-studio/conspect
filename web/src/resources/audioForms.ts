// Pure form state + mapping + validation for the AUDIO ROUTING document.
//
// The Audio page manages the singleton `[audio]` block
// (crates/multiview-config/src/audio.rs, served at /api/v1/audio-routing):
// a working sample rate plus one route per input declaring program-bus
// membership (include/gain/mute) and an optional named discrete track. The
// per-output audio selection (Outputs page) resolves against the tracks this
// document declares, plus the always-available program bus "prog".
//
// Everything here is pure (no React, no fetch) so it unit-tests directly; the
// validator mirrors the control plane's 422 rules so problems surface inline
// before a PUT ever leaves the browser.
import type { FieldErrors, FormErrorCode } from './forms';

/** The channel-layout wire tags (`channels.kind`, internally tagged). */
export type AudioChannelsKind = 'mono' | 'stereo' | 'five_point_one';

/** The user-pickable channel layouts, in display order. */
export const AUDIO_CHANNELS: readonly AudioChannelsKind[] = [
  'mono',
  'stereo',
  'five_point_one',
];

/** The reserved name of the mixed program bus (always selectable). */
export const PROGRAM_TRACK = 'prog';

/** One per-input route as it rides the wire (matches `AudioRoute`). */
export interface AudioRouteDocument {
  /** The managed source id this route takes audio from. */
  readonly input_id: string;
  /** The requested channel layout (internally tagged by `kind`). */
  readonly channels: { readonly kind: AudioChannelsKind };
  /** The named discrete output track (absent = program bus only). */
  readonly target_track?: string;
  /** ISO-639 language tag advertised for the discrete track. */
  readonly language?: string;
  /** Human-friendly track title. */
  readonly title?: string;
  /** Whether this input contributes to the mixed program bus. */
  readonly include_in_program_bus: boolean;
  /** Program-bus contribution gain in dB (0 = unity). */
  readonly gain_db: number;
  /** Whether this input is muted on the program bus. */
  readonly mute: boolean;
}

/** The whole routing document as it rides the wire (matches `AudioRouting`). */
export interface AudioRoutingDocument {
  /** The working/program-bus sample rate in Hz (exact integer, > 0). */
  readonly sample_rate_hz: number;
  /** The per-input routes. */
  readonly routes: readonly AudioRouteDocument[];
}

/** One editable route row (form strings; numbers parse at submit). */
export interface AudioRouteRow {
  /** The source id feeding this route. */
  readonly inputId: string;
  /** The requested channel layout. */
  readonly channels: AudioChannelsKind;
  /** The discrete track name ('' = contributes to the program mix only). */
  readonly targetTrack: string;
  /** ISO-639 language hint for the discrete track ('' = none). */
  readonly language: string;
  /** Track title ('' = none). */
  readonly title: string;
  /** Whether the input joins the mixed program bus. */
  readonly includeInProgramBus: boolean;
  /** Program-bus gain in dB as typed ('' = unity / 0 dB). */
  readonly gainDb: string;
  /** Whether the input is muted on the program bus. */
  readonly mute: boolean;
}

/** The whole-document form state. */
export interface AudioFormState {
  /** The working sample rate in Hz as typed. */
  readonly sampleRateHz: string;
  /** The route rows, in document order. */
  readonly routes: readonly AudioRouteRow[];
}

/** The per-route fields the validator can flag. */
export type AudioRouteField = 'inputId' | 'targetTrack' | 'gainDb';

/** The validation outcome: document-level + one error map per route row. */
export interface AudioFormErrors {
  /** The sample-rate error, if any. */
  readonly sampleRateHz?: FormErrorCode;
  /** Whole-mix error: the program bus has members but every one is muted. */
  readonly program?: FormErrorCode;
  /** Per-row field errors, index-aligned with the form's routes. */
  readonly routes: readonly FieldErrors<AudioRouteField>[];
  /** Whether anything above is set. */
  readonly hasErrors: boolean;
}

/** The default sample rate (ADR-R005's canonical resample target). */
export const DEFAULT_SAMPLE_RATE_HZ = 48000;

/** A fresh, empty form (48 kHz, no routes). */
export function emptyAudioForm(): AudioFormState {
  return { sampleRateHz: String(DEFAULT_SAMPLE_RATE_HZ), routes: [] };
}

/** A fresh route row (stereo, program mix off, unity gain). */
export function emptyAudioRoute(): AudioRouteRow {
  return {
    inputId: '',
    channels: 'stereo',
    targetTrack: '',
    language: '',
    title: '',
    includeInProgramBus: false,
    gainDb: '0',
    mute: false,
  };
}

/** Map the wire document (or `null` when unconfigured) onto the form. */
export function audioFormFromDocument(
  document: AudioRoutingDocument | null,
): AudioFormState {
  if (document === null) {
    return emptyAudioForm();
  }
  return {
    sampleRateHz: String(document.sample_rate_hz),
    routes: document.routes.map(
      (route): AudioRouteRow => ({
        inputId: route.input_id,
        channels: route.channels.kind,
        targetTrack: route.target_track ?? '',
        language: route.language ?? '',
        title: route.title ?? '',
        includeInProgramBus: route.include_in_program_bus,
        gainDb: String(route.gain_db),
        mute: route.mute,
      }),
    ),
  };
}

/** Parse a strict integer string, or `undefined` when not a whole number. */
function parseIntStrict(value: string): number | undefined {
  const trimmed = value.trim();
  if (!/^-?\d+$/.test(trimmed)) {
    return undefined;
  }
  const parsed = Number.parseInt(trimmed, 10);
  return Number.isSafeInteger(parsed) ? parsed : undefined;
}

/** Parse a finite number, or `undefined` ('' parses as unity, 0). */
function parseGain(value: string): number | undefined {
  const trimmed = value.trim();
  if (trimmed === '') {
    return 0;
  }
  const parsed = Number(trimmed);
  // The wire type is an f32: reject magnitudes that overflow it to infinity
  // server-side even though they are finite in JS's f64.
  const F32_MAX = 3.4028235e38;
  return Number.isFinite(parsed) && Math.abs(parsed) <= F32_MAX ? parsed : undefined;
}

/** Map the validated form back onto the wire document (blank optionals omitted). */
export function audioFormToDocument(form: AudioFormState): AudioRoutingDocument {
  return {
    sample_rate_hz: parseIntStrict(form.sampleRateHz) ?? 0,
    routes: form.routes.map((route): AudioRouteDocument => {
      const targetTrack = route.targetTrack.trim();
      const language = route.language.trim();
      const title = route.title.trim();
      return {
        input_id: route.inputId.trim(),
        channels: { kind: route.channels },
        ...(targetTrack !== '' ? { target_track: targetTrack } : {}),
        ...(language !== '' ? { language } : {}),
        ...(title !== '' ? { title } : {}),
        include_in_program_bus: route.includeInProgramBus,
        gain_db: parseGain(route.gainDb) ?? 0,
        mute: route.mute,
      };
    }),
  };
}

/**
 * The selectable tracks the form currently declares: the program bus `"prog"`
 * (always first) plus every distinct non-blank track name in declaration
 * order — the exact set per-output `audio.tracks` selections resolve against.
 */
export function declaredTracks(form: AudioFormState): readonly string[] {
  const tracks: string[] = [PROGRAM_TRACK];
  for (const route of form.routes) {
    const track = route.targetTrack.trim();
    if (track !== '' && !tracks.includes(track)) {
      tracks.push(track);
    }
  }
  return tracks;
}

/**
 * Validate the form, mirroring the control plane's rules
 * (`AudioRouting::validate`): a positive integer sample rate; per route a
 * non-blank, non-duplicate input, a finite gain, and a track name that is
 * neither the reserved `"prog"` nor claimed by an earlier route; and a program
 * mix that is not entirely muted. Cross-checks against the declared sources
 * happen at config export, not here.
 */
export function validateAudioForm(form: AudioFormState): AudioFormErrors {
  const sampleRate = ((): FormErrorCode | undefined => {
    if (form.sampleRateHz.trim() === '') {
      return 'required';
    }
    const parsed = parseIntStrict(form.sampleRateHz);
    return parsed === undefined || parsed <= 0 ? 'positive-int' : undefined;
  })();

  const seenInputs = new Set<string>();
  const seenTracks = new Set<string>();

  const routes = form.routes.map((route): FieldErrors<AudioRouteField> => {
    const errors: Partial<Record<AudioRouteField, FormErrorCode>> = {};

    const inputId = route.inputId.trim();
    if (inputId === '') {
      errors.inputId = 'required';
    } else if (seenInputs.has(inputId)) {
      errors.inputId = 'duplicate-input';
    } else {
      seenInputs.add(inputId);
    }

    if (parseGain(route.gainDb) === undefined) {
      errors.gainDb = 'finite-number';
    }

    const track = route.targetTrack.trim();
    if (track === PROGRAM_TRACK) {
      errors.targetTrack = 'reserved-track';
    } else if (track !== '') {
      if (seenTracks.has(track)) {
        errors.targetTrack = 'duplicate-track';
      } else {
        seenTracks.add(track);
      }
    }
    return errors;
  });

  // An all-muted program mix is a silent program an operator almost never
  // intends (mirrors `AudioRouting::validate`).
  const members = form.routes.filter((route) => route.includeInProgramBus);
  const program: FormErrorCode | undefined =
    members.length > 0 && members.every((route) => route.mute)
      ? 'program-bus-muted'
      : undefined;

  const hasErrors =
    sampleRate !== undefined ||
    program !== undefined ||
    routes.some((errors) => Object.keys(errors).length > 0);

  return {
    ...(sampleRate !== undefined ? { sampleRateHz: sampleRate } : {}),
    ...(program !== undefined ? { program } : {}),
    routes,
    hasErrors,
  };
}
