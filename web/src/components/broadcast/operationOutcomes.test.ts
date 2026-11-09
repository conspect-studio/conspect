// operationOutcomes tests — the accepted→applied/failed/unknown truth table:
// terminal evidence must match corr+topic(+pinned resource/event) EXACTLY;
// `job.progress` is never terminal; uncorrelated frames never succeed; a
// deadline yields `unknown`, never a fake failure (and no deadline never ages
// out — and there is no rollback claim anywhere in the vocabulary); the
// early-terminal cache is bounded with safe drop-oldest eviction; settled
// records are never regressed by replays or late conflicting evidence; and
// the settled-history bound evicts oldest settled records but never an
// unresolved operation.
import { describe, expect, it } from 'vitest';

import {
  EARLY_EVIDENCE_MAX,
  NON_TERMINAL_EVENTS,
  OPERATION_RECORDS_MAX,
  reserveOperationOutcome,
  admitOperationOutcome,
  releaseOperationReservation,
  markOperationResponseUnknown,
  readProvisionalOperation,
  applyOperationAcceptance,
  applyTerminalEvidence,
  createOperationOutcomeState,
  expireOperationDeadlines,
  readOperationOutcome,
} from './operationOutcomes';
import type {
  OperationAcceptance,
  OperationOutcomeState,
  TerminalEvidence,
} from './operationOutcomes';

function accept(
  over: {
    operationId?: string;
    corr?: string;
    topic?: string;
    resource?: string;
    event?: string;
    kind?: string;
    acceptedAt?: number;
    deadlineAt?: number;
  } = {},
): OperationAcceptance {
  return {
    operationId: over.operationId ?? 'op-1',
    corr: over.corr ?? 'corr-1',
    topic: over.topic ?? 'outputs',
    resource: over.resource,
    event: over.event,
    kind: over.kind,
    acceptedAt: over.acceptedAt,
    deadlineAt: over.deadlineAt,
  };
}

function evidence(
  over: {
    corr?: string;
    topic?: string;
    event?: string;
    resource?: string;
    verdict?: 'applied' | 'failed';
    at?: number;
  } = {},
): TerminalEvidence {
  return {
    corr: over.corr ?? 'corr-1',
    topic: over.topic ?? 'outputs',
    event: over.event,
    resource: over.resource,
    verdict: over.verdict ?? 'applied',
    at: over.at,
  };
}

describe('operationOutcomes — acceptance', () => {
  it('records an accepted operation with its exact expectation', () => {
    const state = applyOperationAcceptance(
      createOperationOutcomeState(),
      accept({
        resource: 'out-1',
        event: 'output.status',
        kind: 'output.start',
        acceptedAt: 10,
        deadlineAt: 90,
      }),
    );
    const record = readOperationOutcome(state, 'op-1');
    expect(record).toEqual({
      operationId: 'op-1',
      corr: 'corr-1',
      topic: 'outputs',
      resource: 'out-1',
      event: 'output.status',
      kind: 'output.start',
      phase: 'accepted',
      acceptedAt: 10,
      deadlineAt: 90,
      settledAt: undefined,
    });
  });

  it('refuses trackless acceptances: no id, no corr, or no topic changes nothing', () => {
    const start = createOperationOutcomeState();
    expect(() => applyOperationAcceptance(start, accept({ operationId: '' }))).toThrow(/invalid/);
    // An uncorrelated command can never be joined to evidence, so it is not
    // outcome-trackable at all.
    expect(() => applyOperationAcceptance(start, accept({ corr: '' }))).toThrow(/invalid/);
    expect(() => applyOperationAcceptance(start, accept({ topic: '' }))).toThrow(/invalid/);
    expect(start.records.size).toBe(0);
  });

  it('returns undefined for an unknown operation id', () => {
    expect(
      readOperationOutcome(createOperationOutcomeState(), 'nope'),
    ).toBeUndefined();
  });

  it('a replayed acceptance never regresses a settled record', () => {
    let state = applyOperationAcceptance(
      createOperationOutcomeState(),
      accept({ deadlineAt: 1000 }),
    );
    state = applyTerminalEvidence(state, evidence({ at: 20 }));
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('applied');
    const replayed = applyOperationAcceptance(state, accept({ deadlineAt: 2000 }));
    expect(replayed).toBe(state);
    expect(readOperationOutcome(replayed, 'op-1')?.phase).toBe('applied');
  });

  it('a replayed acceptance may only re-arm the deadline while still accepted', () => {
    let state = applyOperationAcceptance(createOperationOutcomeState(), accept());
    state = applyOperationAcceptance(state, accept({ deadlineAt: 5000 }));
    const record = readOperationOutcome(state, 'op-1');
    expect(record?.phase).toBe('accepted');
    expect(record?.deadlineAt).toBe(5000);
    // The re-armed deadline still expires to unknown, never a fake failure.
    state = expireOperationDeadlines(state, 5000);
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('unknown');
  });
});

