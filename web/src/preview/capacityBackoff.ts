/** First retry after a `whep-capacity` refusal, then x1.5 per refusal, capped. */
export const CAPACITY_RETRY_BASE_MS = 10_000;
export const CAPACITY_RETRY_CAP_MS = 30_000;

/** Backoff delay (ms) before capacity retry number `n` (0-based). */
export function capacityRetryDelayMs(n: number): number {
  return Math.min(CAPACITY_RETRY_BASE_MS * 1.5 ** n, CAPACITY_RETRY_CAP_MS);
}
