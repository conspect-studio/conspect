import { Connection } from '@moq/net';

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
): { close: () => Promise<void>; released: Promise<void> } {
  let closing = false;
  let ended = false;
  const abort = (): void => {
    if (closing || ended) return;
    closing = true;
    session.close();
  };
  const released = session.closed.then(async () => {
    ended = true;
    signal.removeEventListener('abort', abort);
    await release();
  });
  // Keep abort-triggered teardown from generating an unhandled rejection;
  // the original promise remains rejected for the owner to observe.
  void released.catch(() => { /* The owner observes the original promise. */ });
  signal.addEventListener('abort', abort, { once: true });
  if (signal.aborted) abort();
  return { close: (): Promise<void> => { abort(); return released; }, released };
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
): Promise<Connection.Established> {
  if (url.protocol !== 'https:') {
    throw new Error('MoQ requires an HTTPS endpoint');
  }
  signal.throwIfAborted();
  return Connection.connect({ url, websocket: { enabled: false }, signal });
}
