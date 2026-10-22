// Input streams surface: the read-only elementary-stream inventory of an input.
//
// `GET /api/v1/inputs/{id}/streams` returns the cached `StreamInventoryDoc` for
// a configured input — every elementary stream (video / audio / subtitle / data
// / timecode) it offers, in container order, with codec, language, and a stable
// kind-scoped id. It is the off-engine cached snapshot, so it is empty until the
// input has been probed. Read-only: there is NO PID-selection-override mutation
// (the PATCH does not exist); this surface only reads the inventory.
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
} from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

/** The elementary-stream inventory of one input. */
export type StreamInventory = components['schemas']['StreamInventoryDoc'];

/** One elementary stream in an input's inventory. */
export type StreamDescriptor = components['schemas']['StreamDescriptorDoc'];

export { OperationApiError } from './operations';
export type { RequestOptions } from './operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isStreamInventory(value: unknown): value is StreamInventory {
  return isRecord(value) && Array.isArray(value.streams);
}

/** Fetch an input's elementary-stream inventory (`GET /api/v1/inputs/{id}/streams`). */
export async function getInputStreams(
  id: string,
  options: RequestOptions = {},
): Promise<StreamInventory> {
  const response = await fetch(
    apiUrl(options, `/api/v1/inputs/${encodeURIComponent(id)}/streams`),
    {
      method: 'GET',
      headers: buildHeaders(options, false),
    },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isStreamInventory(body)) {
    throw new OperationApiError('The server returned an unexpected stream inventory.');
  }
  return body;
}
