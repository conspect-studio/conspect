// Typed view-models for the resource surfaces the SPA manages alongside layouts.
//
// Sources, Outputs, and Overlays are first-class control-plane REST resources,
// each stored as a `{ id, name, body }` record where `body` is the opaque,
// validated config document (`multiview-config`: `Source`, `Output`, `Overlay`).
// The control plane stores and version-stamps the body without the SPA having to
// model every kind's fields. The view-models below are the small, display-facing
// projections the read views and the layout-editor palette render; they are
// derived from the opaque body with typed field guards (see `./api.ts`), never
// `as`-casts.

/** The resource collections the SPA manages (the REST path segment). */
export type ResourceKind = 'sources' | 'outputs' | 'overlays' | 'probes';

/**
 * A persisted resource record, exactly as the control plane returns it: a stable
 * id, an operator label, and the opaque config `body`.
 */
export interface ResourceRecord {
  /** Stable resource id. */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** The opaque, validated config document. */
  readonly body: Record<string, unknown>;
}

/** The create/update payload accepted by the control plane. */
export interface ResourceInput {
  /** Operator label. */
  readonly name: string;
  /** The config document to store. */
  readonly body: Record<string, unknown>;
}

/**
 * The transport kinds Multiview can ingest (config `SourceKind`, internally
 * tagged by `kind`). These tags are the literal config wire kinds (snake_case),
 * so they double as the `kind` field written into the body.
 */
export type SourceKind =
  | 'bars'
  | 'solid'
  | 'clock'
  | 'timer'
  | 'rtsp'
  | 'hls'
  | 'youtube'
  | 'ts'
  | 'srt'
  | 'rtmp'
  | 'ndi'
  | 'file'
  // `test` is a legacy alias for `bars` (ADR-0027); kept in the union for
  // back-compat parsing of older bodies, but not offered in the picker.
  | 'test';

/**
 * The user-pickable source kinds, for building selectors. Excludes the legacy
 * `test` alias (folded to `bars`); `test` stays in {@link SourceKind} so older
 * bodies still parse.
 */
export const SOURCE_KINDS: readonly SourceKind[] = [
  'bars',
  'solid',
  'clock',
  'timer',
  'rtsp',
  'hls',
  'youtube',
  'ts',
  'srt',
  'rtmp',
  'ndi',
  'file',
];

/** A managed ingest source. */
export interface SourceView {
  /** Stable source id (referenced by a cell's `input_id`). */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Transport kind. */
  readonly kind: SourceKind;
  /**
   * The kind tag exactly as authored in the stored body (e.g. `aes67` for a
   * kind this UI has no form for). Display this, never the folded `kind`, so
   * an unknown-kind document is shown as it is.
   */
  readonly rawKind: string;
  /**
   * Whether this UI can edit the record (its kind has a typed form). An
   * unknown kind renders + deletes normally but Edit is refused — editing
   * through a fold would silently rewrite the authored document.
   */
  readonly editable: boolean;
  /**
   * The configured locator for display — the kind's key field: `url` for the
   * network kinds, the source `name` for NDI, the `path` for file. Absent for
   * the synthetic kinds (`bars`/`solid`/`clock`/`timer`, and the legacy `test`
   * alias), which carry no locator.
   */
  readonly locator: string | undefined;
}

/** The output transport display kinds (config `Output`, folded for display). */
export type OutputKind = 'rtsp' | 'hls' | 'll-hls' | 'ndi' | 'rtmp' | 'srt' | 'display';

/** All output kinds, for building selectors. */
export const OUTPUT_KINDS: readonly OutputKind[] = [
  'rtsp',
  'hls',
  'll-hls',
  'ndi',
  'rtmp',
  'srt',
  'display',
];

/** A configured output sink/server. */
export interface OutputView {
  /** Stable output id. */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Output transport kind. */
  readonly kind: OutputKind;
  /** The kind tag exactly as authored in the stored body (wire form). */
  readonly rawKind: string;
  /** Whether this UI can edit the record (see {@link SourceView.editable}). */
  readonly editable: boolean;
  /**
   * The kind's key field for display: the RTSP `mount`, the HLS/LL-HLS `path`,
   * the RTMP/SRT `url`, the NDI source `name`, or the display `connector`.
   */
  readonly target: string | undefined;
  /**
   * The video codec (absent for NDI and display, which carry raw frames, not
   * an encoded rendition).
   */
  readonly codec: string | undefined;
}

/** The overlay kinds (config `[[overlays]]` `kind`). */
export type OverlayKind = 'clock' | 'label' | 'tally_border' | 'image' | 'subtitle';

/** All overlay kinds, for building selectors. */
export const OVERLAY_KINDS: readonly OverlayKind[] = [
  'clock',
  'label',
  'tally_border',
  'image',
  'subtitle',
];

/**
 * The per-cell fail-state probe kinds (config `ProbeKind`, internally tagged
 * by `kind`, flattened into the probe body). These tags are the literal config
 * wire kinds (snake_case).
 */
export type ProbeKind = 'black' | 'freeze' | 'silence' | 'loudness';

/** All probe kinds, for building selectors. */
export const PROBE_KINDS: readonly ProbeKind[] = ['black', 'freeze', 'silence', 'loudness'];

/** A configured per-cell fail-state probe. */
export interface ProbeView {
  /** Stable probe id. */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Probe kind (folded for typed consumers). */
  readonly kind: ProbeKind;
  /** The kind tag exactly as authored in the stored body. */
  readonly rawKind: string;
  /** Whether this UI can edit the record (see {@link SourceView.editable}). */
  readonly editable: boolean;
  /** The cell id the probe watches. */
  readonly cell: string;
  /** The X.733 perceived severity the probe asserts (wire form, PascalCase). */
  readonly severity: string;
  /** Whether the alarm latches until explicitly reset. */
  readonly latched: boolean;
}

/** A configured overlay layer. */
export interface OverlayView {
  /** Stable overlay id (also draggable onto a cell from the palette). */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Overlay kind. */
  readonly kind: OverlayKind;
  /** The kind tag exactly as authored in the stored body. */
  readonly rawKind: string;
  /** Whether this UI can edit the record (see {@link SourceView.editable}). */
  readonly editable: boolean;
  /** Attachment target (`canvas` or a cell id). */
  readonly target: string;
  /** Stacking order over the program. */
  readonly z: number;
}
