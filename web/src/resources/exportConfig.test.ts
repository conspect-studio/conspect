// Tests for the config-as-code export call: GET /api/v1/config/export returns
// the composed MultiviewConfig as TOML (ADR-W015 §3). A 404/501 means the
// backend does not (yet) serve the route — surfaced as a typed "unsupported"
// error so the UI can explain instead of failing opaquely.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { ConfigExportUnsupportedError, fetchConfigExport } from './exportConfig';

const TOML = '[canvas]\nwidth = 1920\nheight = 1080\nfps = "30/1"\n';

let lastAuth: string | null;

const server = setupServer(
  http.get('*/api/v1/config/export', ({ request }) => {
    lastAuth = request.headers.get('authorization');
    // The backend sends Content-Disposition (attachment + filename); the client
    // does not parse it - the download must keep working regardless.
    return new HttpResponse(TOML, {
      status: 200,
      headers: {
        'Content-Type': 'application/toml',
        'Content-Disposition': 'attachment; filename="multiview.toml"',
      },
    });
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  lastAuth = null;
});
afterAll(() => {
  server.close();
});

describe('fetchConfigExport', () => {
  it('GETs the TOML document with the bearer token', async () => {
    const result = await fetchConfigExport({ token: 'tok-9' });
    expect(result.toml).toBe(TOML);
    expect(result.filename).toBe('multiview.toml');
    expect(lastAuth).toBe('Bearer tok-9');
  });

  it('raises the typed unsupported error on 404 (route not deployed)', async () => {
    server.use(
      http.get('*/api/v1/config/export', () => new HttpResponse(null, { status: 404 })),
    );
    await expect(fetchConfigExport({ token: 't' })).rejects.toBeInstanceOf(
      ConfigExportUnsupportedError,
    );
  });

  it('raises the typed unsupported error on 501 (not implemented)', async () => {
    server.use(
      http.get('*/api/v1/config/export', () => new HttpResponse(null, { status: 501 })),
    );
    await expect(fetchConfigExport({ token: 't' })).rejects.toBeInstanceOf(
      ConfigExportUnsupportedError,
    );
  });

  it('surfaces other failures as plain errors with the status', async () => {
    server.use(
      http.get('*/api/v1/config/export', () => new HttpResponse(null, { status: 403 })),
    );
    await expect(fetchConfigExport({ token: 't' })).rejects.toMatchObject({ status: 403 });
  });
});