describe('operationOutcomes — terminal evidence must match exactly', () => {
  it('settles applied with the evidence timestamp', () => {
    let state = applyOperationAcceptance(createOperationOutcomeState(), accept());
    state = applyTerminalEvidence(state, evidence({ at: 42 }));
    expect(readOperationOutcome(state, 'op-1')).toEqual({
      operationId: 'op-1',
      corr: 'corr-1',
      topic: 'outputs',
      resource: undefined,
      event: undefined,
      kind: undefined,
      phase: 'applied',
      acceptedAt: undefined,
      deadlineAt: undefined,
      settledAt: 42,
    });
  });

  it('settles failed evidence as failed', () => {
    let state = applyOperationAcceptance(createOperationOutcomeState(), accept());
    state = applyTerminalEvidence(
      state,
      evidence({ verdict: 'failed', at: 42 }),
    );
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('failed');
  });

  it('a same-corr frame on the wrong topic settles nothing (and waits for its own acceptance)', () => {
    let state = applyOperationAcceptance(createOperationOutcomeState(), accept());
    state = applyTerminalEvidence(state, evidence({ corr: 'switcher-corr', topic: 'switcher', at: 5 }));
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('accepted');
    // The frame is cached for an acceptance expecting that topic…
    expect(state.earlyEvidence).toHaveLength(1);
    // …which it then settles, while the outputs op stays honestly accepted.
    state = applyOperationAcceptance(
      state,
      accept({ operationId: 'op-2', corr: 'switcher-corr', topic: 'switcher' }),
    );
    expect(readOperationOutcome(state, 'op-2')?.phase).toBe('applied');
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('accepted');
    expect(state.earlyEvidence).toHaveLength(0);
  });

  it('a frame naming the wrong resource never settles a pinned expectation', () => {
    let state = applyOperationAcceptance(
      createOperationOutcomeState(),
      accept({ resource: 'out-1' }),
    );
    state = applyTerminalEvidence(state, evidence({ resource: 'out-2', at: 5 }));
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('accepted');
    // A resource-less frame cannot satisfy a pinned resource either.
    state = applyTerminalEvidence(state, evidence({ at: 6 }));
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('accepted');
    // The exact resource does settle it.
    state = applyTerminalEvidence(state, evidence({ resource: 'out-1', at: 7 }));
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('applied');
  });

  it('a pinned event type must match exactly', () => {
    let state = applyOperationAcceptance(
      createOperationOutcomeState(),
      accept({ event: 'output.status' }),
    );
    state = applyTerminalEvidence(
      state,
      evidence({ event: 'output.link_state', at: 5 }),
    );
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('accepted');
    state = applyTerminalEvidence(
      state,
      evidence({ event: 'output.status', at: 6 }),
    );
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('applied');
  });

  it('job.progress is never terminal: it settles nothing and is never cached', () => {
    expect(NON_TERMINAL_EVENTS.has('job.progress')).toBe(true);
    const state = applyOperationAcceptance(
      createOperationOutcomeState(),
      accept({ topic: 'jobs', event: 'job.progress' }),
    );
    const withProgress = applyTerminalEvidence(
      state,
      evidence({ topic: 'jobs', event: 'job.progress', at: 5 }),
    );
    expect(withProgress).toBe(state);
    expect(readOperationOutcome(withProgress, 'op-1')?.phase).toBe('accepted');
    expect(withProgress.earlyEvidence).toHaveLength(0);
  });

  it('uncorrelated frames never settle anything and are not cached', () => {
    const state = applyOperationAcceptance(createOperationOutcomeState(), accept());
    const uncorrelated = applyTerminalEvidence(state, evidence({ corr: '' }));
    expect(uncorrelated).toBe(state);
    expect(readOperationOutcome(uncorrelated, 'op-1')?.phase).toBe('accepted');
    expect(uncorrelated.earlyEvidence).toHaveLength(0);
  });

  it('a correlation id cannot be accepted for two operation identities', () => {
    let state = applyOperationAcceptance(
      createOperationOutcomeState(),
      accept({ operationId: 'op-a' }),
    );
    expect(() => applyOperationAcceptance(state, accept({ operationId: 'op-b' }))).toThrow(/identity/);
    state = applyTerminalEvidence(state, evidence({ at: 9 }));
    expect(readOperationOutcome(state, 'op-a')?.phase).toBe('applied');
    expect(readOperationOutcome(state, 'op-b')).toBeUndefined();
  });

  it('the first terminal verdict stands: late conflicting evidence flips nothing and is not cached', () => {
    let state = applyOperationAcceptance(createOperationOutcomeState(), accept());
    state = applyTerminalEvidence(state, evidence({ at: 10 }));
    const late = applyTerminalEvidence(
      state,
      evidence({ verdict: 'failed', at: 11 }),
    );
    expect(readOperationOutcome(late, 'op-1')?.phase).toBe('applied');
    expect(readOperationOutcome(late, 'op-1')?.settledAt).toBe(10);
    expect(late.earlyEvidence).toHaveLength(0);
  });
});

