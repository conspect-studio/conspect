// mediaPlayerPresentation tests — the pure derivations behind the VT page:
// the config-vs-live honesty helpers, the live-only vamp gates, the focused
// slot rule, and the per-player operation-record model that drives the page's
// OperationFeedback region and its duplicate-action gating.
//
// The record model is now JOINED to the shared QueryClient operation tracker
// (../components/broadcast/operationTracker): these tests pin exactly what the
// page may claim. A 202 alone is `accepted` (OperationFeedback `pending`);
// `applied` comes ONLY from the tracker record settled by exact terminal
// evidence; an unverifiable error or an unobserved tracker deadline is
// `unknown`, never `failed` and never a rollback claim; a capacity refusal
// happens BEFORE any POST and is neither failed nor unknown.
import { describe, expect, it } from 'vitest';
import { QueryClient } from '@tanstack/react-query';
import { setStoredToken } from '../api/token';

import { OperationApiError } from '../api/operations';
import type { MediaPlayer } from '../api/media-playersQueries';
import type { MediaPlayerEvent } from '../realtime/generated-types';
import { OperationOutcomeError } from '../components/broadcast/operationOutcomes';
import type { OperationRecord } from '../components/broadcast/operationOutcomes';
import {
  classifySubmissionError,
  classifyTransportError,
  configuredDefaultAsset,
  configuredLoop,
  effectiveSelectedPlayerId,
  isExitArmed,
  isVamping,
  MEDIA_PLAYER_OPERATIONS_QUERY_KEY,
  operationFeedbackView,
  readMediaPlayerOperations,
  replaceMediaPlayerOperation,
  unresolvedMediaPlayerOperations,
  mediaPlayerIdentity,
  mediaPlayerOperationsKey,
  stateVariant,
  transportExpectation,
  unresolvedVerb,
  updateMediaPlayerOperation,
  vampGate,
  verbBlockedByOperation,
} from './mediaPlayerPresentation';
import type { MediaPlayerOperation } from './mediaPlayerPresentation';

function playerOf(body: unknown): MediaPlayer {
  return { id: 'vt-1', name: 'VT 1', body };
}

function liveEvent(state: MediaPlayerEvent['state'], asset?: string): MediaPlayerEvent {
  return { player: 'vt-1', position_frames: 12, state, ...(asset !== undefined ? { asset } : {}) };
}

/** The caller-localized explanation the page passes for tracker-derived unknowns. */
const TRACKER_UNKNOWN =
  'The applied event was not observed on the realtime stream. The command may or may not have been applied.';

/** A tracker record for the page's operation id, in the given phase. */
function tracked(phase: OperationRecord['phase']): OperationRecord {
  return { operationId: 'op-1', corr: 'op-1', topic: 'switcher', resource: 'vt-1',
    event: 'media.player_state', phase };
}

/** The page's accepted record for a correlated verb. */
const ACCEPTED_PLAY: MediaPlayerOperation = { phase: 'accepted', operationId: 'op-1',
  verb: 'play', label: 'Play VT 1', uncorrelated: false };

describe('configured-default extraction', () => {
  it('reads a declared default asset id off the config body', () => {
    expect(configuredDefaultAsset(playerOf({ default: 'opener' }))).toBe('opener');
  });

  it('returns nothing for an absent, non-string or bodyless default', () => {
    expect(configuredDefaultAsset(playerOf({}))).toBeUndefined();
    expect(configuredDefaultAsset(playerOf({ default: 42 }))).toBeUndefined();
    expect(configuredDefaultAsset(playerOf('not-a-record'))).toBeUndefined();
  });

  it('treats loop_default === true as the only configured-loop truth', () => {
    expect(configuredLoop(playerOf({ loop_default: true }))).toBe(true);
    expect(configuredLoop(playerOf({ loop_default: false }))).toBe(false);
    expect(configuredLoop(playerOf({}))).toBe(false);
    expect(configuredLoop(playerOf('not-a-record'))).toBe(false);
  });
});

