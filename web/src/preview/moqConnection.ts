import { Connection } from '@moq/net';

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
