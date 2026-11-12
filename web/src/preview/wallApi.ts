// Hand-written client for the shared multiview "wall" preview stream (issue #250).
// Not yet in the generated OpenAPI schema; replace with generated types once it is.
import { getStoredToken } from '../api/token';
import { MoqUnavailable, type MoqLease, type MoqLeaseApi } from './moqPlayback';

export interface WallCell {
  readonly source: string;
  /** Rectangle in coded-picture pixels of the decoded wall frame. */
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
  /** False while the source has no picture yet: that surface keeps its still. */
  readonly live?: boolean;
}

export interface WallGeometry {
  readonly generation: number;
  readonly width: number;
  readonly height: number;
  readonly cadence: { readonly num: number; readonly den: number };
  readonly cells: readonly WallCell[];
  readonly preview_source: string | null;
}

/** The wall is off, not ready, forbidden for this principal, or unreachable. */
export class WallUnavailable extends Error {}

const WALL = '/api/v1/preview/wall';

function headers(): Record<string, string> {
  const token = getStoredToken();
  return token === undefined ? {} : { Authorization: `Bearer ${token}` };
}

export function findCell(geometry: WallGeometry | undefined, source: string | null | undefined): WallCell | undefined {
  if (!geometry || source == null) return undefined;
  return geometry.cells.find((cell) => cell.source === source && cell.live !== false);
}

/** Source rectangle for drawImage, clamped to the frame; undefined when it is empty or outside. */
export function cropRect(cell: WallCell, frameWidth: number, frameHeight: number): readonly [number, number, number, number] | undefined {
  const x = Math.max(0, cell.x);
  const y = Math.max(0, cell.y);
  const w = Math.min(cell.w - (x - cell.x), frameWidth - x);
  const h = Math.min(cell.h - (y - cell.y), frameHeight - y);
  return w > 0 && h > 0 ? [x, y, w, h] : undefined;
}

/** GET the geometry. Every call also renews the cells' preview-demand lease. */
export async function fetchWall(signal?: AbortSignal): Promise<WallGeometry> {
  let response: Response;
  try {
    response = await fetch(WALL, { headers: headers(), cache: 'no-store', ...(signal ? { signal } : {}) });
  } catch (error) {
    if (signal?.aborted) throw error;
    throw new WallUnavailable('wall request failed');
  }
  if (!response.ok) throw new WallUnavailable(`wall unavailable (${String(response.status)})`);
  const body: unknown = await response.json().catch(() => undefined);
  if (typeof body !== 'object' || body === null || !('generation' in body) || !('cells' in body) || !Array.isArray(body.cells)) {
    throw new WallUnavailable('wall answer malformed');
  }
  // Shape validated above (generation + cells); the engine contract is fixed.
  // eslint-disable-next-line @typescript-eslint/no-unsafe-type-assertion
  return body as WallGeometry;
}

/** Lease client for the wall MoQ stream; 403/503 mean "fall back to stills". */
export function createWallLeaseApi(): MoqLeaseApi {
  return {
    async acquire(signal) {
      const response = await fetch(`${WALL}/moq`, { method: 'POST', headers: headers(), signal });
      if (response.status === 403 || response.status === 503) throw new MoqUnavailable('wall MoQ unavailable');
      if (!response.ok) throw new Error('wall MoQ lease request failed');
      const lease: unknown = await response.json();
      if (typeof lease !== 'object' || lease === null || !('lease_id' in lease) || !('url' in lease)) throw new Error('wall MoQ lease malformed');
      // Same JSON shape as the program lease (engine contract).
      // eslint-disable-next-line @typescript-eslint/no-unsafe-type-assertion
      return lease as MoqLease;
    },
    async release(lease) {
      const url = `${WALL}/moq/${encodeURIComponent(lease.lease_id)}?generation=${String(lease.generation)}`;
      const response = await fetch(url, { method: 'DELETE', headers: headers(), signal: AbortSignal.timeout(5000) });
      if (!response.ok && response.status !== 404) throw new Error('wall MoQ lease revocation not acknowledged');
    },
  };
}