describe('operationOutcomes — deadlines', () => {
  it('a passed deadline yields unknown — never a fake failure, never a rollback claim', () => {
    let state = applyOperationAcceptance(
      createOperationOutcomeState(),
      accept({ deadlineAt: 5000, acceptedAt: 1000 }),
    );
    state = expireOperationDeadlines(state, 4999);
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('accepted');
    state = expireOperationDeadlines(state, 5000);
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('unknown');
    // Further ticks never escalate unknown into anything else.
    state = expireOperationDeadlines(state, 60_000);
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('unknown');
    expect(readOperationOutcome(state, 'op-1')?.settledAt).toBeUndefined();
  });

  it('an operation with no deadline never ages out (deadline unknown ≠ fake fail)', () => {
    let state = applyOperationAcceptance(createOperationOutcomeState(), accept());
    state = expireOperationDeadlines(state, 10 ** 12);
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('accepted');
  });

  it('late exact terminal evidence reconciles an expired record honestly', () => {
    let state = applyOperationAcceptance(
      createOperationOutcomeState(),
      accept({ deadlineAt: 5000 }),
    );
    state = expireOperationDeadlines(state, 5000);
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('unknown');
    state = applyTerminalEvidence(state, evidence({ at: 7000 }));
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('applied');
    expect(readOperationOutcome(state, 'op-1')?.settledAt).toBe(7000);
  });
});

describe('operationOutcomes — bounded early-terminal cache', () => {
  it('caches a terminal frame that arrives before its acceptance, then joins it', () => {
    let state = applyTerminalEvidence(
      createOperationOutcomeState(),
      evidence({ corr: 'corr-9', resource: 'out-9', at: 30 }),
    );
    expect(state.earlyEvidence).toHaveLength(1);
    state = applyOperationAcceptance(
      state,
      accept({
        operationId: 'op-9',
        corr: 'corr-9',
        resource: 'out-9',
        acceptedAt: 31,
      }),
    );
    expect(readOperationOutcome(state, 'op-9')?.phase).toBe('applied');
    expect(readOperationOutcome(state, 'op-9')?.settledAt).toBe(30);
    expect(state.earlyEvidence).toHaveLength(0);
  });

  it('evicts drop-oldest at the bound, and eviction is safe (no fabricated outcome)', () => {
    let state = createOperationOutcomeState();
    for (let i = 0; i <= EARLY_EVIDENCE_MAX; i += 1) {
      state = applyTerminalEvidence(
        state,
        evidence({ corr: `early-${String(i)}`, at: i }),
      );
    }
    expect(state.earlyEvidence).toHaveLength(EARLY_EVIDENCE_MAX);
    // The oldest frame (early-0) was evicted: its acceptance finds nothing and
    // stays honestly accepted — that is the safe eviction path.
    state = applyOperationAcceptance(
      state,
      accept({ operationId: 'op-evicted', corr: 'early-0' }),
    );
    expect(readOperationOutcome(state, 'op-evicted')?.phase).toBe('accepted');
    // The newest frame (early-64) is still cached and settles its acceptance.
    state = applyOperationAcceptance(
      state,
      accept({ operationId: 'op-newest', corr: `early-${String(EARLY_EVIDENCE_MAX)}` }),
    );
    expect(readOperationOutcome(state, 'op-newest')?.phase).toBe('applied');
  });

  it('a wrong-topic early frame cannot block correct same-corr evidence', () => {
    let state = applyTerminalEvidence(createOperationOutcomeState(), evidence({ topic: 'wrong', verdict: 'failed', at: 1 }));
    state = applyTerminalEvidence(state, evidence({ topic: 'outputs', verdict: 'applied', at: 2 }));
    expect(state.earlyEvidence).toHaveLength(2);
    state = applyOperationAcceptance(state, accept());
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('applied');
  });

  it('does not cache a duplicate frame twice', () => {
    const frame = evidence({ corr: 'corr-d', at: 8 });
    const state = applyTerminalEvidence(createOperationOutcomeState(), frame);
    const deduped = applyTerminalEvidence(state, frame);
    expect(deduped).toBe(state);
    expect(state.earlyEvidence).toHaveLength(1);
  });
});

