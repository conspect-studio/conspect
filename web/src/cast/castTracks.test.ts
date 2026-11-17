// Track derivation from the CAF sender's media session (DESIGN-418 decision 7).
import { i18n } from '@lingui/core';
import { describe, expect, it } from 'vitest';

import { messages } from '../locales/en/messages';
import { EMPTY_TRACKS, MAX_TRACKS, deriveTracks } from './castTracks';
import type { ChromeCastMedia, ChromeCastMediaInfo, ChromeCastTrack } from './castTypes';

i18n.load('en', messages);
i18n.activate('en');

function media(tracks: readonly ChromeCastTrack[] | null, activeTrackIds: readonly number[] | null): ChromeCastMedia {
  // Real SDK shape: tracks live on the nested `MediaInfo` (`Media.media`),
  // not on `Media` itself; only `activeTrackIds` is on the outer `Media`.
  const mediaInfo: ChromeCastMediaInfo = {
    contentId: 'content',
    contentType: 'application/vnd.apple.mpegurl',
    streamType: 'LIVE',
    tracks,
  };
  return {
    media: mediaInfo,
    activeTrackIds,
    addUpdateListener: () => undefined,
    removeUpdateListener: () => undefined,
    stop: () => undefined,
    editTracksInfo: () => undefined,
  };
}

describe('deriveTracks', () => {
  it('returns the empty snapshot for null/undefined media or missing tracks', () => {
    expect(deriveTracks(null)).toEqual(EMPTY_TRACKS);
    expect(deriveTracks(undefined)).toEqual(EMPTY_TRACKS);
    expect(deriveTracks(media(null, null))).toEqual(EMPTY_TRACKS);
  });

  it('splits audio/text tracks and marks the active id of each kind', () => {
    const tracks: ChromeCastTrack[] = [
      { trackId: 1, type: 'AUDIO', name: 'English', language: 'en' },
      { trackId: 2, type: 'AUDIO', name: 'French', language: 'fr' },
      { trackId: 10, type: 'TEXT', name: 'English CC', language: 'en' },
      { trackId: 99, type: 'VIDEO', name: null, language: null },
    ];
    const snapshot = deriveTracks(media(tracks, [2, 10]));
    expect(snapshot.audioTracks).toEqual([
      { id: 1, name: 'English', language: 'en' },
      { id: 2, name: 'French', language: 'fr' },
    ]);
    expect(snapshot.textTracks).toEqual([{ id: 10, name: 'English CC', language: 'en' }]);
    expect(snapshot.activeAudioId).toBe(2);
    expect(snapshot.activeTextId).toBe(10);
  });

  it('activeTextId is null when no text track is active (subtitles off)', () => {
    const tracks: ChromeCastTrack[] = [
      { trackId: 1, type: 'AUDIO', name: 'English', language: 'en' },
      { trackId: 10, type: 'TEXT', name: 'English CC', language: 'en' },
    ];
    const snapshot = deriveTracks(media(tracks, [1]));
    expect(snapshot.activeAudioId).toBe(1);
    expect(snapshot.activeTextId).toBeNull();
  });

  it('falls back to the language, then a generic name, when the track has no name', () => {
    const tracks: ChromeCastTrack[] = [
      { trackId: 1, type: 'AUDIO', name: null, language: 'es' },
      { trackId: 2, type: 'AUDIO', name: null, language: null },
    ];
    const snapshot = deriveTracks(media(tracks, []));
    expect(snapshot.audioTracks[0]?.name).toBe('es');
    expect(snapshot.audioTracks[1]?.name).toBe('Track 2');
  });

  it('bounds each kind of list to MAX_TRACKS entries', () => {
    const many: ChromeCastTrack[] = Array.from({ length: MAX_TRACKS + 5 }, (_, i) => ({
      trackId: i,
      type: 'AUDIO',
      name: `Track ${String(i)}`,
      language: null,
    }));
    const snapshot = deriveTracks(media(many, []));
    expect(snapshot.audioTracks).toHaveLength(MAX_TRACKS);
  });
});
