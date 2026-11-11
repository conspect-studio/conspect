import { describe, expect, it } from 'vitest';
import { switcherOperationEvidence } from './operationEvidence';
import type { Envelope } from './envelope';

const frame = (overrides: Partial<Envelope> = {}): Envelope => ({
  v: 1, t: 'switcher.operation', topic: 'switcher', id: 'me1', corr: 'op-1', seq: 1, ts: 1,
  data: { me: 'me1', program: 'main', operation_id: 'op-1', tick: 4, completed: true },
  ...overrides,
});

describe('switcher output-tick terminal evidence', () => {
  it('recognises exact successful operation identity', () => {
    expect(switcherOperationEvidence(frame())).toEqual({ corr: 'op-1', topic: 'switcher', resource: 'me1', event: 'switcher.operation', me: 'me1', verdict: 'applied' });
  });
  it('returns failure with the real engine reason', () => {
    expect(switcherOperationEvidence(frame({ data: { me: 'me1', program: 'main', operation_id: 'op-1', tick: 5, completed: false, error: 'missing source' } }))).toEqual({ corr: 'op-1', topic: 'switcher', resource: 'me1', event: 'switcher.operation', me: 'me1', verdict: 'failed', reason: 'missing source' });
  });
  it.each([
    { t: 'media.player_state' }, { topic: 'outputs' }, { id: 'me2' }, { corr: 'wrong' },
    { data: { me: 'me2', program: 'main', operation_id: 'op-1', tick: 4, completed: true } },
    { data: { me: 'me1', program: 'main', operation_id: 'wrong', tick: 4, completed: true } },
    { data: { me: 'me1', program: 'main', operation_id: 'op-1', tick: 4, completed: 'true' } },
    { data: { me: 'me1', program: 'main', operation_id: 'op-1', tick: 4 } },
  ])('does not turn mismatched or malformed frames into terminal evidence: %j', (override) => {
    expect(switcherOperationEvidence(frame(override))).toBeUndefined();
  });
});
