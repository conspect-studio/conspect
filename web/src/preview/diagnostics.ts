/** Privacy boundary: never retain raw reports, identifiers, errors, SDP or URLs. */
function choice(value: unknown, allowed: readonly string[]): string {
  return typeof value === 'string' && allowed.includes(value) ? value : 'unknown';
}
function count(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;
}
export function safeStates(pc: RTCPeerConnection) {
  return {
    ice: choice(pc.iceConnectionState, ['new', 'checking', 'connected', 'completed', 'disconnected', 'failed', 'closed']),
    peer: choice(pc.connectionState, ['new', 'connecting', 'connected', 'disconnected', 'failed', 'closed']),
  };
}
export function safeStats(report?: RTCStatsReport) {
  let bytesReceived: number | null = null;
  let framesDecoded: number | null = null;
  let width: number | null = null;
  let height: number | null = null;
  let codecMime = 'unknown';
  let codecProfile = 'unknown';
  let dtls = 'unknown';
  const candidates: { type: string; protocol: string }[] = [];
  const rows: Record<string, unknown>[] = [];
  report?.forEach((stat: RTCStats) => rows.push({ ...stat }));
  for (const row of rows) {
    if (row.type === 'transport') dtls = choice(row.dtlsState, ['new', 'connecting', 'connected', 'closed', 'failed']);
    if (row.type === 'local-candidate' || row.type === 'remote-candidate') {
      const candidate = { type: choice(row.candidateType, ['host', 'srflx', 'prflx', 'relay']), protocol: choice(row.protocol, ['udp', 'tcp']) };
      if (!candidates.some((entry) => entry.type === candidate.type && entry.protocol === candidate.protocol)) candidates.push(candidate);
    }
    if (row.type !== 'inbound-rtp' || (row.kind !== 'video' && row.mediaType !== 'video')) continue;
    const bytes = count(row.bytesReceived);
    const frames = count(row.framesDecoded);
    if (bytes !== null) bytesReceived = (bytesReceived ?? 0) + bytes;
    if (frames !== null) framesDecoded = (framesDecoded ?? 0) + frames;
    width = count(row.frameWidth);
    height = count(row.frameHeight);
    const codec = rows.find((entry) => entry.type === 'codec' && entry.id === row.codecId);
    codecMime = choice(codec?.mimeType, ['video/H264', 'video/VP8', 'video/VP9', 'video/AV1', 'video/H265']);
    // Only the H264 six-hex profile-level-id is safe; never export raw fmtp.
    if (codecMime === 'video/H264' && typeof codec?.sdpFmtpLine === 'string') {
      codecProfile = /(?:^|;)\s*profile-level-id=([0-9a-fA-F]{6})\s*(?:;|$)/.exec(codec.sdpFmtpLine)?.[1] ?? 'unknown';
    }
  }
  return { bytesReceived, framesDecoded, width, height, codecMime, codecProfile, dtls, candidates };
}
export type SafeStats = ReturnType<typeof safeStats>;
export type FailureStage = 'http-capacity' | 'http-error' | 'signaling' | 'ice-timeout' | 'connection-timeout' | 'ice-failed' | 'peer-failed' | 'dtls-failed' | 'video-no-rtp' | 'video-no-decode' | 'video-stalled' | 'media-unknown' | 'media-play-error';
export interface PreviewDiagnostics {
  stage: FailureStage;
  httpStatus: number | null;
  elapsedMs: number;
  signalingMs: number | null;
  connectedMs: number | null;
  ice: string;
  peer: string;
  media: SafeStats;
}
export function failureStage(stats: SafeStats): FailureStage {
  if (stats.dtls === 'failed') return 'dtls-failed';
  if (stats.bytesReceived === 0) return 'video-no-rtp';
  if (stats.bytesReceived !== null && stats.bytesReceived > 0 && stats.framesDecoded === 0) return 'video-no-decode';
  return stats.bytesReceived === null ? 'media-unknown' : 'video-stalled';
}
