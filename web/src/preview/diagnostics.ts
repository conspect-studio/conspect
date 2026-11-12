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
  const selectedPair = selectedCandidatePair(rows);
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
  return { bytesReceived, framesDecoded, width, height, codecMime, codecProfile, dtls, candidates, selectedPair };
}
export type SafeStats = ReturnType<typeof safeStats>;

/** The selected ICE pair as enums only: which transport actually carries the media. */
export interface SelectedPair {
  /** `udp` or `tcp` (the local candidate's protocol; both ends agree for a host/prflx pair). */
  readonly protocol: string;
  readonly local: string;
  readonly remote: string;
}

/**
 * The pair the transport selected (`transport.selectedCandidatePairId`, the spec
 * field), else the nominated succeeded pair (Firefox reports no selected id).
 * `null` until a pair is selected. Never copies addresses or ports.
 */
function selectedCandidatePair(rows: readonly Record<string, unknown>[]): SelectedPair | null {
  const byId = (id: unknown): Record<string, unknown> | undefined =>
    typeof id === 'string' ? rows.find((row) => row.id === id) : undefined;
  const transport = rows.find((row) => row.type === 'transport' && typeof row.selectedCandidatePairId === 'string');
  const pair = byId(transport?.selectedCandidatePairId)
    ?? rows.find((row) => row.type === 'candidate-pair' && (row.selected === true || (row.nominated === true && row.state === 'succeeded')));
  if (pair === undefined) return null;
  const local = byId(pair.localCandidateId);
  const remote = byId(pair.remoteCandidateId);
  if (local === undefined || remote === undefined) return null;
  return {
    protocol: choice(local.protocol, ['udp', 'tcp']),
    local: choice(local.candidateType, ['host', 'srflx', 'prflx', 'relay']),
    remote: choice(remote.candidateType, ['host', 'srflx', 'prflx', 'relay']),
  };
}

/** `tcp/host-host`-style label for a probe (`data-ice-pair`), or `none` before selection. */
export function pairLabel(pair: SelectedPair | null): string {
  return pair === null ? 'none' : `${pair.protocol}/${pair.local}-${pair.remote}`;
}
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