describe('state badge variants', () => {
  it('maps each live transport kind to its badge variant', () => {
    expect(stateVariant('playing')).toBe('live');
    expect(stateVariant('vamping')).toBe('live');
    expect(stateVariant('cued')).toBe('reconnecting');
    expect(stateVariant('paused')).toBe('reconnecting');
    expect(stateVariant('loading')).toBe('stale');
    expect(stateVariant('eof')).toBe('nosignal');
    expect(stateVariant('stopped')).toBe('offline');
  });
});

describe('vamp gates — live truth only', () => {
  it('a missing live observation gates every exit key as unknown — never "not vamping"', () => {
    expect(isVamping(undefined)).toBe(false);
    expect(isExitArmed(undefined)).toBe(false);
    expect(vampGate(undefined, 'arm')).toBe('no-live-state');
    expect(vampGate(undefined, 'take')).toBe('no-live-state');
    expect(vampGate(undefined, 'cancel')).toBe('no-live-state');
  });

  it('a non-vamping live state says so exactly', () => {
    const live = liveEvent({ kind: 'playing' });
    expect(vampGate(live, 'arm')).toBe('not-vamping');
    expect(vampGate(live, 'take')).toBe('not-vamping');
    expect(vampGate(live, 'cancel')).toBe('no-armed-exit');
  });

  it('a mid-vamp unarmed player may arm or take, but has no exit to cancel', () => {
    const live = liveEvent({ kind: 'vamping', exit_armed: false });
    expect(isVamping(live)).toBe(true);
    expect(isExitArmed(live)).toBe(false);
    expect(vampGate(live, 'arm')).toBeUndefined();
    expect(vampGate(live, 'take')).toBeUndefined();
    expect(vampGate(live, 'cancel')).toBe('no-armed-exit');
  });

  it('an armed vamp may take or cancel, but not arm twice', () => {
    const live = liveEvent({ kind: 'vamping', exit_armed: true });
    expect(isExitArmed(live)).toBe(true);
    expect(vampGate(live, 'arm')).toBe('exit-already-armed');
    expect(vampGate(live, 'take')).toBeUndefined();
    expect(vampGate(live, 'cancel')).toBeUndefined();
  });
});

describe('transport expectations — mirror CorrKey::for_command', () => {
  it('maps every correlated verb to its single unambiguous terminal state', () => {
    // crates/multiview-control/src/realtime.rs: play→Playing, pause→Paused,
    // stop→Stopped, cue→Cued — the state the COMMAND transitions into, not
    // just any state change.
    expect(transportExpectation('vt-1', 'play')).toEqual({
      topic: 'switcher', resource: 'vt-1', event: 'media.player_state',
      terminal: { kind: 'player-state', state: 'playing' },
    });
    expect(transportExpectation('vt-1', 'pause')).toEqual({
      topic: 'switcher', resource: 'vt-1', event: 'media.player_state',
      terminal: { kind: 'player-state', state: 'paused' },
    });
    expect(transportExpectation('vt-1', 'stop')).toEqual({
      topic: 'switcher', resource: 'vt-1', event: 'media.player_state',
      terminal: { kind: 'player-state', state: 'stopped' },
    });
    expect(transportExpectation('vt-1', 'cue')).toEqual({
      topic: 'switcher', resource: 'vt-1', event: 'media.player_state',
      terminal: { kind: 'player-state', state: 'cued' },
    });
  });

  it('keys arm/take to vamping with the exit armed and cancel to it disarmed', () => {
    const armed = {
      topic: 'switcher', resource: 'vt-1', event: 'media.player_state',
      terminal: { kind: 'player-state', state: 'vamping', exitArmed: true },
    } as const;
    expect(transportExpectation('vt-1', 'arm-exit')).toEqual(armed);
    expect(transportExpectation('vt-1', 'take-exit')).toEqual(armed);
    expect(transportExpectation('vt-1', 'cancel-exit')).toEqual({
      topic: 'switcher', resource: 'vt-1', event: 'media.player_state',
      terminal: { kind: 'player-state', state: 'vamping', exitArmed: false },
    });
  });

  it('leaves load and seek uncorrelated — no guessed expected media state', () => {
    // CorrKey::for_command maps Load/Seek to None (ambiguous outcome): the
    // submission is accepted-unobserved and no state may be guessed.
    expect(transportExpectation('vt-1', 'load')).toBeUndefined();
    expect(transportExpectation('vt-1', 'seek')).toBeUndefined();
  });
});