describe('operationOutcomes — bounded records', () => {
  it('keeps a hard cap and evicts only authoritative history', () => {
    let state: OperationOutcomeState = createOperationOutcomeState();
    for (let i = 0; i < OPERATION_RECORDS_MAX; i += 1) {
      state = applyOperationAcceptance(
        state,
        accept({
          operationId: `op-${String(i)}`,
          corr: `corr-${String(i)}`,
          deadlineAt: 1000 + i,
        }),
      );
    }
    expect(state.records.size).toBe(OPERATION_RECORDS_MAX);
    // All 256 are unresolved: the next acceptance must NOT evict any of them.
    expect(() => applyOperationAcceptance(
      state,
      accept({ operationId: 'op-256', corr: 'corr-256' }),
    )).toThrow(/capacity/);
    expect(state.records.size).toBe(OPERATION_RECORDS_MAX);
    expect(readOperationOutcome(state, 'op-0')?.phase).toBe('accepted');
    expect(readOperationOutcome(state, 'op-256')).toBeUndefined();
    // Capacity becomes available only by evicting authoritative history.
    state = applyTerminalEvidence(state, evidence({ corr: 'corr-0', at: 5 }));
    state = applyOperationAcceptance(state, accept({ operationId: 'op-257', corr: 'corr-257' }));
    expect(state.records.size).toBe(OPERATION_RECORDS_MAX);
    expect(readOperationOutcome(state, 'op-0')).toBeUndefined();
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('accepted');
    expect(readOperationOutcome(state, 'op-257')?.phase).toBe('accepted');
  });
});

describe('anonymous pre-submit admission', () => {
  it('binds real server-issued 202 identity in the same seat and joins an earlier terminal frame', () => {
    const seat = reserveOperationOutcome(createOperationOutcomeState());
    const body = { operation_id: 'op_8f2a', kind: 'output.start', applied_live: null, carried_only: null };
    const requestResource = 'out-1';
    let state = applyTerminalEvidence(seat.state, evidence({ corr: body.operation_id, resource: requestResource, event: 'output.status', at: 30 }));
    state = admitOperationOutcome(state, seat.token, accept({ operationId: body.operation_id,
      corr: body.operation_id, resource: requestResource, event: 'output.status', kind: body.kind, acceptedAt: 31 }));
    expect(state.records.size + state.provisional.size).toBe(1);
    expect(readProvisionalOperation(state, seat.token)).toBeUndefined();
    expect(readOperationOutcome(state, body.operation_id)?.phase).toBe('applied');
    expect(state.earlyEvidence).toHaveLength(0);
  });

  it('releases definitive HTTP rejection and pre-submit cancellation idempotently, without leaking 256 seats', () => {
    let state = createOperationOutcomeState();
    for (let i = 0; i < OPERATION_RECORDS_MAX; i++) {
      const seat = reserveOperationOutcome(state);
      state = releaseOperationReservation(seat.state, seat.token, 'pre-submit-cancelled');
      expect(releaseOperationReservation(state, seat.token, 'pre-submit-cancelled')).toBe(state);
    }
    expect(state.provisional.size).toBe(0);
    const rejected = reserveOperationOutcome(state);
    state = releaseOperationReservation(rejected.state, rejected.token, 'definitive-http-rejection');
    expect(state.records.size + state.provisional.size).toBe(0);
  });

  it('sent cancellation or network uncertainty retains the anonymous seat until identity is known', () => {
    const seat = reserveOperationOutcome(createOperationOutcomeState());
    let state = markOperationResponseUnknown(seat.state, seat.token, 'sent-cancelled');
    expect(readProvisionalOperation(state, seat.token)?.phase).toBe('unknown');
    expect(markOperationResponseUnknown(state, seat.token, 'transport-unknown')).toBe(state);
    expect(releaseOperationReservation(state, seat.token, 'pre-submit-cancelled')).toBe(state);
    state = expireOperationDeadlines(state, 10 ** 12);
    expect(readProvisionalOperation(state, seat.token)?.phase).toBe('unknown');
    state = admitOperationOutcome(state, seat.token, accept({ operationId: 'server-identity', corr: 'server-identity' }));
    expect(readOperationOutcome(state, 'server-identity')?.phase).toBe('accepted');
    expect(state.provisional.size).toBe(0);
  });

  it('does not double-count occupied seats or evict unresolved records', () => {
    let state = createOperationOutcomeState();
    for (let i = 0; i < OPERATION_RECORDS_MAX; i++) state = reserveOperationOutcome(state).state;
    expect(() => reserveOperationOutcome(state)).toThrow(/capacity/);
    const first = state.provisional.keys().next().value;
    expect(first).toBeDefined();
    state = admitOperationOutcome(state, first!, accept({ operationId: 'real', corr: 'real' }));
    expect(state.records.size + state.provisional.size).toBe(OPERATION_RECORDS_MAX);
    expect(() => reserveOperationOutcome(state)).toThrow(/capacity/);
  });
});

