// Shared contract types for the media-library UI components.
//
// The library surfaces are PRESENTATIONAL and FULLY CONTROLLED (see the
// folder README): no fetch, no persistence, no timers, no state authority of
// any kind. The caller (page + realtime ledger) owns every value rendered
// here and every state transition; the components only render what they are
// given and emit INTENT callbacks. A callback firing is never a claim that
// anything was accepted, applied or saved — the caller's ledger reports
// outcomes back through props (accepted → applied / failed / unknown).
//
// Honesty rules pinned by the evidence audit (library-evidence.md):
//   * Actions exist only where the caller explicitly declares the backend
//     capability `supported`; an unsupported action renders DISABLED with
//     its reason — never a fake "working" or enabled state (§7 SSRF/fetch,
//     §9.10–11).
//   * On-air slots never offer unload-style actions; the reason rides on
//     the control. Saving bytes alone stays available — protecting bytes
//     is always safe (prototype flow rules).
//   * Durations and clip endpoints are EXACT grid boundaries (integer
//     frame/sample indices on a rational grid) — never divided float fps,
//     never float seconds as stored authority (audit §4, §9.7).
//   * A transcript/job/rights value that is not known renders as unknown
//     or not-measured — never NaN, undefined, or an invented precision
//     (audit §9.3, §9.9, §9.11).
import type { AssetDurationSource, RationalBoundary } from '../duration';

/**
 * An explicit capability + availability statement for one action or data
 * source. The caller owns this verdict; the component renders it honestly.
 */
export interface ActionGate {
  /**
   * Does the backend contract for this action/data source actually exist
   * and is it currently usable? `false` renders the control disabled with
   * `unavailableReason` — never a fake enabled/working state.
   */
  readonly supported: boolean;
  /**
   * A localized sentence explaining WHY the action is unavailable, shown
   * as visible text on the disabled control (WCAG 1.4.1 — never a mystery
   * disable). Required when `supported` is false; may also carry a reason
   * for a temporarily blocked-but-implemented action (e.g. a stale link).
   */
  readonly unavailableReason: string | null;
}

/** Rights status of one library asset — unknown is never silently cleared. */
export type LibraryRightsStatus = 'cleared' | 'unknown' | 'expired';

/** Transcript state of one asset (the STT contract is CONCEPT today). */
export type LibraryTranscriptState = 'none' | 'processing' | 'done' | 'failed';

/** The origin/provenance family recorded for the asset's bytes. */
export type LibraryOriginKind =
  | 'captured'
  | 'imported'
  | 'fetched'
  | 'recovered'
  | 'derived'
  | 'unknown';

/**
 * The caller-supplied gate for one row action on one asset. Return
 * `null`/`undefined` to use the action's declared default gate; return a
 * gate to block the action for this asset (e.g. the destination slot is
 * ON AIR). Pure per render — the component never calls it outside render.
 */
export type RowActionGateResolver<Asset> = (
  asset: Asset,
  actionId: string,
) => ActionGate | null | undefined;

/**
 * One controlled filter facet chip group for the browser. Counts are the
 * caller's (computed over the caller's FULL asset set, not the filtered
 * page — the component only renders the rows it is given).
 */
export interface LibraryFacetGroup {
  /** Stable facet id (`type`, `rights`, `tags`…), caller-defined. */
  readonly id: string;
  /** The visible group label (already localized by the caller). */
  readonly label: string;
  readonly options: readonly LibraryFacetOption[];
}

/** One selectable value inside a {@link LibraryFacetGroup}. */
export interface LibraryFacetOption {
  /** Stable option id (e.g. `video`, `cleared`) — echoed in toggle intents. */
  readonly id: string;
  /** The visible chip label (already localized by the caller). */
  readonly label: string;
  /** Matching asset count over the caller's full set; `null` = unknown. */
  readonly count: number | null;
}

/** One controlled sort option for the browser. */
export interface LibrarySortOption {
  /** Stable sort id (`recent`, `name`…), caller-defined. */
  readonly id: string;
  /** The visible label (already localized by the caller). */
  readonly label: string;
}

/**
 * One asset row for the library browser. All display strings are supplied
 * by the caller; measured values come as honest nullable data — the
 * component renders explicit unknown/not-measured fallbacks for `null`.
 */
export interface LibraryAssetSummary {
  readonly id: string;
  readonly name: string;
  readonly kind: 'video' | 'audio' | 'clip';
  /** Provenance line (e.g. `Fetched — youtube · host`); null = unknown. */
  readonly originLabel: string | null;
  /** Measured grid + length for the per-asset duration label; nulls degrade honestly. */
  readonly duration: AssetDurationSource;
  /** Exact byte size, already formatted by the caller; null = unknown. */
  readonly sizeLabel: string | null;
  readonly rights: LibraryRightsStatus;
  /** Only for `expired` rights: the already-localized expiry note. */
  readonly rightsExpiryLabel: string | null;
  readonly transcript: LibraryTranscriptState;
  /** Slots currently holding a reference to this asset (usage chips). */
  readonly inUseSlots: readonly {
    readonly slotName: string;
    /** Whether that slot is currently on air (red usage chip). */
    readonly onAir: boolean;
  }[];
  /**
   * A localized exception note pinned to the row (e.g. the failed
   * transcript reason); null when the row carries no exception.
   */
  readonly exceptionNote: string | null;
}

/** A batch action offered for the browser's current selection. */
export interface LibraryBatchAction {
  readonly id: string;
  /** Visible label (already localized by the caller). */
  readonly label: string;
  /** Capability gate for the whole batch action. */
  readonly gate: ActionGate;
}

/** A per-row action rendered on every browser row (e.g. load/open). */
export interface LibraryRowAction {
  readonly id: string;
  /** Visible label (already localized by the caller). */
  readonly label: string;
  /** Default gate; per-asset overrides come from the gate resolver. */
  readonly gate: ActionGate;
}

/**
 * One word of a transcript with its EXACT grid boundaries. Times live on
 * the ASSET's own media timeline as integer indices on its rational/sample
 * grid — never float seconds (audit §5: float seconds do not round-trip
 * 60000/1001; a text correction must never silently move a timecode).
 */
export interface TranscriptWord {
  /** Stable word id (caller-defined, echoed in selection intents). */
  readonly id: string;
  /** The displayed word text. */
  readonly text: string;
  /** Start boundary on the asset's grid; null = unaligned word. */
  readonly startBoundary: RationalBoundary | null;
  /** End boundary on the asset's grid; null = unaligned word. */
  readonly endBoundary: RationalBoundary | null;
  /** Optional 0..1 confidence from the STT provider; null = not reported. */
  readonly confidence: number | null;
  /** Speaker label already localized by the caller; null = unlabeled. */
  readonly speakerLabel: string | null;
  /** Segment grouping id; words sharing it render under one header; null = ungrouped. */
  readonly segmentId: string | null;
}

/**
 * A controlled word selection, matching the approved tap-tap-range flow:
 * `endWordId === null` is an ANCHOR ONLY (one word selected); a non-null
 * `endWordId` is the RANGE anchor..end (inclusive, order-free).
 */
export interface TranscriptSelection {
  /** The anchor word (the first tapped / keyboard anchor). */
  readonly startWordId: string;
  /** The extend target; `null` = anchor only, no range yet. */
  readonly endWordId: string | null;
}
