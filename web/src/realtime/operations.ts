// Operation correlation (SW-19): a 202 `AcceptedBody.operation_id` is resolved by
// the realtime envelope whose `corr` equals it (authoritative completion), with a
// timeout rejection.
//
// Ordering: the outcome can beat the POST response over the wire, so every
// correlated envelope is also kept in a small bounded cache (capacity + TTL) and
// consulted at registration. A `kind: "replay"` acceptance (replayed
// Idempotency-Key) is NOT a completion: it resolves from a cached authoritative
// outcome if one exists, otherwise waits for the matching `corr` like a fresh
// acceptance (flagged `replayed`).
import type { AcceptedBody } from "../api/operations";
import type { Envelope } from "./envelope";

export interface OperationOutcome {
  readonly operationId: string;
  /** The completing envelope. */
  readonly envelope: Envelope;
  /** True when the acceptance was an Idempotency-Key replay. */
  readonly replayed: boolean;
}

interface Pending {
  readonly resolve: (outcome: OperationOutcome) => void;
  readonly timer: ReturnType<typeof setTimeout>;
  readonly replayed: boolean;
}

const EARLY_CAPACITY = 256;
const EARLY_TTL_MS = 60_000;

const pending = new Map<string, Pending>();
/** Insertion-ordered: the first key is the oldest. */
const early = new Map<string, { readonly envelope: Envelope; readonly at: number }>();

function takeEarly(id: string): Envelope | undefined {
  const entry = early.get(id);
  if (entry === undefined) {
    return undefined;
  }
  early.delete(id);
  return Date.now() - entry.at <= EARLY_TTL_MS ? entry.envelope : undefined;
}

function rememberEarly(id: string, envelope: Envelope): void {
  const now = Date.now();
  for (const [key, entry] of early) {
    if (now - entry.at <= EARLY_TTL_MS) {
      break;
    }
    early.delete(key);
  }
  early.delete(id);
  early.set(id, { envelope, at: now });
  while (early.size > EARLY_CAPACITY) {
    const oldest = early.keys().next();
    if (oldest.done === true) {
      break;
    }
    early.delete(oldest.value);
  }
}

/** Register the accepted operation id and await its authoritative envelope. */
export function correlateOperation(
  accepted: AcceptedBody,
  timeoutMs = 60_000,
): Promise<OperationOutcome> {
  const id = accepted.operation_id;
  const replayed = accepted.kind === "replay";
  const cached = takeEarly(id);
  if (cached !== undefined) {
    return Promise.resolve({ operationId: id, envelope: cached, replayed });
  }
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`Operation ${id} outcome timed out`));
    }, timeoutMs);
    pending.set(id, { resolve, timer, replayed });
  });
}

/** Match an envelope's `corr` to a pending operation; true when it resolved one. */
export function resolveOperationEnvelope(envelope: Envelope): boolean {
  const corr = envelope.corr;
  if (typeof corr !== "string") {
    return false;
  }
  const entry = pending.get(corr);
  if (entry === undefined) {
    rememberEarly(corr, envelope);
    return false;
  }
  pending.delete(corr);
  clearTimeout(entry.timer);
  entry.resolve({ operationId: corr, envelope, replayed: entry.replayed });
  return true;
}