describe('admission, uncertain authority, and hostile input', () => {
  it('reserves before submission and rejects full unresolved capacity without dropping unknown', () => {
    let state = createOperationOutcomeState();
    for (let i = 0; i < OPERATION_RECORDS_MAX; i++) {
      const seat = reserveOperationOutcome(state);
      state = admitOperationOutcome(seat.state, seat.token, accept({ operationId: `op-${String(i)}`, corr: `op-${String(i)}`, deadlineAt: 1 }));
      state = expireOperationDeadlines(state, 1);
    }
    expect(state.records.size).toBe(256);
    expect(() => reserveOperationOutcome(state)).toThrow(/capacity/);
    expect(() => applyOperationAcceptance(state, accept({ operationId: 'late-202', corr: 'late-202' }))).toThrow(/capacity/);
    expect(readOperationOutcome(state, 'op-0')?.phase).toBe('unknown');
    state = applyTerminalEvidence(state, evidence({ corr: 'op-0', at: 2 }));
    expect(readOperationOutcome(state, 'op-0')?.phase).toBe('applied');
    state = reserveOperationOutcome(state).state;
    expect(state.records.size + state.provisional.size).toBe(256);
  });

  it('rejects 300 unresolved acceptances at the hard cap', () => {
    let state = createOperationOutcomeState();
    for (let i = 0; i < 300; i++) {
      if (i < OPERATION_RECORDS_MAX) state = reserveOperationOutcome(state).state;
      else expect(() => reserveOperationOutcome(state)).toThrow(/capacity/);
    }
    expect(state.provisional.size).toBe(OPERATION_RECORDS_MAX);
  });

  it('conflicting replay cannot mutate expectation or deadline', () => {
    const original = accept({ resource: 'out-1', event: 'output.status', kind: 'start', deadlineAt: 100 });
    const state = applyOperationAcceptance(createOperationOutcomeState(), original);
    for (const change of [{corr: 'other'}, {topic: 'other'}, {resource: 'out-2'},
      {event: 'other'}, {kind: 'stop'}]) {
      expect(() => applyOperationAcceptance(state, {...original, ...change, deadlineAt: 1})).toThrow(/identity/);
    }
    expect(readOperationOutcome(state, 'op-1')?.deadlineAt).toBe(100);
    expect(applyOperationAcceptance(state, {...original, deadlineAt: 1})).toBe(state);
  });

  it('first contradictory early terminal verdict wins for one corr', () => {
    let state = applyTerminalEvidence(createOperationOutcomeState(), evidence({verdict: 'failed', at: 1}));
    state = applyTerminalEvidence(state, evidence({verdict: 'applied', at: 2}));
    expect(state.earlyEvidence).toHaveLength(1);
    state = applyOperationAcceptance(state, accept());
    expect(readOperationOutcome(state, 'op-1')?.phase).toBe('failed');
    expect(() => applyOperationAcceptance(state, accept({operationId: 'other'}))).toThrow(/identity/);
  });

  it('rejects malformed ingress and non-finite clocks', () => {
    const state = createOperationOutcomeState();
    expect(() => admitOperationOutcome(reserveOperationOutcome(state).state, 1, accept({deadlineAt: Number.NaN}))).toThrow(/invalid/);
    expect(() => applyOperationAcceptance(state, accept({corr: '   '}))).toThrow(/invalid/);
    expect(applyTerminalEvidence(state, {...evidence(), verdict: 'other'} as unknown as TerminalEvidence)).toBe(state);
    expect(applyTerminalEvidence(state, {...evidence(), at: Infinity})).toBe(state);
    expect(() => expireOperationDeadlines(state, Number.NaN)).toThrow(/invalid/);
  });
});
