import { describe, expect, it, vi } from 'vitest';

import { guardFetch, isSpaFallback } from './spaFallbackGuard';
import { isUnavailableStatus, readPendingProblem, BackendUnavailableError } from './operations';

const html = (): Response => new Response('<!doctype html>', { status: 200, headers: { 'content-type': 'text/html' } });

describe('spaFallbackGuard', () => {
  it('maps an HTML 200 under /api/ to a 501 that becomes BackendUnavailableError', async () => {
    const guarded = guardFetch(vi.fn(() => Promise.resolve(html())));
    const res = await guarded('/api/v1/switcher/me1');
    expect(res.status).toBe(501);
    expect(isUnavailableStatus(res.status)).toBe(true);
    expect(await readPendingProblem(res, 'the switcher')).toBeInstanceOf(BackendUnavailableError);
  });
  it('passes JSON, images and non-API html through untouched', async () => {
    const json = new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } });
    expect((await guardFetch(() => Promise.resolve(json))('/api/v1/x')).status).toBe(200);
    expect((await guardFetch(() => Promise.resolve(html()))('/index.html')).status).toBe(200);
    expect(isSpaFallback(new Response(null, { status: 204 }))).toBe(false);
    expect(isSpaFallback(new Response('x', { status: 503, headers: { 'content-type': 'text/html' } }))).toBe(false);
  });
});