describe('operation feedback view — accepted ≠ applied until the tracker joins', () => {
  it('renders nothing for a clean slate, an in-flight submission or a local refusal', () => {
    expect(operationFeedbackView(undefined, undefined, TRACKER_UNKNOWN)).toBeUndefined();
    // The pre-202 window never shows the accepted wording.
    expect(operationFeedbackView(
      { phase: 'sending', verb: 'play', label: 'Play VT 1' }, undefined, TRACKER_UNKNOWN,
    )).toBeUndefined();
    // A capacity refusal happened before any POST: no command outcome exists.
    expect(operationFeedbackView(
      { phase: 'refused', reason: 'full', label: 'Play VT 1' }, undefined, TRACKER_UNKNOWN,
    )).toBeUndefined();
  });

  it('keeps a correlated acceptance pending until the tracker holds terminal evidence', () => {
    expect(operationFeedbackView(ACCEPTED_PLAY, undefined, TRACKER_UNKNOWN))
      .toEqual({ phase: 'pending', correlationId: 'op-1' });
    expect(operationFeedbackView(ACCEPTED_PLAY, tracked('accepted'), TRACKER_UNKNOWN))
      .toEqual({ phase: 'pending', correlationId: 'op-1' });
  });

  it('resolves applied only from the tracker record — exact terminal evidence', () => {
    expect(operationFeedbackView(ACCEPTED_PLAY, tracked('applied'), TRACKER_UNKNOWN))
      .toEqual({ phase: 'applied', correlationId: 'op-1' });
  });

  it('maps a tracker unknown (deadline/gap) to unknown with the caller reason and id', () => {
    expect(operationFeedbackView(ACCEPTED_PLAY, tracked('unknown'), TRACKER_UNKNOWN))
      .toEqual({ phase: 'unknown', unknownReason: TRACKER_UNKNOWN, correlationId: 'op-1' });
  });

  it('renders an uncorrelated acceptance (load/seek) as accepted-unobserved, never applied', () => {
    const load: MediaPlayerOperation = { phase: 'accepted', operationId: 'op-77',
      verb: 'load', label: 'Load VT 1', uncorrelated: true };
    expect(operationFeedbackView(load, undefined, TRACKER_UNKNOWN))
      .toEqual({ phase: 'accepted-unobserved', correlationId: 'op-77' });
  });

  it('maps a definitive rejection to failed with its reason', () => {
    expect(operationFeedbackView(
      { phase: 'failed', reason: 'Player busy', label: 'Play VT 1' }, undefined, TRACKER_UNKNOWN,
    )).toEqual({ phase: 'failed', failureReason: 'Player busy' });
  });

  it('maps an unverifiable outcome to unknown with its reason', () => {
    expect(operationFeedbackView(
      { phase: 'unknown', verb: 'play', reason: 'No answer', label: 'Play VT 1' },
      undefined, TRACKER_UNKNOWN,
    )).toEqual({ phase: 'unknown', unknownReason: 'No answer' });
  });

  it('never derives an applied verdict without the tracker record', () => {
    const records = [
      { phase: 'sending', verb: 'play', label: 'Play VT 1' },
      ACCEPTED_PLAY,
      { phase: 'failed', reason: 'no', label: 'Play VT 1' },
      { phase: 'unknown', verb: 'play', reason: 'no', label: 'Play VT 1' },
      { phase: 'refused', reason: 'full', label: 'Play VT 1' },
    ] as const;
    // Without the tracker's authoritative evidence the derived view is at
    // most pending/failed/unknown/accepted-unobserved — `applied` exists only
    // via a settled tracker record.
    const allowed: readonly string[] = ['pending', 'failed', 'unknown', 'accepted-unobserved'];
    for (const record of records) {
      const view = operationFeedbackView(record, undefined, TRACKER_UNKNOWN);
      if (view !== undefined) {
        expect(allowed).toContain(view.phase);
      }
    }
  });
});

