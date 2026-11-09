// mediaPlayerPresentation tests — the pure derivations behind the VT page:
// the config-vs-live honesty helpers, the live-only vamp gates, the focused
// slot rule, and the per-player operation-record model that drives the page's
// OperationFeedback region and its duplicate-action gating.
//
// The record model is the page-side TRACKER ADAPTER SEAM: the realtime
// operation tracker is a separate workstream and is deliberately not imported
// here. These tests pin what the page may claim on its own: a 202 is
// `accepted` (OperationFeedback `pending`), never `applied`; an unverifiable
// error is `unknown`, never `failed` and never a rollback claim.
import { describe, expect, it } from 'vitest';

import { OperationApiError } from '../api/operations';
import type { MediaPlayer } from '../api/media-playersQueries';
import type { MediaPlayerEvent } from '../realtime/generated-types';
import {
  classifyTransportError,
  configuredDefaultAsset,
  configuredLoop,
  effectiveSelectedPlayerId,
  isExitArmed,
  isVamping,
  operationFeedbackView,
  stateVariant,
  vampGate,
  verbBlockedByOperation,
} from './mediaPlayerPresentation';

function playerOf(body: unknown): MediaPlayer {
  return { id: 'vt-1', name: 'VT 1', body };
}

function liveEvent(state: MediaPlayerEvent['state'], asset?: string): MediaPlayerEvent {
  return { player: 'vt-1', position_frames: 12, state, ...(asset !== undefined ? { asset } : {}) };
}

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

describe('operation feedback view — accepted ≠ applied', () => {
  it('renders nothing for a clean slate or an in-flight submission', () => {
    expect(operationFeedbackView(undefined)).toBeUndefined();
    // The pre-202 window never shows the accepted wording.
    expect(
      operationFeedbackView({ phase: 'sending', verb: 'play', label: 'Play VT 1' }),
    ).toBeUndefined();
  });

  it('maps a real 202 to pending with the correlation id', () => {
    expect(operationFeedbackView({ phase: 'accepted', operationId: 'op-9001', label: 'Play VT 1' }))
      .toEqual({ phase: 'pending', correlationId: 'op-9001' });
  });

  it('maps a definitive rejection to failed with its reason', () => {
    expect(operationFeedbackView({ phase: 'failed', reason: 'Player busy', label: 'Play VT 1' }))
      .toEqual({ phase: 'failed', failureReason: 'Player busy' });
  });

  it('maps an unverifiable outcome to unknown with its reason', () => {
    expect(
      operationFeedbackView({ phase: 'unknown', verb: 'play', reason: 'No answer', label: 'Play VT 1' }),
    ).toEqual({ phase: 'unknown', unknownReason: 'No answer' });
  });

  it('never derives an applied verdict from the page-local record alone', () => {
    const records = [
      { phase: 'sending', verb: 'play', label: 'Play VT 1' },
      { phase: 'accepted', operationId: 'op-1', label: 'Play VT 1' },
      { phase: 'failed', reason: 'no', label: 'Play VT 1' },
      { phase: 'unknown', verb: 'play', reason: 'no', label: 'Play VT 1' },
    ] as const;
    // The derived view is at most pending/failed/unknown: `applied` exists
    // only on the OperationFeedback phase union and this model can never
    // reach it without the tracker's authoritative evidence.
    const allowed: readonly string[] = ['pending', 'failed', 'unknown'];
    for (const record of records) {
      const view = operationFeedbackView(record);
      if (view !== undefined) {
        expect(allowed).toContain(view.phase);
      }
    }
  });
});

describe('duplicate-action gating', () => {
  it('blocks nothing without a record', () => {
    expect(verbBlockedByOperation(undefined, 'play')).toBe(false);
  });

  it('blocks every verb of the player while a submission is in flight', () => {
    const op = { phase: 'sending', verb: 'play', label: 'Play VT 1' } as const;
    for (const verb of ['load', 'cue', 'play', 'pause', 'stop', 'arm-exit', 'take-exit', 'cancel-exit'] as const) {
      expect(verbBlockedByOperation(op, verb)).toBe(true);
    }
  });

  it('never blocks on an accepted 202 or a definitive failure — no page deadlock without an event join', () => {
    expect(verbBlockedByOperation({ phase: 'accepted', operationId: 'op-1', label: 'Play VT 1' }, 'play')).toBe(false);
    expect(verbBlockedByOperation({ phase: 'failed', reason: 'no', label: 'Play VT 1' }, 'play')).toBe(false);
  });

  it('blocks only the duplicate of the possibly-in-flight verb on an unknown outcome', () => {
    const op = { phase: 'unknown', verb: 'play', reason: 'No answer', label: 'Play VT 1' } as const;
    expect(verbBlockedByOperation(op, 'play')).toBe(true);
    for (const verb of ['load', 'cue', 'pause', 'stop', 'arm-exit', 'take-exit', 'cancel-exit'] as const) {
      expect(verbBlockedByOperation(op, verb)).toBe(false);
    }
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
