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

  it('mints a valid RFC 4122 v4 key from getRandomValues when randomUUID is unavailable (plain HTTP)', async () => {
    const getRandomValues = vi.fn((bytes: Uint8Array) => {
      bytes.fill(0xff);
      return bytes;
    });
    vi.stubGlobal('crypto', { getRandomValues });
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({ operation_id: 'op1', kind: 'start' }), { status: 202 }));
    vi.stubGlobal('fetch', fetchMock);
    await submitOperation('/api/v1/command');
    const key = new Headers(fetchMock.mock.calls[0]?.[1]?.headers).get('Idempotency-Key');
    expect(key).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(getRandomValues).toHaveBeenCalledTimes(1);
    expect(getRandomValues.mock.calls[0]?.[0]).toHaveLength(16);
  });

  it('preserves explicit idempotency keys', () => {
    expect(buildHeaders({ idempotencyKey: 'provided' }, false).get('Idempotency-Key')).toBe('provided');
  });
});

describe('submitOperation completion', () => {
  it.each(['take_salvo', 'media_play', 'replay'])('does not arm a generic completion for %s', async (kind) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(JSON.stringify({ operation_id: `op-${kind}`, kind }), { status: 202 })));
    const accepted = await submitOperation('/api/v1/x/take');
    expect(accepted.completion).toBeUndefined();
    expect(accepted.operation_id).toBe(`op-${kind}`);
  });
});