describe('duplicate-action gating with the tracker join', () => {
  it('blocks nothing without a record', () => {
    expect(verbBlockedByOperation(undefined, 'play')).toBe(false);
  });

  it('blocks every verb of the player while a submission is in flight', () => {
    const op = { phase: 'sending', verb: 'play', label: 'Play VT 1' } as const;
    for (const verb of ['load', 'cue', 'play', 'pause', 'stop', 'arm-exit', 'take-exit', 'cancel-exit'] as const) {
      expect(verbBlockedByOperation(op, verb)).toBe(true);
    }
  });

  it('never blocks on an accepted 202, an applied join or a definitive failure', () => {
    expect(verbBlockedByOperation(ACCEPTED_PLAY, 'play', tracked('accepted'))).toBe(false);
    expect(verbBlockedByOperation(ACCEPTED_PLAY, 'play', tracked('applied'))).toBe(false);
    expect(verbBlockedByOperation({ phase: 'failed', reason: 'no', label: 'Play VT 1' }, 'play')).toBe(false);
  });

  it('blocks only the duplicate of the unresolved verb on an unknown outcome', () => {
    const op = { phase: 'unknown', verb: 'play', reason: 'No answer', label: 'Play VT 1' } as const;
    expect(verbBlockedByOperation(op, 'play')).toBe(true);
    for (const verb of ['load', 'cue', 'pause', 'stop', 'arm-exit', 'take-exit', 'cancel-exit'] as const) {
      expect(verbBlockedByOperation(op, verb)).toBe(false);
    }
  });

  it('gates the duplicate verb once the tracker reports the outcome unknown (deadline/gap)', () => {
    expect(unresolvedVerb(ACCEPTED_PLAY, tracked('unknown'))).toBe('play');
    expect(verbBlockedByOperation(ACCEPTED_PLAY, 'play', tracked('unknown'))).toBe(true);
    // Every other verb stays usable — the page never deadlocks.
    for (const verb of ['load', 'cue', 'pause', 'stop', 'arm-exit', 'take-exit', 'cancel-exit'] as const) {
      expect(verbBlockedByOperation(ACCEPTED_PLAY, verb, tracked('unknown'))).toBe(false);
    }
  });

  it('never gates on an uncorrelated acceptance (load/seek): no duplicate uncertainty', () => {
    const load = { phase: 'accepted', operationId: 'op-77', verb: 'load',
      label: 'Load VT 1', uncorrelated: true } as const;
    expect(unresolvedVerb(load, undefined)).toBeUndefined();
    expect(verbBlockedByOperation(load, 'load')).toBe(false);
  });

  it('a capacity refusal (before any POST) blocks nothing — the tracker re-refuses safely', () => {
    const op = { phase: 'refused', reason: 'full', label: 'Play VT 1' } as const;
    expect(unresolvedVerb(op, undefined)).toBeUndefined();
    for (const verb of ['load', 'cue', 'play', 'pause', 'stop', 'arm-exit', 'take-exit', 'cancel-exit'] as const) {
      expect(verbBlockedByOperation(op, verb)).toBe(false);
    }
  });
});

