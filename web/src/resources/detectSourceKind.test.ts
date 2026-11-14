// Unit tests for the pure "paste a link or address" detector (#419). Framework
// free; covers every mapping named in the issue plus negatives and the id
// slug/collision behaviour.
import { describe, expect, it } from 'vitest';

import { detectSourceKind } from './detectSourceKind';

describe('detectSourceKind — YouTube', () => {
  it('detects a watch URL', () => {
    const d = detectSourceKind('https://www.youtube.com/watch?v=KHy_n-EMWY8');
    expect(d?.kind).toBe('youtube');
    expect(d?.locatorField).toBe('url');
    expect(d?.locator).toBe('https://www.youtube.com/watch?v=KHy_n-EMWY8');
    expect(d?.suggestedId).toBe('youtube-khy-n-emwy8');
    expect(d?.suggestedName).toBe('YouTube KHy_n-EMWY8');
  });

  it('detects the bare youtube.com host', () => {
    const d = detectSourceKind('https://youtube.com/watch?v=abc123');
    expect(d?.kind).toBe('youtube');
  });

  it('detects m.youtube.com', () => {
    const d = detectSourceKind('https://m.youtube.com/watch?v=abc123');
    expect(d?.kind).toBe('youtube');
    expect(d?.suggestedId).toBe('youtube-abc123');
  });

  it('detects a youtu.be short link', () => {
    const d = detectSourceKind('https://youtu.be/KHy_n-EMWY8');
    expect(d?.kind).toBe('youtube');
    expect(d?.suggestedId).toBe('youtube-khy-n-emwy8');
  });

  it('detects youtube.com/live/<id>', () => {
    const d = detectSourceKind('https://www.youtube.com/live/abc123');
    expect(d?.kind).toBe('youtube');
    expect(d?.suggestedId).toBe('youtube-abc123');
    expect(d?.suggestedName).toBe('YouTube abc123');
  });

  it('detects a channel handle live URL with no video id', () => {
    const d = detectSourceKind('https://www.youtube.com/@SomeChannel/live');
    expect(d?.kind).toBe('youtube');
    expect(d?.suggestedId).toBe('youtube-somechannel');
    expect(d?.suggestedName).toBe('YouTube @SomeChannel live');
  });

  it('does not detect an unrecognized youtube.com path (e.g. the homepage)', () => {
    expect(detectSourceKind('https://www.youtube.com/')).toBeUndefined();
    expect(detectSourceKind('https://www.youtube.com/playlist?list=xyz')).toBeUndefined();
  });

  it('trims surrounding whitespace', () => {
    const d = detectSourceKind('  https://youtu.be/abc123  ');
    expect(d?.kind).toBe('youtube');
    expect(d?.locator).toBe('https://youtu.be/abc123');
  });
});

describe('detectSourceKind — HLS', () => {
  it('detects any http(s) URL ending in .m3u8', () => {
    const d = detectSourceKind('https://example.com/live/master.m3u8');
    expect(d?.kind).toBe('hls');
    expect(d?.locatorField).toBe('url');
    expect(d?.suggestedName).toBe('example.com');
  });

  it('ignores a query string / fragment after .m3u8', () => {
    const d = detectSourceKind('https://example.com/live/master.m3u8?token=xyz#frag');
    expect(d?.kind).toBe('hls');
  });

  it('does not detect an .m3u8-like path that is not the actual path', () => {
    expect(detectSourceKind('https://example.com/live/master.m3u8x')).toBeUndefined();
    expect(detectSourceKind('https://example.com/live?file=master.m3u8')).toBeUndefined();
  });

  it('does not detect a plain http(s) URL with no .m3u8 suffix', () => {
    expect(detectSourceKind('https://example.com/stream')).toBeUndefined();
  });
});

