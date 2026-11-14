// Track model derived from the CAF sender's media session
// (`chrome.cast.media.Media.tracks` / `.activeTrackIds`), as published by the
// receiver in MEDIA_STATUS (DESIGN-418 decision 6/7). Pure, framework-agnostic.
import { t } from '@lingui/core/macro';

import type { ChromeCastMedia, ChromeCastTrack } from './castTypes';

/** Bound on how many tracks of one kind the UI shows (receiver already bounds its report the same way). */
export const MAX_TRACKS = 16;

export interface CastTrackOption {
  readonly id: number;
  readonly name: string;
  readonly language: string | null;
}

export interface CastTracksSnapshot {
  readonly audioTracks: readonly CastTrackOption[];
  readonly textTracks: readonly CastTrackOption[];
  readonly activeAudioId: number | null;
  readonly activeTextId: number | null;
}

export const EMPTY_TRACKS: CastTracksSnapshot = {
  audioTracks: [],
  textTracks: [],
  activeAudioId: null,
  activeTextId: null,
};

function text(value: unknown): string | null {
  return typeof value === 'string' && value !== '' ? value : null;
}

function toOption(track: ChromeCastTrack): CastTrackOption {
  const language = text(track.language);
  const name = text(track.name) ?? language ?? t`Track ${track.trackId}`;
  return { id: track.trackId, name, language };
}

/**
 * Derive the audio/text track lists and active selections from the CAF
 * media session. `null`/missing `tracks` or `activeTrackIds` yields the empty
 * snapshot (no tracks published, or the receiver has not loaded media yet).
 */
export function deriveTracks(media: ChromeCastMedia | null | undefined): CastTracksSnapshot {
  if (media == null) return EMPTY_TRACKS;
  const raw = media.tracks;
  if (raw == null) return EMPTY_TRACKS;
  const active = new Set(media.activeTrackIds ?? []);
  const audioTracks: CastTrackOption[] = [];
  const textTracks: CastTrackOption[] = [];
  let activeAudioId: number | null = null;
  let activeTextId: number | null = null;
  for (const track of raw) {
    if (typeof track.trackId !== 'number') continue;
    if (track.type === 'AUDIO' && audioTracks.length < MAX_TRACKS) {
      audioTracks.push(toOption(track));
      if (active.has(track.trackId)) activeAudioId = track.trackId;
    } else if (track.type === 'TEXT' && textTracks.length < MAX_TRACKS) {
      textTracks.push(toOption(track));
      if (active.has(track.trackId)) activeTextId = track.trackId;
    }
  }
  return { audioTracks, textTracks, activeAudioId, activeTextId };
}
