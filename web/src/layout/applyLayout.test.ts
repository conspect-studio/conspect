// Tests for the apply-layout command call: POST /api/v1/commands/apply-layout
// with body `{ layout }` (crates/multiview-control/src/routes/mod.rs
// `ApplyLayoutRequest`), returning `202 Accepted` + `{ operation_id, kind }`.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { applyLayoutCommand, describeApplyError } from './applyLayout';

let lastBody: unknown;
let lastAuth: string | null;

const server = setupServer(
  http.post('*/api/v1/commands/apply-layout', async ({ request }) => {
    lastBody = await request.json();
    lastAuth = request.headers.get('authorization');
    return HttpResponse.json(
      { operation_id: 'op-123', kind: 'apply_layout' },
      { status: 202 },
    );
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  lastBody = undefined;
  lastAuth = null;
});
afterAll(() => {
  server.close();
});

describe('applyLayoutCommand', () => {
  it('POSTs { layout } and returns the accepted operation id', async () => {
    const accepted = await applyLayoutCommand('wall-1', { token: 'tok-1' });
    expect(lastBody).toEqual({ layout: 'wall-1' });
    expect(lastAuth).toBe('Bearer tok-1');
    expect(accepted.operation_id).toBe('op-123');
    expect(accepted.kind).toBe('apply_layout');
  });

  it('surfaces an RFC 9457 problem on rejection', async () => {
    server.use(
      http.post('*/api/v1/commands/apply-layout', () =>
        HttpResponse.json(
          { title: 'Engine command bus at capacity', status: 503 },
          { status: 503 },
        ),
      ),
    );
    await expect(applyLayoutCommand('wall-1', { token: 'tok-1' })).rejects.toMatchObject({
      message: 'Engine command bus at capacity',
      status: 503,
    });
  });

  it('carries the 422 problem detail (the honest pre-202 refusal, ADR-W019)', async () => {
    server.use(
      http.post('*/api/v1/commands/apply-layout', () =>
        HttpResponse.json(
          {
            title: 'Request validation failed',
            status: 422,
            detail: 'layout "wall-9" does not exist in the layouts library',
          },
          { status: 422 },
        ),
      ),
    );
    const failure = await applyLayoutCommand('wall-9', { token: 'tok-1' }).then(
      () => undefined,
      (error: unknown) => error,
    );
    expect(failure).toMatchObject({ status: 422 });
    expect(describeApplyError(failure)).toBe(
      'layout "wall-9" does not exist in the layouts library',
    );
  });
});

describe('describeApplyError', () => {
  it('prefers the problem detail, falls back to the message, then String()', () => {
    expect(describeApplyError(new Error('plain failure'))).toBe('plain failure');
    expect(describeApplyError('odd rejection')).toBe('odd rejection');
  });
});