describe('submission error classification', () => {
  it('a tracker capacity refusal is a local refusal carrying the localized message', () => {
    // The hook wraps OperationOutcomeError('capacity') in a localized Error.
    const wrapped = new Error(
      'Too many unresolved commands. Wait for an outcome or resolve an unknown command before retrying.',
      { cause: new OperationOutcomeError('capacity') },
    );
    const op = classifySubmissionError(wrapped, 'play', 'Play VT 1', 'explanation');
    // Refused — never `failed` (nothing was sent) and never `unknown` (the
    // command was NOT submitted, so its outcome is not uncertain).
    expect(op).toEqual({
      phase: 'refused',
      reason: 'Too many unresolved commands. Wait for an outcome or resolve an unknown command before retrying.',
      label: 'Play VT 1',
    });
  });

  it('recognizes an unwrapped capacity error too', () => {
    const op = classifySubmissionError(
      new OperationOutcomeError('capacity'), 'play', 'Play VT 1', 'explanation',
    );
    expect(op.phase).toBe('refused');
  });

  it('keeps a definitive HTTP rejection a failure and everything else unknown', () => {
    expect(classifySubmissionError(
      new OperationApiError('Player busy', 409, 'The deck is mid-load'),
      'play', 'Play VT 1', 'explanation',
    ).phase).toBe('failed');
    expect(classifySubmissionError(
      new OperationApiError('The server returned an unexpected command body.'),
      'play', 'Play VT 1', 'explanation',
    ).phase).toBe('unknown');
    expect(classifySubmissionError(
      new TypeError('Failed to fetch'), 'play', 'Play VT 1', 'explanation',
    ).phase).toBe('unknown');
  });
});

describe('transport error classification', () => {
  it('a definitive HTTP rejection is a failure carrying the server reason', () => {
    const op = classifyTransportError(
      new OperationApiError('Player busy', 409, 'The deck is mid-load'),
      'play',
      'Play VT 1',
      'explanation',
    );
    expect(op).toEqual({ phase: 'failed', reason: 'Player busy: The deck is mid-load', label: 'Play VT 1' });
  });

  it('only a definite 4xx/5xx HTTP status is a failure — mirroring the tracker', () => {
    // A 3xx/6xx+ status is not a definitive rejection the tracker would
    // release a seat for, so the page must not claim failure from it either.
    expect(classifyTransportError(
      new OperationApiError('See other', 303), 'play', 'Play VT 1', 'explanation',
    ).phase).toBe('unknown');
    expect(classifyTransportError(
      new OperationApiError('Weird', 600), 'play', 'Play VT 1', 'explanation',
    ).phase).toBe('unknown');
  });

  it('a 2xx with an unparseable body is unknown — the command may have been accepted', () => {
    const op = classifyTransportError(
      new OperationApiError('The server returned an unexpected command body.'),
      'play',
      'Play VT 1',
      'The control plane did not answer.',
    );
    expect(op.phase).toBe('unknown');
    expect(op).toMatchObject({ verb: 'play' });
  });

  it('a transport-level fetch error is unknown with the explanation and the raw detail', () => {
    const op = classifyTransportError(
      new TypeError('Failed to fetch'),
      'play',
      'Play VT 1',
      'The control plane did not answer.',
    );
    expect(op.phase).toBe('unknown');
    if (op.phase === 'unknown') {
      expect(op.reason).toMatch(/The control plane did not answer\. \(Failed to fetch\)/);
      // The page-authored sentence (before the parenthesized raw detail)
      // claims neither failure nor rollback.
      expect(op.reason.replace(/\((.*)\)$/, '')).not.toMatch(/failed|rollback/i);
    }
  });
});

