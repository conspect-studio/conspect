import { describe, expect, it } from 'vitest';
import { EarlyEvidence, mergeEvidence, takeEvidenceOf } from './patchTakeEvidence';
import type { Envelope } from '../../realtime/envelope';

const env = (partial: Partial<Envelope> & Pick<Envelope, 't' | 'data'>): Envelope => ({ v: 1, seq: 1, ts: 1, topic: 'outputs', ...partial });

describe('takeEvidenceOf (E3-04)', () => {
  it('normalizes patch.take to the set path', () => {
    expect(takeEvidenceOf(env({ t: 'patch.take', data: { operation_id: 'op', outcome: 'refused', refusal: 'capacity', tick: 1, destination: { output: 'a' } } })))
      .toEqual({ operationId: 'op', path: 'set', outcome: 'refused', reason: 'capacity' });
  });
  it('normalizes route.output to the output path; refused:* is a refusal with its kind', () => {
    expect(takeEvidenceOf(env({ t: 'route.output', data: { output: 'a', operation_id: 'op', tick: 1, outcome: 'applied' } })))
      .toEqual({ operationId: 'op', path: 'output', outcome: 'applied' });
    expect(takeEvidenceOf(env({ t: 'route.output', data: { output: 'a', operation_id: 'op', tick: 1, outcome: 'refused:capacity' } })))
      .toEqual({ operationId: 'op', path: 'output', outcome: 'refused', reason: 'refused:capacity' });
  });
  it('normalizes a validated switcher.operation to the mixer path', () => {
    expect(takeEvidenceOf(env({ t: 'switcher.operation', topic: 'switcher', id: 'me1', corr: 'op', data: { me: 'me1', program: 'cam', tick: 3, operation_id: 'op', completed: true } })))
      .toEqual({ operationId: 'op', path: 'mixer', outcome: 'applied' });
  });
  it('ignores malformed and fallback (empty operation id) deliveries', () => {
    expect(takeEvidenceOf(env({ t: 'route.output', data: { output: 'a', operation_id: '', tick: 1, outcome: 'fallback:aux-failed' } }))).toBeUndefined();
    expect(takeEvidenceOf(env({ t: 'patch.take', data: { operation_id: 'op', outcome: 'maybe', destination: {} } }))).toBeUndefined();
    expect(takeEvidenceOf(env({ t: 'switcher.operation', topic: 'switcher', id: 'me1', corr: 'other', data: { me: 'me1', program: 'cam', tick: 3, operation_id: 'op', completed: true } }))).toBeUndefined();
  });
});

describe('mergeEvidence', () => {
  it('a set is refused once any destination refused', () => {
    const applied = { operationId: 'op', path: 'set' as const, outcome: 'applied' as const };
    const refused = { operationId: 'op', path: 'set' as const, outcome: 'refused' as const, reason: 'capacity' };
    expect(mergeEvidence(mergeEvidence(undefined, applied), refused)).toEqual(refused);
    expect(mergeEvidence(refused, applied)).toEqual(refused);
  });
});

describe('EarlyEvidence (E3-05)', () => {
  const ev = (operationId: string, path: 'set' | 'output' | 'mixer' = 'set') => ({ operationId, path, outcome: 'applied' as const });
  it('drains only the matching id on the matching path', () => {
    const buffer = new EarlyEvidence();
    buffer.push(ev('a'), 0);
    buffer.push(ev('b'), 0);
    buffer.push(ev('a', 'output'), 0);
    expect(buffer.drain('a', 'set', 1)).toEqual([ev('a')]);
    expect(buffer.size).toBe(2);
  });
  it('is bounded to the last N deliveries', () => {
    const buffer = new EarlyEvidence(2, 10_000);
    buffer.push(ev('a'), 0);
    buffer.push(ev('b'), 0);
    buffer.push(ev('c'), 0);
    expect(buffer.size).toBe(2);
    expect(buffer.drain('a', 'set', 0)).toEqual([]);
  });
  it('expires deliveries older than the TTL', () => {
    const buffer = new EarlyEvidence(64, 100);
    buffer.push(ev('a'), 0);
    expect(buffer.drain('a', 'set', 500)).toEqual([]);
  });
});
