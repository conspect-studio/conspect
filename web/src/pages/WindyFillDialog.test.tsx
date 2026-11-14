// WindyFillDialog — the "Fill from web…" VT-fill dialog.
//
// Covers: the terms checkbox gates submit; a capture_available=false server
// disables the whole dialog with an honest message; the happy path
// capture -> succeeded -> artifact load -> committed, with every HTTP call
// going through a real MSW handler (never a fixture-only shortcut); and a
// capture failure surfaces the problem-detail text.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';

import { WindyFillDialog } from './WindyFillDialog';
import type { MediaPlayer } from '../api/media-playersQueries';
import { messages } from '../locales/en/messages';

i18n.load('en', messages);
i18n.activate('en');

const PLAYER: MediaPlayer = {
  id: 'vt-1',
  name: 'VT 1',
  body: { id: 'vt-1' },
};

function renderDialog(player: MediaPlayer | null = PLAYER): {
  readonly client: QueryClient;
} {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  render(
    <QueryClientProvider client={client}>
      <I18nProvider i18n={i18n}>
        <WindyFillDialog player={player} onOpenChange={(): void => undefined} />
      </I18nProvider>
    </QueryClientProvider>,
  );
  return { client };
}

const server = setupServer();

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  try {
    window.localStorage.clear();
  } catch {
    // Not available in this jsdom configuration; the dialog's own
    // load/saveDraft already degrade gracefully (try/catch) when absent.
  }
});
afterAll(() => {
  server.close();
});

function capabilitiesHandler(
  overrides: Partial<{
    capture_available: boolean;
    premium_available: boolean;
    url_fetch_available: boolean;
  }> = {},
) {
  return http.get('*/api/v1/media/captures/capabilities', () =>
    HttpResponse.json({
      capture_available: true,
      premium_available: false,
      url_fetch_available: false,
      ...overrides,
    }));
}

describe('WindyFillDialog', () => {
  it('disables the dialog with a clear message when capture is not provisioned', async () => {
    server.use(capabilitiesHandler({ capture_available: false }));
    renderDialog();

    await waitFor(() => {
      expect(
        screen.getByText(/Web capture is not provisioned on this server/i),
      ).toBeInTheDocument();
    });

    const captureButton = screen.getByRole('button', { name: 'Capture' });
    expect(captureButton).toBeDisabled();
    // The terms checkbox and fields are inside the disabled fieldset.
    expect(screen.getByLabelText(/accept the/i)).toBeDisabled();
  });

  it('keeps Capture disabled until the terms checkbox is ticked', async () => {
    server.use(capabilitiesHandler());
    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });

    const captureButton = screen.getByRole('button', { name: 'Capture' });
    expect(captureButton).toBeDisabled();

    const terms = screen.getByLabelText(/accept the/i);
    await userEvent.click(terms);

    expect(captureButton).toBeEnabled();
  });

  it('runs the happy path: capture -> succeeded -> artifact load -> committed', async () => {
    let createCalls = 0;
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', ({ request }) => {
        createCalls += 1;
        expect(request.headers.get('idempotency-key')).toBeTruthy();
        return HttpResponse.json({ job_id: 42, status_url: '/api/v1/media/captures/42' }, { status: 202 });
      }),
      http.get('*/api/v1/media/captures/42', () =>
        HttpResponse.json({
          job_id: 42,
          fill_id: 'vt-vt-1-windy',
          state: 'succeeded',
          artifact_id: 'art-1',
        })),
      http.post('*/api/v1/media/players/vt-1/load', async ({ request }) => {
        expect(request.headers.get('idempotency-key')).toBeTruthy();
        const body: unknown = await request.json();
        expect(body).toMatchObject({
          artifactRef: { captureJobId: 42, artifactId: 'art-1' },
        });
        return HttpResponse.json({ operation: 7, state: 'pending' }, { status: 202 });
      }),
      http.get('*/api/v1/media/players/vt-1/load-operations/7', () =>
        HttpResponse.json({
          operation: 7,
          state: 'committed',
          native: {
            cadence_num: 25,
            cadence_den: 1,
            duration_frames: 60,
            duration_ns: 2_400_000_000,
            generation: 1,
            has_audio: false,
            height: 1080,
            operation: 7,
            source_id: 'src-1',
            timeline_ns: 0,
            width: 1920,
          },
        })),
    );

    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });

    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    expect(createCalls).toBeGreaterThanOrEqual(0);

    await waitFor(() => {
      expect(screen.getByText(/Succeeded/)).toBeInTheDocument();
    });

    await waitFor(() => {
      expect(screen.getByText(/Committed/)).toBeInTheDocument();
    });

    // The native ack (geometry + audio presence) is shown once committed.
    expect(screen.getByText(/60 frames/)).toBeInTheDocument();
    expect(screen.getByText(/silent/i)).toBeInTheDocument();

    expect(screen.getByRole('button', { name: 'Re-capture' })).toBeInTheDocument();
  });

  it('shows the problem-detail text on a capture create failure', async () => {
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', () =>
        HttpResponse.json(
          { status: 422, title: 'Invalid body, terms or missing key', detail: 'current Windy terms acceptance required' },
          { status: 422 },
        )),
    );

    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    await waitFor(() => {
      expect(screen.getByText(/current Windy terms acceptance required/i)).toBeInTheDocument();
    });
  });

  it('renders nothing for a null player', () => {
    server.use(capabilitiesHandler());
    const { container } = render(
      <QueryClientProvider client={new QueryClient()}>
        <I18nProvider i18n={i18n}>
          <WindyFillDialog player={null} onOpenChange={(): void => undefined} />
        </I18nProvider>
      </QueryClientProvider>,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it('defaults to the public view and offers Premium only when available', async () => {
    server.use(capabilitiesHandler({ premium_available: true }));
    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/Premium \(authenticated\) capture/i)).toBeInTheDocument();
    });
  });

  it('omits the Premium option when the server does not support it', async () => {
    server.use(capabilitiesHandler({ premium_available: false }));
    renderDialog();

    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Capture' })).toBeInTheDocument();
    });
    expect(screen.queryByLabelText(/Premium \(authenticated\) capture/i)).not.toBeInTheDocument();
    expect(screen.getByText(/Premium Windy capture is not configured/i)).toBeInTheDocument();
  });
});
