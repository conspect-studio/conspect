import { describe, expect, it } from 'vitest';
import { hlsPlaybackPaths } from './outputPlayback';
import type { OutputView } from '../resources/types';
const output = (id: string, target = '/srv/program.m3u8'): OutputView => ({ id, name: id, kind: 'hls', rawKind: 'hls', editable: true, target, codec: 'h264' });
describe('HLS playback delivery paths', () => {
  it('uses the configured playlist, not an unshipped master playlist', () => {
    expect(hlsPlaybackPaths([output('program')]).get('program')).toBe('/hls/program/program.m3u8');
  });
  it('encodes playlist filenames and normalizes unsafe mount characters', () => {
    expect(hlsPlaybackPaths([output('a b', '/srv/my output.m3u8')]).get('a b')).toBe('/hls/a-b/my%20output.m3u8');
  });
  it('does not guess colliding mounts or directory targets', () => {
    expect(hlsPlaybackPaths([output('a b'), output('a-b'), output('a-b-2'), output('directory', '/srv/hls/')]).size).toBe(0);
  });
});
