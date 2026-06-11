// The apply-layout engine command (ADR-W019).
//
// `POST /api/v1/commands/apply-layout` with body `{ layout: <layout id> }`
// (crates/multiview-control/src/routes/mod.rs `ApplyLayoutRequest`). The route
// resolves + solves the STORED layout body at request time, so a `202 Accepted`
// (`{ operation_id, kind, applied_live, carried_only }`) is a promise: the
// layout swaps in at the engine's next frame boundary. An unknown id, a body
// that does not parse/solve, or a pinned-canvas (Class-2) mismatch is an honest
// `422` problem whose `detail` names the reason — surface it to the operator
// (`describeApplyError`). The outcome rides the realtime stream as a
// `job.progress` event (ADR-W008, invariant #10 — the request never blocks the
// engine). Unlike the stored-resource CRUD (which applies via config export +
// restart), apply-layout IS a live action.
//
// `submitOperation` in ../api/operations only posts body-less commands, so this
// reuses its header/url/problem helpers with an explicit JSON body.
import { apiUrl, buildHeaders, OperationApiError, readProblem } from '../api/operations';
import type { AcceptedBody, RequestOptions } from '../api/operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isAcceptedBody(value: unknown): value is AcceptedBody {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.kind === 'string'
  );
}

/**
 * Apply the layout with `layoutId` to the running multiview. Resolves with the
 * accepted operation id; rejects with an `OperationApiError` carrying the RFC
 * 9457 problem title/status on refusal (401/403/503).
 */
/**
 * A human-actionable description of an apply-layout failure: the RFC 9457
 * problem `detail` when one was returned (the 422 reason — unknown id,
 * unsolvable body, pinned-canvas mismatch), else the error message.
 */
export function describeApplyError(error: unknown): string {
  if (error instanceof OperationApiError && error.detail !== undefined) {
    return error.detail;
  }
  return error instanceof Error ? error.message : String(error);
}

export async function applyLayoutCommand(
  layoutId: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  const response = await fetch(apiUrl(options, '/api/v1/commands/apply-layout'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify({ layout: layoutId }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isAcceptedBody(body)) {
    throw new OperationApiError('The server returned an unexpected command body.');
  }
  return body;
}
