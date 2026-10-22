// System capabilities surface: the read-only build capability + licence report.
//
// `GET /api/v1/system/capabilities` (ADR-W030) returns which codec backends this
// build can use, the compositor acceleration tier, the effective build-profile
// licence (a compliance surface — ADR-0012), and the mandatory NDI attribution.
// Read-only and system-global (a viewer may read it). The server value is a
// static startup snapshot — it does not change while the process runs — so the
// hook reads it once and does not poll.
import { apiUrl, buildHeaders, OperationApiError, readProblem } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

/** The build capability + licence surface (`SystemCapabilities`). */
export type SystemCapabilities = components['schemas']['SystemCapabilities'];
/** One `(kind, stage)` backend availability row. */
export type BackendCapability = components['schemas']['BackendCapability'];
/** The compositor acceleration tier. */
export type CompositorCapability = components['schemas']['CompositorCapability'];
/** The build-profile compliance surface. */
export type BuildInfo = components['schemas']['BuildInfo'];
/** The effective build-profile licence literal (`LGPL-clean` | `GPL`). */
export type EffectiveLicense = components['schemas']['EffectiveLicense'];

export { OperationApiError } from './operations';
export type { RequestOptions } from './operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** Narrow an unknown body to the capability report shape. */
function isSystemCapabilities(value: unknown): value is SystemCapabilities {
  return (
    isRecord(value) &&
    Array.isArray(value.backends) &&
    isRecord(value.compositor) &&
    isRecord(value.build)
  );
}

/**
 * Read the build capability + licence surface
 * (`GET /api/v1/system/capabilities`).
 */
export async function getSystemCapabilities(
  options: RequestOptions = {},
): Promise<SystemCapabilities> {
  const response = await fetch(apiUrl(options, '/api/v1/system/capabilities'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isSystemCapabilities(body)) {
    throw new OperationApiError(
      'The server returned an unexpected capability report.',
    );
  }
  return body;
}
