// Unit tests for the pure WHEP signaling layer (ADR-W023 §1). No DOM: a mocked
// `fetch` drives the protocol — the 201 + Location happy path, relative-Location
// resolution against the post-redirect URL, the 503 fallback hint, and the
// best-effort keepalive DELETE.
import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  deleteWhepSession,
  postWhepOffer,
  resolveSessionUrl,
  WhepSignalingError,
} from './whepSession';
import { setStoredToken, clearStoredToken } from '../api/token';

afterEach(() => {
  clearStoredToken();
  vi.restoreAllMocks();
});

/** Build a minimal Response-like object for the mocked fetch. */
function sdpResponse(opts: {
  status: number;
  url: string;
  location?: string | undefined;
  body?: string | undefined;
  json?: unknown;
}): Response {
  const headers = new Headers();
  if (opts.location !== undefined) {
    headers.set('Location', opts.location);
  }
  const resp = {
    ok: opts.status >= 200 && opts.status < 300,
    status: opts.status,
    url: opts.url,
    headers,
    text: (): Promise<string> => Promise.resolve(opts.body ?? ''),
    clone(): Response {
      const cloned = {
        json: (): Promise<unknown> =>
          opts.json === undefined
            ? Promise.reject(new Error('no json body'))
            : Promise.resolve(opts.json),
      };
      return cloned as unknown as Response;
    },
  };
  return resp as unknown as Response;
}

describe('resolveSessionUrl', () => {
  it('resolves a relative Location against the post-redirect response URL', () => {
    expect(
      resolveSessionUrl('session/42', 'https://host.example/api/v1/whep/out1'),
    ).toBe('https://host.example/api/v1/whep/session/42');
  });

  it('keeps an absolute Location as-is', () => {
    expect(
      resolveSessionUrl('https://other.example/s/9', 'https://host.example/api/v1/whep/out1'),
    ).toBe('https://other.example/s/9');
  });
});

describe('postWhepOffer', () => {
  it('posts application/sdp with the bearer and returns answer + resolved session url', async () => {
    setStoredToken('tok-123');
    const fetchImpl = vi.fn<typeof fetch>().mockResolvedValue(
      sdpResponse({
        status: 201,
        url: 'https://host.example/api/v1/whep/out1',
        location: 'sessions/abc',
        body: 'v=0\r\na=answer',
      }),
    );
    const result = await postWhepOffer(
      'https://host.example/api/v1/whep/out1',
      'v=0\r\na=offer',
      fetchImpl,
    );
    expect(result.answerSdp).toBe('v=0\r\na=answer');
    expect(result.sessionUrl).toBe('https://host.example/api/v1/whep/sessions/abc');
    // The call carried application/sdp + the stored bearer.
    const init = fetchImpl.mock.calls[0]?.[1];
    const headers = init?.headers as Record<string, string>;
    expect(headers['Content-Type']).toBe('application/sdp');
    expect(headers.Authorization).toBe('Bearer tok-123');
    expect(init?.body).toBe('v=0\r\na=offer');
  });

  it('throws with the status on a non-2xx (no fallback hint)', async () => {
    const fetchImpl = vi
      .fn<typeof fetch>()
      .mockResolvedValue(sdpResponse({ status: 415, url: 'https://h/whep/x' }));
    await expect(postWhepOffer('https://h/whep/x', 'offer', fetchImpl)).rejects.toMatchObject({
      status: 415,
      fallbackHinted: false,
    });
  });

  it('flags the fallback hint when a 503 body carries fallback: "jpeg"', async () => {
    const fetchImpl = vi.fn<typeof fetch>().mockResolvedValue(
      sdpResponse({
        status: 503,
        url: 'https://h/whep/x',
        json: { status: 503, title: 'capacity', fallback: 'jpeg' },
      }),
    );
    const error = await postWhepOffer('https://h/whep/x', 'offer', fetchImpl).catch(
      (e: unknown) => e,
    );
    expect(error).toBeInstanceOf(WhepSignalingError);
    expect((error as WhepSignalingError).fallbackHinted).toBe(true);
  });

  it('rejects a 201 with no Location as a protocol error', async () => {
    const fetchImpl = vi
      .fn<typeof fetch>()
      .mockResolvedValue(sdpResponse({ status: 201, url: 'https://h/whep/x', body: 'v=0' }));
    await expect(
      postWhepOffer('https://h/whep/x', 'offer', fetchImpl),
    ).rejects.toBeInstanceOf(WhepSignalingError);
  });
});

describe('deleteWhepSession', () => {
  it('DELETEs the session url with keepalive + the bearer, best-effort', () => {
    setStoredToken('tok-xyz');
    const fetchImpl = vi
      .fn<typeof fetch>()
      .mockResolvedValue(sdpResponse({ status: 204, url: '' }));
    deleteWhepSession('https://h/whep/sessions/abc', fetchImpl);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const call = fetchImpl.mock.calls[0];
    expect(call?.[0]).toBe('https://h/whep/sessions/abc');
    const init = call?.[1];
    expect(init?.method).toBe('DELETE');
    expect(init?.keepalive).toBe(true);
    expect((init?.headers as Record<string, string>).Authorization).toBe('Bearer tok-xyz');
  });

  it('never throws when the delete rejects', () => {
    const fetchImpl = vi
      .fn<typeof fetch>()
      .mockRejectedValue(new Error('network down'));
    expect(() => {
      deleteWhepSession('https://h/whep/sessions/abc', fetchImpl);
    }).not.toThrow();
  });
});
