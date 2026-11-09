import { afterEach, describe, expect, it, vi } from 'vitest';
import { commandSwitcher } from './switcher';

const request = { me: 'me1', program: 'main', action: 'cut' } as const;
const response = () => new Response(JSON.stringify({ operation_id: 'op-switch', kind: 'cut' }), { status: 202 });

afterEach(() => vi.unstubAllGlobals());

describe('commandSwitcher', () => {
  it('preserves the explicit idempotency key and returns acceptance only', async () => {
    const fetchMock = vi.fn().mockResolvedValue(response());
    vi.stubGlobal('fetch', fetchMock);
    const result = await commandSwitcher('cut', request, { idempotencyKey: 'same-key' });
    expect(result).toEqual({ operation_id: 'op-switch', kind: 'cut' });
    expect(new Headers(fetchMock.mock.calls[0]?.[1]?.headers).get('Idempotency-Key')).toBe('same-key');
    expect(fetchMock.mock.calls[0]?.[0]).toBe('/api/v1/switcher/commands/cut');
  });
  it('mints a key when called without one', async () => {
    const fetchMock = vi.fn().mockResolvedValue(response());
    vi.stubGlobal('fetch', fetchMock);
    vi.stubGlobal('crypto', { randomUUID: () => 'new-key' });
    await commandSwitcher('cut', request);
    expect(new Headers(fetchMock.mock.calls[0]?.[1]?.headers).get('Idempotency-Key')).toBe('new-key');
  });
});
