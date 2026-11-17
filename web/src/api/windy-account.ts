// Windy Premium account (issue #470): Settings → Integrations → Windy.
//
// Admin only. The email/password are WRITE-ONLY: `PUT` sends them once and the
// server stores them encrypted; no response ever echoes them. `GET` reports
// only `set`, the last check time/result and whether Premium is available.
// `POST …/verify` runs one bounded login check (up to ~90 s) of the stored
// credential; Premium capture is enabled only after a passing check, and
// replacing the credential resets it.
import { apiUrl, buildHeaders, readProblem, OperationApiError } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './types';

export type WindyAccountView = components['schemas']['WindyAccountView'];
export type WindyVerifyResult = components['schemas']['WindyVerifyResult'];
export type WindyVerdict = components['schemas']['WindyVerdict'];

const ACCOUNT = '/api/v1/integrations/windy';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isView(value: unknown): value is WindyAccountView {
  return isRecord(value) && typeof value.set === 'boolean' && typeof value.premium_available === 'boolean';
}

function isVerdict(value: unknown): value is WindyVerdict {
  return value === 'premium' || value === 'rejected' || value === 'not_premium' || value === 'unavailable';
}

async function call(
  method: string,
  path: string,
  options: RequestOptions,
  body?: unknown,
): Promise<unknown> {
  const response = await fetch(apiUrl(options, path), {
    method,
    headers: buildHeaders(options, body !== undefined),
    cache: 'no-store',
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  return response.json();
}

async function view(method: string, options: RequestOptions, body?: unknown): Promise<WindyAccountView> {
  const parsed = await call(method, ACCOUNT, options, body);
  if (!isView(parsed)) {
    throw new OperationApiError('The server returned an unexpected Windy account body.');
  }
  return parsed;
}

/** `GET /api/v1/integrations/windy` — secret-free status. */
export function getWindyAccount(options: RequestOptions = {}): Promise<WindyAccountView> {
  return view('GET', options);
}

/** `PUT /api/v1/integrations/windy` — store (replace) the credential. */
export function setWindyAccount(
  email: string,
  password: string,
  options: RequestOptions = {},
): Promise<WindyAccountView> {
  return view('PUT', options, { email, password });
}

/** `DELETE /api/v1/integrations/windy` — remove the credential. */
export function clearWindyAccount(options: RequestOptions = {}): Promise<WindyAccountView> {
  return view('DELETE', options);
}

/** `POST /api/v1/integrations/windy/verify` — one bounded login check. */
export async function verifyWindyAccount(options: RequestOptions = {}): Promise<WindyVerifyResult> {
  const parsed = await call('POST', `${ACCOUNT}/verify`, options);
  if (!isRecord(parsed) || !isVerdict(parsed.result) || !isView(parsed.account)) {
    throw new OperationApiError('The server returned an unexpected Windy verify body.');
  }
  return { result: parsed.result, account: parsed.account };
}
