import { describe, expect, it } from 'vitest';
import { correlateOperation, resolveOperationEnvelope } from './operations';
import type { Envelope } from './envelope';

const frame: Envelope = { v: 1, t: 'media.player_state', topic: 'switcher', id: 'player-1', seq: 1, ts: 1, corr: 'op-1', data: { state: { kind: 'loading' } } };

describe('legacy correlation compatibility', () => {
  it('cannot mark a loading or arbitrary corr frame completed', async () => {
    const pending = correlateOperation({ operation_id: 'op-1', kind: 'media_play' });
    expect(resolveOperationEnvelope(frame)).toBe(false);
    await expect(pending).rejects.toThrow('Command-specific terminal expectation required');
  });
  it('does not infer the original command from a replay acceptance', async () => {
    await expect(correlateOperation({ operation_id: 'op-1', kind: 'replay' })).rejects.toThrow('outcome unknown');
  });
});
