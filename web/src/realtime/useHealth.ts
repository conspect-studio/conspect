// useHealth — the React binding over the `alerts` realtime topic for SA-0 health
// warnings (ADR-0035).
//
// The engine publishes `health.warning.raised` / `health.warning.cleared` events
// on the `alerts` lane (a richer sibling of operator alerts). This hook subscribes
// the same self-reconnecting, non-blocking way `useSystemMetrics` does and folds
// every warning into a BOUNDED map keyed by its stable `code` (raise upserts,
// clear removes). The engine is isolated (invariant #10): a slow UI only loses its
// own frames and can never back-pressure the engine, and the client map never
// grows past the number of distinct catalog codes.
import { useEffect, useState } from "react";

import { getStoredToken } from "../api/token";
import { RealtimeConnection } from "./connection";
import type { RealtimeStatus } from "./connection";

/** The severity classes a health warning carries (mirrors `WarningSeverity`). */
export type WarningSeverity = "info" | "warning" | "critical";

const WARNING_SEVERITIES: readonly WarningSeverity[] = [
  "info",
  "warning",
  "critical",
];

/** An actionable health warning (mirrors the Rust `HealthWarning`). */
export interface HealthWarning {
  /** The stable catalog code (kebab-case), e.g. `gpu-present-no-vulkan-adapter`. */
  readonly code: string;
  /** The severity. */
  readonly severity: WarningSeverity;
  /** The affected subsystem (e.g. `compositor`). */
  readonly subsystem: string;
  /** A clear, human-readable description of the condition. */
  readonly message: string;
  /** The concrete remediation — what the operator must do to fix it. */
  readonly remediation: string;
  /** When the condition was first raised (engine monotonic nanoseconds). */
  readonly since: number;
  /** Whether the condition is currently active. */
  readonly active: boolean;
}

/** The two realtime event types health warnings ride. */
export const HEALTH_WARNING_RAISED = "health.warning.raised";
export const HEALTH_WARNING_CLEARED = "health.warning.cleared";

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function severityFrom(value: unknown): WarningSeverity {
  // An unknown/absent severity maps to "warning" (forward-compatible with the
  // `#[non_exhaustive]` Rust enum) rather than dropping the warning.
  return WARNING_SEVERITIES.find((s) => s === value) ?? "warning";
}

/**
 * Defensively narrow an envelope `data` body to a {@link HealthWarning}. Returns
 * `undefined` when a required string/number field is missing or mistyped, so a
 * malformed frame is dropped rather than rendered as a broken banner.
 */
export function parseHealthWarning(data: unknown): HealthWarning | undefined {
  if (!isRecord(data)) {
    return undefined;
  }
  const { code, subsystem, message, remediation, since, active } = data;
  if (
    typeof code !== "string" ||
    typeof subsystem !== "string" ||
    typeof message !== "string" ||
    typeof remediation !== "string" ||
    typeof since !== "number" ||
    !Number.isFinite(since) ||
    typeof active !== "boolean"
  ) {
    return undefined;
  }
  return {
    code,
    severity: severityFrom(data.severity),
    subsystem,
    message,
    remediation,
    since,
    active,
  };
}

/**
 * A bounded map of active health warnings keyed by their stable `code`.
 *
 * A `raised` (active) warning upserts its entry (coalescing on code — a latched
 * warning cannot stack); a `cleared` (or `active: false`) warning removes it.
 * Because the map is keyed by the finite catalog of codes it can never grow
 * unbounded, matching the engine-side latched/dedupe semantics (inv #10).
 */
export class HealthWarningMap {
  #byCode = new Map<string, HealthWarning>();

  /** Fold one warning in: active → upsert, inactive → remove. */
  apply(warning: HealthWarning): void {
    if (warning.active) {
      this.#byCode.set(warning.code, warning);
    } else {
      this.#byCode.delete(warning.code);
    }
  }

  /**
   * Apply an envelope's parsed warning, keyed off the event type so a
   * `health.warning.cleared` always removes even if the carried `active` flag
   * were stale. Returns `true` if the frame was a (well-formed) health warning.
   */
  applyEnvelope(eventType: string, data: unknown): boolean {
    if (
      eventType !== HEALTH_WARNING_RAISED &&
      eventType !== HEALTH_WARNING_CLEARED
    ) {
      return false;
    }
    const warning = parseHealthWarning(data);
    if (warning === undefined) {
      return false;
    }
    // A `cleared` event always removes, regardless of the carried `active` flag.
    this.apply(
      eventType === HEALTH_WARNING_CLEARED
        ? { ...warning, active: false }
        : warning,
    );
    return true;
  }

  /** The active warnings, sorted by code for a stable, flicker-free order. */
  active(): HealthWarning[] {
    return [...this.#byCode.values()].sort((a, b) =>
      a.code.localeCompare(b.code),
    );
  }
}

/** The realtime topic health warnings ride (the operator-alert lane). */
export const ALERTS_TOPIC = "alerts";

function resolveWsUrl(): string {
  // Same-origin: the dev proxy and the embedded build both serve `/api/v1/ws`.
  const { protocol, host } = window.location;
  const wsProtocol = protocol === "https:" ? "wss:" : "ws:";
  const base = `${wsProtocol}//${host}/api/v1/ws`;
  // A browser WebSocket cannot send an Authorization header; the control plane
  // also accepts the bearer token as an `access_token` query parameter.
  const token = getStoredToken();
  return token === undefined
    ? base
    : `${base}?access_token=${encodeURIComponent(token)}`;
}

/** What {@link useHealth} returns. */
export interface HealthState {
  /** The active warnings (empty on a clean host → the banner renders nothing). */
  readonly warnings: readonly HealthWarning[];
  /** Coarse realtime connection status. */
  readonly status: RealtimeStatus;
}

/**
 * Subscribe to the engine's `alerts` lane and keep the live set of active health
 * warnings (SA-0). Returns the active warnings + the connection status. The hook
 * never blocks render: the socket lives in the effect, and warnings land via
 * `setState` from the (cheap, non-blocking) frame callback.
 */
export function useHealth(): HealthState {
  const [status, setStatus] = useState<RealtimeStatus>("connecting");
  const [warnings, setWarnings] = useState<readonly HealthWarning[]>([]);

  useEffect(() => {
    const map = new HealthWarningMap();
    const connection = new RealtimeConnection(resolveWsUrl(), {
      onStatus: (next): void => {
        setStatus(next);
      },
      onEnvelope: (envelope): void => {
        if (map.applyEnvelope(envelope.t, envelope.data)) {
          setWarnings(map.active());
        }
      },
    });
    connection.start();
    return (): void => {
      connection.stop();
    };
  }, []);

  return { warnings, status };
}
