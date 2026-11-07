import { describe, expect, it } from 'vitest';
import { safeStats, failureStage } from './diagnostics';

function report(rows: Record<string, unknown>[]): RTCStatsReport {
  return new Map(rows.map((row, i) => [String(i), row]));
}

describe('safe preview diagnostics', () => {
  it('copies only allowlisted video metrics, codec and candidate enums', () => {
    const stats = safeStats(report([
      { type: 'inbound-rtp', kind: 'audio', bytesReceived: 9000 },
      { type: 'inbound-rtp', kind: 'video', bytesReceived: 42, framesDecoded: 0, frameWidth: 1280, frameHeight: 720, codecId: 'codec-secret', address: 'secret' },
      { id: 'codec-secret', type: 'codec', mimeType: 'video/H264', sdpFmtpLine: 'profile-level-id=42e01f;secret=token' },
      { type: 'local-candidate', candidateType: 'host', protocol: 'udp', address: 'secret', port: 1234, url: 'secret', usernameFragment: 'secret' },
      { type: 'remote-candidate', candidateType: 'secret', protocol: 'secret' },
      { type: 'transport', dtlsState: 'failed', iceLocalUsernameFragment: 'secret' },
    ]));
    expect(stats).toMatchObject({ bytesReceived: 42, framesDecoded: 0, width: 1280, height: 720, codecMime: 'video/H264', codecProfile: '42e01f', dtls: 'failed' });
    expect(stats.candidates).toEqual([{ type: 'host', protocol: 'udp' }, { type: 'unknown', protocol: 'unknown' }]);
    expect(JSON.stringify(stats)).not.toMatch(/secret|9000|1234/);
  });
  it('reports missing or invalid stats as unknown, not zero', () => {
    expect(safeStats(report([])).bytesReceived).toBeNull();
    const stats = safeStats(report([{ type: 'inbound-rtp', kind: 'video', bytesReceived: NaN, framesDecoded: -1 }, { type: 'codec', mimeType: 'video/secret' }]));
    expect(stats.bytesReceived).toBeNull();
    expect(stats.framesDecoded).toBeNull();
    expect(stats.codecMime).toBe('unknown');
  });
  it('distinguishes no RTP, decode failure and unknown without network speculation', () => {
    expect(failureStage({ ...safeStats(report([])), bytesReceived: 0 })).toBe('video-no-rtp');
    expect(failureStage({ ...safeStats(report([])), bytesReceived: 10, framesDecoded: 0 })).toBe('video-no-decode');
    expect(failureStage(safeStats(report([])))).toBe('media-unknown');
  });
});
