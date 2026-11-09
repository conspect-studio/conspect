// Program/Preview first-slice HTTP calls. Wire types are generated from
// OpenAPI; every mutation returns 202 pending, never an optimistic on-air claim.
import type { components } from './schema';
import { apiUrl, buildHeaders, OperationApiError, readProblem, newIdempotencyKey } from './operations';
import type { RequestOptions } from './operations';

export type SwitcherRequest = components['schemas']['SwitcherRequest'];
export type SwitcherState = components['schemas']['SwitcherStateResponse'];
export type SwitcherPlan = components['schemas']['SwitcherPlan'];
export type SwitcherAccepted = components['schemas']['SwitcherAccepted'];
export type SwitcherAction = 'select_program' | 'select_preview' | 'cut' | 'auto';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}
function isState(value: unknown): value is SwitcherState {
  return isRecord(value) && typeof value.program_source === 'string'
    && typeof value.preview_source === 'string' && typeof value.tick === 'number'
    && typeof value.in_flight === 'boolean';
}
function isPlan(value: unknown): value is SwitcherPlan {
  return isRecord(value) && typeof value.can_take === 'boolean'
    && typeof value.class === 'string' && typeof value.scene_version === 'number';
}
function isAccepted(value: unknown): value is SwitcherAccepted {
  return isRecord(value) && typeof value.kind === 'string'
    && typeof value.operation_id === 'string';
}

/** Read only state the real engine published after an actual output tick. */
export async function getSwitcher(options: RequestOptions = {}): Promise<SwitcherState> {
  const response = await fetch(apiUrl(options, '/api/v1/switcher/me1'), { headers: buildHeaders(options, false) });
  if (!response.ok) throw await readProblem(response);
  const body: unknown = await response.json();
  if (!isState(body)) throw new OperationApiError('Invalid switcher state');
  return body;
}

/** Ask the server to classify and validate a take before queuing it. */
export async function planSwitcher(request: SwitcherRequest, options: RequestOptions = {}): Promise<SwitcherPlan> {
  const response = await fetch(apiUrl(options, '/api/v1/switcher/plan'), {
    method: 'POST', headers: buildHeaders(options, true), body: JSON.stringify(request),
  });
  if (!response.ok) throw await readProblem(response);
  const body: unknown = await response.json();
  if (!isPlan(body)) throw new OperationApiError('Invalid switcher plan');
  return body;
}

/** Submit once; 202 is acceptance, not output-tick completion. */
export async function commandSwitcher(
  action: SwitcherAction,
  request: SwitcherRequest,
  options: RequestOptions = {},
): Promise<SwitcherAccepted> {
  const requestOptions = { ...options, idempotencyKey: options.idempotencyKey ?? newIdempotencyKey() };
  const response = await fetch(apiUrl(requestOptions, `/api/v1/switcher/commands/${action}`), {
    method: 'POST', headers: buildHeaders(requestOptions, true), body: JSON.stringify(request),
  });
  if (!response.ok) throw await readProblem(response);
  if (response.status !== 202) throw new OperationApiError('Switcher command was not accepted', response.status);
  const body: unknown = await response.json();
  if (!isAccepted(body)) throw new OperationApiError('Invalid switcher operation id');
  return body;
}
