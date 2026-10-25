// Operation correlation (SW-19): a 202 `AcceptedBody.operation_id` is resolved by
// the realtime envelope whose `corr` equals it (authoritative completion), with a
// timeout rejection. A `kind: "replay"` acceptance (replayed Idempotency-Key)
// names an operation whose outcome may already have been delivered, so it
// resolves immediately as `replayed` rather than waiting for an event that will
// not recur.
import type { AcceptedBody } from "../api/operations";
import type { Envelope } from "./envelope";

export interface OperationOutcome {
  readonly operationId: string;
  /** The completing envelope; absent for a replayed acceptance. */
  readonly envelope?: Envelope;
  readonly replayed: boolean;
}

interface Pending {
  readonly resolve: (outcome: OperationOutcome) => void;
  readonly timer: ReturnType<typeof setTimeout>;
}

const pending = new Map<string, Pending>();

/** Register the accepted operation id and await its authoritative envelope. */
export function correlateOperation(
  accepted: AcceptedBody,
  timeoutMs = 60_000,
): Promise<OperationOutcome> {
  if (accepted.kind === "replay") {
    return Promise.resolve({ operationId: accepted.operation_id, replayed: true });
  }
  return new Promise((resolve, reject) => {
    const id = accepted.operation_id;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`Operation ${id} outcome timed out`));
    }, timeoutMs);
    pending.set(id, { resolve, timer });
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
    return false;
  }
  pending.delete(corr);
  clearTimeout(entry.timer);
  entry.resolve({ operationId: corr, envelope, replayed: false });
  return true;
}
