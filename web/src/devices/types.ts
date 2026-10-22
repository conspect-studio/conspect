// Typed view-models for the managed-devices domain (managed-devices.md,
// ADR-M008/M009/M010).
//
// Devices and sync groups are `{ id, name, body }` resource records exactly
// like sources/outputs (the control plane validates the body against
// `multiview_config::Device` / `SyncGroup` with 422, ADR-W015). The
// view-models below are the display projections derived with typed field
// guards (see `./api.ts`), never `as`-casts. Runtime status is a SEPARATE
// read-only lane (the conflated `device.status` realtime topic + the
// `/devices/{id}/status` REST fallback) and never lives on these records.

/** The compiled-in device-driver families (`multiview_config::DeviceDriver`). */
export type DeviceDriver = 'zowietek' | 'displaynode' | 'cast';

/** All adoptable drivers, for building selectors. */
export const DEVICE_DRIVERS: readonly DeviceDriver[] = [
  'zowietek',
  'displaynode',
  'cast',
];

/**
 * A managed device as stored: the declarative desired state. The `driver`
 * folds an unknown tag to `'unknown'` for typed consumers — never silently to
 * a real driver — while `rawDriver` carries the authored tag for display.
 */
export interface DeviceView {
  /** Stable device id (referenced by sync-group members and `device_ref`). */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Driver family, folded for typed consumers (`'unknown'` when unrecognized). */
  readonly driver: DeviceDriver | 'unknown';
  /** The driver tag exactly as authored in the stored body. */
  readonly rawDriver: string;
  /** Management address (IPv6-first), when authored. */
  readonly address: string | undefined;
  /** The desired converged work mode (driver vocabulary), when authored. */
  readonly desiredMode: string | undefined;
  /** Whether this UI can edit the record (its driver has a typed form). */
  readonly editable: boolean;
}

/** One sync-group member as stored (`{ device, offset_ms }`). */
export interface SyncMemberView {
  /** The member device id. */
  readonly device: string;
  /** Per-member presentation offset trim (ms, AES67 link-offset semantics). */
  readonly offsetMs: number;
}

/** A synchronized presentation group as stored. */
export interface SyncGroupView {
  /** Stable sync-group id. */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Drift-alarm threshold in milliseconds (`1..=10_000`). */
  readonly targetSkewMs: number;
  /** The member devices with their offsets. */
  readonly members: readonly SyncMemberView[];
  /** Whether this UI can edit the record. */
  readonly editable: boolean;
}

/**
 * The sync tier a group can honestly claim (managed-devices.md §8): our
 * display nodes are frame-accurate, vendor decoders hold a bounded skew
 * (±100–500 ms drift between re-aligns), and cast-class devices are never
 * part of a synchronized canvas. Claims fold to the WEAKEST member and are
 * never over-claimed; the MEASURED tier comes from the runtime status lane.
 */
export type SyncTier = 'frame-accurate' | 'bounded-skew' | 'none';
