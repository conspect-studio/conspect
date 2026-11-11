// Legacy operation-correlation compatibility: a corr alone is not completion.
// Only the QueryClient-scoped operation tracker can match command-specific
// terminal evidence. Keep these exports so older callers do not claim success.
import type { AcceptedBody } from '../api/operations';
import type { Envelope } from './envelope';

export interface OperationOutcome {
  readonly operationId: string;
  readonly envelope: Envelope;
  readonly replayed: boolean;
}

/** No generic waiter can decide whether an event satisfies an unknown command. */
export function correlateOperation(_accepted: AcceptedBody, timeoutMs = 60_000): Promise<OperationOutcome> {
  void timeoutMs;
  const unsupported = Promise.reject<OperationOutcome>(new Error('Command-specific terminal expectation required; outcome unknown'));
  // Legacy callers sometimes ignore completion; mark this rejection handled.
  void unsupported.catch(() => undefined);
  return unsupported;
}

/** The shared socket dispatches only; the tracker is the sole outcome matcher. */
export function resolveOperationEnvelope(envelope: Envelope): boolean {
  void envelope;
  return false;
}