describe('per-player operation store — QueryClient-backed, survives navigation', () => {
  it('persists the latest record per player in the QueryClient', () => {
    const client = new QueryClient();
    expect(readMediaPlayerOperations(client)).toEqual({});
    updateMediaPlayerOperation(client, 'vt-1', () => ACCEPTED_PLAY);
    updateMediaPlayerOperation(client, 'vt-2', () => ({ phase: 'sending', verb: 'play', label: 'Play VT 2' }));
    expect(readMediaPlayerOperations(client)['vt-1']).toMatchObject({ phase: 'accepted', operationId: 'op-1' });
    expect(readMediaPlayerOperations(client)['vt-2']).toMatchObject({ phase: 'sending' });
    // The store lives under the stable page key, not in component state, so
    // navigating away and back keeps the feedback region truthful.
    expect(client.getQueryData(MEDIA_PLAYER_OPERATIONS_QUERY_KEY)).toBeDefined();
  });

  it('is latest-wins per player and can derive from the previous record', () => {
    const client = new QueryClient();
    updateMediaPlayerOperation(client, 'vt-1', () => ACCEPTED_PLAY);
    // A dismiss only clears a definitive failure — never an accepted record.
    updateMediaPlayerOperation(client, 'vt-1', (prev) => (prev?.phase === 'failed' ? undefined : prev));
    expect(readMediaPlayerOperations(client)['vt-1']).toMatchObject({ phase: 'accepted' });
    updateMediaPlayerOperation(client, 'vt-1', () => ({ phase: 'failed', reason: 'no', label: 'Play VT 1' }));
    updateMediaPlayerOperation(client, 'vt-1', (prev) => (prev?.phase === 'failed' ? undefined : prev));
    expect(readMediaPlayerOperations(client)['vt-1']).toBeUndefined();
  });
});

describe('preserved uncertainty and credential epochs', () => {
  it('retains unresolved play separately from latest pause and resolves only with tracker evidence', () => {
    const unknown: MediaPlayerOperation = { phase: 'unknown', verb: 'play', label: 'Play', reason: 'No answer' };
    const sending: MediaPlayerOperation = { phase: 'sending', verb: 'pause', label: 'Pause' };
    const pause = replaceMediaPlayerOperation(replaceMediaPlayerOperation(unknown, sending), {
      phase: 'accepted', verb: 'pause', label: 'Pause', operationId: 'op-pause', uncorrelated: false,
    });
    expect(unresolvedMediaPlayerOperations(pause, () => undefined)).toMatchObject([{ phase: 'unknown', verb: 'play' }]);
    expect(pause.prior?.some((entry) => entry.phase === 'unknown')).toBe(true);
    expect(verbBlockedByOperation(pause, 'pause')).toBe(false);
    expect(replaceMediaPlayerOperation(ACCEPTED_PLAY, sending, () => tracked('applied')).prior).toEqual([]);
  });

  it('never puts credentials into a key and clears old page records on identity change', () => {
    setStoredToken('principal-one');
    const client = new QueryClient();
    const epoch = mediaPlayerIdentity(client);
    updateMediaPlayerOperation(client, 'vt-1', () => ACCEPTED_PLAY);
    setStoredToken('principal-two');
    const nextEpoch = mediaPlayerIdentity(client);
    expect(nextEpoch).toBe(epoch + 1);
    expect(readMediaPlayerOperations(client)).toEqual({});
    expect(client.getQueryData(MEDIA_PLAYER_OPERATIONS_QUERY_KEY)).toBeUndefined();
    expect(JSON.stringify(mediaPlayerOperationsKey(nextEpoch))).not.toContain('principal');
    setStoredToken('');
  });
});

describe('focused slot rule', () => {
  const players: readonly MediaPlayer[] = [
    { id: 'vt-1', name: 'VT 1', body: {} },
    { id: 'vt-2', name: 'VT 2', body: {} },
  ];

  it('focuses the first configured player by default', () => {
    expect(effectiveSelectedPlayerId(null, players)).toBe('vt-1');
    expect(effectiveSelectedPlayerId(undefined, players)).toBe('vt-1');
  });

  it('honours the operator-controlled pick', () => {
    expect(effectiveSelectedPlayerId('vt-2', players)).toBe('vt-2');
  });

  it('falls back to the first player when the picked slot disappears', () => {
    expect(effectiveSelectedPlayerId('vt-gone', players)).toBe('vt-1');
    expect(effectiveSelectedPlayerId('vt-2', [])).toBeUndefined();
  });
});
