import { describe, expect, it, vi } from 'vitest';
import { buildHeaders, submitOperation } from './operations';

describe('submitOperation', () => {
  it('adds a fresh UUID idempotency key when none is supplied', async () => {
    vi.stubGlobal('crypto', { randomUUID: vi.fn().mockReturnValue('uuid-1') });
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({ operation_id: 'op1', kind: 'start' }), { status: 202 }));
    vi.stubGlobal('fetch', fetchMock);
    await submitOperation('/api/v1/command');
    expect(fetchMock.mock.calls[0]?.[1]?.headers).toBeDefined();
    expect(new Headers(fetchMock.mock.calls[0]?.[1]?.headers).get('Idempotency-Key')).toBe('uuid-1');
  });

  it('preserves explicit idempotency keys', () => {
    expect(buildHeaders({ idempotencyKey: 'provided' }, false).get('Idempotency-Key')).toBe('provided');
  });
});
