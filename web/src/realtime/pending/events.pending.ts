/**
 * PENDING-BACKEND EVENT TYPES — DO NOT EDIT BY HAND.
 *
 * Types for realtime events that main's docs/api/asyncapi.json does NOT yet
 * declare (or declares narrower). Generated from
 * scripts/pending-asyncapi.superset.json by scripts/gen-pending-events.mjs
 * (npm run generate:pending-events). DELETE this file and regenerate
 * generated-types.ts once the backend PRs listed in
 * src/api/pending/BACKEND_DEPENDENCIES.md merge.
 */

/** Data body of `catalogue.link_status` (IPTV-ORG, ADR-M015): a linked source's catalogue link status changed. Envelope `id` = source id. */
export interface CatalogueLinkStatus {
  readonly channel?: string;
  readonly feed?: string;
  /** Previous status, when known. */
  readonly previous?: string;
  /** Replacement channel id (channel_closed). */
  readonly replaced_by?: string;
  /** The linked source id. */
  readonly source_id: string;
  readonly status: "in_sync" | "updated" | "stream_removed" | "channel_closed" | "blocklisted" | "unlinked" | "edited";
}

/** Data body of `graphics.channel_state`: a graphics playout channel's preview/program after a cue / take / clear / next. Envelope `id` = channel id. */
export interface GraphicsChannelEvent {
  /** The verb that produced this state: cue, take, clear, next or rundown. */
  readonly action: string;
  /** Graphics channel id (matches the envelope `id`). */
  readonly channel: string;
  /** Cued (preview) item id, if any. */
  readonly preview?: string;
  /** On-air (program) item id, if any. */
  readonly program?: string;
  /** Increments on every program change. */
  readonly seq: number;
}

/** Data body of the `input.lifecycle` event. */
export interface InputLifecycle {
  /** Unix milliseconds. */
  readonly at_ms: number;
  readonly from: "idle" | "starting" | "ready" | "failed";
  /** Start generation; increments on every start from idle. */
  readonly generation: number;
  readonly lease_holders?: readonly {
    readonly kind: string;
    readonly label: string;
  }[];
  /** Operator-safe reason. */
  readonly reason: string;
  /** The source id (the authorization object). */
  readonly source_id: string;
  readonly to: "idle" | "starting" | "ready" | "failed";
}

/** Running state of an output sink. */
export type OutputRunState = "idle" | "starting" | "running" | "migrating" | "error";

/** Terminal outcome, emitted only at the engine output tick; envelope corr equals operation_id, id equals M/E id. */
export interface SwitcherOperation {
  readonly completed: boolean;
  readonly error?: string;
  readonly me: string;
  readonly operation_id: string;
  readonly program: string;
  readonly tick: number;
}

/** Stable catalog code of a health warning (kebab-case). `#[non_exhaustive]`: the catalog grows over time, so a client must treat an unknown code as a forward-compatible warning, not an error. */
export type WarningCode = "gpu-present-no-vulkan-adapter" | "config-file-invalid" | "config-file-requires-restart" | "config-file-apply-incomplete" | "container-no-host-networking" | "catalogue-link-attention";
