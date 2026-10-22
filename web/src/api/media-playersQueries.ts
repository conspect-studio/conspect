// React Query bindings for the media-player (VT) transport surface.
//
// `useMediaPlayers()` lists the configured players; `useMediaPlayerTransport()`
// drives one player through a transport verb (load / cue / play / pause / stop /
// seek and the vamp exit arm / take / cancel), returning the `202` operation id
// so the page can surface it. The engine is isolated (invariant #10): the read
// degrades to loading / error states, and a command's outcome (the player's new
// transport state) arrives on the realtime `media.player_state` stream, never in
// the mutation response.
import { useMutation, useQuery } from '@tanstack/react-query';
import type {
  UseMutationResult,
  UseQueryResult,
} from '@tanstack/react-query';

import {
  armMediaPlayerExit,
  cancelMediaPlayerExit,
  cueMediaPlayer,
  listMediaPlayers,
  loadMediaPlayer,
  pauseMediaPlayer,
  playMediaPlayer,
  seekMediaPlayer,
  stopMediaPlayer,
  takeMediaPlayerExit,
} from './media-players';
import type {
  AcceptedBody,
  MediaPlayer,
  OperationApiError,
  RequestOptions,
} from './media-players';

export type { MediaPlayer } from './media-players';
export { OperationApiError } from './operations';

/** Connection options threaded into the media-player hooks. */
export interface MediaPlayerContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

function options(context: MediaPlayerContext): RequestOptions {
  return {
    ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
    ...(context.token !== undefined ? { token: context.token } : {}),
  };
}

/** Stable React Query key for the media-player list. */
export const mediaPlayerKeys = {
  list: ['media-players'] as const,
};

/** List all configured media players. */
export function useMediaPlayers(
  context: MediaPlayerContext = {},
): UseQueryResult<MediaPlayer[], OperationApiError> {
  return useQuery<MediaPlayer[], OperationApiError>({
    queryKey: mediaPlayerKeys.list,
    queryFn: (): Promise<MediaPlayer[]> => listMediaPlayers(options(context)),
  });
}

/**
 * A transport verb to apply to one player. `load` carries an `asset`; `cue` and
 * `seek` may carry a `frame` (absent => the in-point); the rest carry nothing.
 * The union is discriminated by `action` so the payload is checked per verb.
 */
export type TransportVars =
  | { readonly id: string; readonly action: 'load'; readonly asset: string }
  | {
      readonly id: string;
      readonly action: 'cue' | 'seek';
      readonly frame?: number;
    }
  | {
      readonly id: string;
      readonly action:
        | 'play'
        | 'pause'
        | 'stop'
        | 'arm-exit'
        | 'take-exit'
        | 'cancel-exit';
    };

/** Every transport `action` discriminant (for exhaustive UI wiring). */
export type TransportAction = TransportVars['action'];

/**
 * Drive one player through a transport verb, returning the `202 Accepted` body.
 * The outcome arrives on the realtime stream (`media.player_state`).
 */
export function useMediaPlayerTransport(
  context: MediaPlayerContext = {},
): UseMutationResult<AcceptedBody, OperationApiError, TransportVars> {
  return useMutation<AcceptedBody, OperationApiError, TransportVars>({
    mutationFn: (vars): Promise<AcceptedBody> => {
      const base = options(context);
      switch (vars.action) {
        case 'load':
          return loadMediaPlayer(vars.id, vars.asset, base);
        case 'cue':
          return cueMediaPlayer(vars.id, vars.frame, base);
        case 'seek':
          return seekMediaPlayer(vars.id, vars.frame, base);
        case 'play':
          return playMediaPlayer(vars.id, base);
        case 'pause':
          return pauseMediaPlayer(vars.id, base);
        case 'stop':
          return stopMediaPlayer(vars.id, base);
        case 'arm-exit':
          return armMediaPlayerExit(vars.id, base);
        case 'take-exit':
          return takeMediaPlayerExit(vars.id, base);
        case 'cancel-exit':
          return cancelMediaPlayerExit(vars.id, base);
      }
    },
  });
}
