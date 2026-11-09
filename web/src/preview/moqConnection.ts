import { Connection, type Origin } from '@moq/net';

/**
 * Own an established session separately from its handshake AbortSignal.
 * Callers must await `released` before opening a fallback lease. A failed
 * release rejects that promise; it never grants permission to overlap leases.
 * The control-plane release callback must have its own bounded deadline.
 */
export function ownMoqSession(
  session: Pick<Connection.Established, 'close' | 'closed'>,
  signal: AbortSignal,
  release: () => Promise<void>,
  closeTimeoutMs = 3000,
): { close: () => Promise<void>; readonly released: Promise<void> } {
  if (!Number.isFinite(closeTimeoutMs) || closeTimeoutMs <= 0) {
    throw new Error('Invalid MoQ closure deadline');
  }
  let closing = false;
  let ended = false;
  let succeeded = false;
  let inFlight: Promise<void> | undefined;
  const confirmed = session.closed.then(
    () => { ended = true; },
    () => { ended = true; }, // A rejected closed promise also confirms termination.
  );
  const attempt = (): Promise<void> => {
    if (succeeded) return Promise.resolve();
    if (inFlight) return inFlight;
    inFlight = (async (): Promise<void> => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        await Promise.race([
          confirmed,
          new Promise<never>((_, reject) => {
            timer = setTimeout(() => { reject(new Error('MoQ closure deadline exceeded')); }, closeTimeoutMs);
          }),
        ]);
      } finally {
        if (timer !== undefined) clearTimeout(timer);
      }
      await release();
      succeeded = true;
    })().finally(() => { inFlight = undefined; });
    void inFlight.catch(() => { /* Owner observes failure; abort must not throw. */ });
    return inFlight;
  };
  const close = (): Promise<void> => {
    if (!closing && !ended) {
      closing = true;
      try {
        session.close();
      } catch {
        // A close request can fail synchronously. Only the independently
        // settled closed promise confirms termination; otherwise time out.
        closing = false;
      }
    }
    return attempt();
  };
  const abort = (): void => { void close(); };
  const released = confirmed.then(() => {
    signal.removeEventListener('abort', abort);
    return attempt();
  });
  void released.catch(() => { /* Owner observes the original failure. */ });
  signal.addEventListener('abort', abort, { once: true });
  if (signal.aborted) abort();
  return { close, get released(): Promise<void> { return succeeded ? Promise.resolve() : (inFlight ?? released); } };
}

/**
 * One QUIC-only attempt. Do not use the pooled Connection constructor: it
 * reconnects and retains sessions after the last handle closes. The caller
 * owns its deadline and must close the returned session before releasing its
 * authorized preview lease or selecting WebRTC. Connection failures are not
 * automatically classified as availability failures (auth/capacity must remain
 * visible). This transport primitive is not yet wired to PreviewSurface.
 */
export async function connectMoq(
  url: URL,
  signal: AbortSignal,
  consume?: Origin.Producer,
  webtransport?: Connection.WebTransportProps,
): Promise<Connection.Established> {
  if (url.protocol !== 'https:') {
    throw new Error('MoQ requires an HTTPS endpoint');
  }
  signal.throwIfAborted();
  return Connection.connect({ url, websocket: { enabled: false }, signal,
    ...(consume ? { consume } : {}), ...(webtransport ? { webtransport } : {}),
  });
}