describe('detectSourceKind — plain transport schemes', () => {
  it('detects rtsp:// and rtsps://', () => {
    expect(detectSourceKind('rtsp://cam.example:554/stream')?.kind).toBe('rtsp');
    expect(detectSourceKind('rtsps://cam.example:554/stream')?.kind).toBe('rtsp');
  });

  it('detects srt://', () => {
    expect(detectSourceKind('srt://host.example:7001')?.kind).toBe('srt');
  });

  it('detects rist://', () => {
    expect(detectSourceKind('rist://host.example:5000')?.kind).toBe('rist');
  });

  it('detects rtmp:// and rtmps://', () => {
    expect(detectSourceKind('rtmp://ingest.example/app/key')?.kind).toBe('rtmp');
    expect(detectSourceKind('rtmps://ingest.example/app/key')?.kind).toBe('rtmp');
  });

  it('detects udp:// and rtp:// as ts', () => {
    expect(detectSourceKind('udp://[ff3e::1]:5004')?.kind).toBe('ts');
    expect(detectSourceKind('rtp://239.1.1.1:5004')?.kind).toBe('ts');
  });

  it('builds a host-based id for a plain transport URL', () => {
    const d = detectSourceKind('rtsp://cam.example:554/stream');
    expect(d?.suggestedId).toBe('rtsp-cam-example');
    expect(d?.suggestedName).toBe('cam.example');
  });
});

describe('detectSourceKind — file paths', () => {
  it('detects an absolute filesystem path', () => {
    const d = detectSourceKind('/media/clip.mp4');
    expect(d?.kind).toBe('file');
    expect(d?.locatorField).toBe('path');
    expect(d?.locator).toBe('/media/clip.mp4');
    expect(d?.suggestedId).toBe('clip-mp4');
    expect(d?.suggestedName).toBe('clip.mp4');
  });

  it('percent-decodes a file:// URL path (spaces, Unicode, literal percent)', () => {
    expect(detectSourceKind('file:///media/My%20Clip.mp4')?.locator).toBe('/media/My Clip.mp4');
    expect(detectSourceKind('file:///media/caf%C3%A9.mp4')?.locator).toBe('/media/café.mp4');
    expect(detectSourceKind('file:///media/100%25.mp4')?.locator).toBe('/media/100%.mp4');
    expect(detectSourceKind('file:///media/My%20Clip.mp4')?.suggestedName).toBe('My Clip.mp4');
  });

  it('keeps a malformed escape in a file:// URL without throwing', () => {
    expect(detectSourceKind('file:///media/bad%ZZ.mp4')?.locator).toBe('/media/bad%ZZ.mp4');
  });

  it('detects a file:// URL', () => {
    const d = detectSourceKind('file:///media/clip.mp4');
    expect(d?.kind).toBe('file');
    expect(d?.locator).toBe('/media/clip.mp4');
  });
});

describe('detectSourceKind — negatives', () => {
  it('returns undefined for empty / whitespace-only input', () => {
    expect(detectSourceKind('')).toBeUndefined();
    expect(detectSourceKind('   ')).toBeUndefined();
  });

  it('returns undefined for an unparseable string', () => {
    expect(detectSourceKind('not a url at all')).toBeUndefined();
  });

  it('returns undefined for an unrecognized scheme', () => {
    expect(detectSourceKind('ndi://some-name')).toBeUndefined();
  });

  it('returns undefined for a relative path', () => {
    expect(detectSourceKind('media/clip.mp4')).toBeUndefined();
  });
});

describe('detectSourceKind — id collisions', () => {
  it('appends -2, -3 to avoid an existing id', () => {
    const existing = new Set(['youtube-khy-n-emwy8']);
    const d = detectSourceKind('https://youtu.be/KHy_n-EMWY8', existing);
    expect(d?.suggestedId).toBe('youtube-khy-n-emwy8-2');
  });

  it('keeps incrementing past multiple collisions', () => {
    const existing = new Set(['youtube-abc', 'youtube-abc-2', 'youtube-abc-3']);
    const d = detectSourceKind('https://youtu.be/abc', existing);
    expect(d?.suggestedId).toBe('youtube-abc-4');
  });

  it('produces a lowercase, id-safe slug even for an unusual video id', () => {
    const d = detectSourceKind('https://youtu.be/A_B-C123');
    expect(d?.suggestedId).toBe('youtube-a-b-c123');
    expect(d?.suggestedId).toMatch(/^[a-z0-9-]+$/);
  });
});
