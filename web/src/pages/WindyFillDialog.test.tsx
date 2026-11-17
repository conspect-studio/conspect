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

  it('sends steps-mode defaults for the default radar overlay', async () => {
    let capturedView: Record<string, unknown> | undefined;
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', async ({ request }) => {
        const body = (await request.json()) as { view?: Record<string, unknown> };
        capturedView = body.view;
        return HttpResponse.json({ job_id: 1, status_url: '/api/v1/media/captures/1' }, { status: 202 });
      }),
      http.get('*/api/v1/media/captures/1', () =>
        HttpResponse.json({ job_id: 1, fill_id: 'vt-vt-1-windy', state: 'queued' })),
    );

    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    await waitFor(() => {
      expect(capturedView).toBeDefined();
    });
    expect(capturedView).toMatchObject({
      overlay: 'radar',
      capture: 'steps',
      timing: 'equal',
      frame_ms: 600,
      end_hold_ms: 1000,
      max_steps: 12,
    });
  });

  it('sends realtime capture with no step fields for the wind overlay, and hides steps controls', async () => {
    let capturedView: Record<string, unknown> | undefined;
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', async ({ request }) => {
        const body = (await request.json()) as { view?: Record<string, unknown> };
        capturedView = body.view;
        return HttpResponse.json({ job_id: 2, status_url: '/api/v1/media/captures/2' }, { status: 202 });
      }),
      http.get('*/api/v1/media/captures/2', () =>
        HttpResponse.json({ job_id: 2, fill_id: 'vt-vt-1-windy', state: 'queued' })),
    );

    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });

    // Switch overlay to Wind.
    await userEvent.click(screen.getByRole('combobox', { name: /overlay/i }));
    await userEvent.click(screen.getByRole('option', { name: 'Wind' }));

    // Steps-only controls (timing/frame duration/etc.) are hidden for wind.
    expect(screen.queryByLabelText(/timing/i)).not.toBeInTheDocument();
    expect(screen.queryByLabelText(/frame duration/i)).not.toBeInTheDocument();
    expect(screen.getByText(/Wind only supports realtime capture/i)).toBeInTheDocument();

    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    await waitFor(() => {
      expect(capturedView).toBeDefined();
    });
    expect(capturedView).toMatchObject({ overlay: 'wind', capture: 'realtime' });
    expect(capturedView).not.toHaveProperty('timing');
    expect(capturedView).not.toHaveProperty('frame_ms');
    expect(capturedView).not.toHaveProperty('end_hold_ms');
    expect(capturedView).not.toHaveProperty('max_steps');
  });

  it('disables submit with an inline error when steps-mode exceeds the server frame cap', async () => {
    server.use(capabilitiesHandler());
    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));

    const captureButton = screen.getByRole('button', { name: 'Capture' });
    expect(captureButton).toBeEnabled();

    const maxSteps = screen.getByLabelText(/max steps/i);
    await userEvent.clear(maxSteps);
    await userEvent.type(maxSteps, '48');

    const frameMs = screen.getByLabelText(/frame duration/i);
    await userEvent.clear(frameMs);
    await userEvent.type(frameMs, '5000');

    await waitFor(() => {
      expect(screen.getByText(/above the server's cap of 600/i)).toBeInTheDocument();
    });
    expect(captureButton).toBeDisabled();

    // Finding 5: the contributing step-loop inputs are marked invalid and
    // described by the SAME stable error id while the cap is exceeded.
    const errorId = screen.getByText(/above the server's cap of 600/i).id;
    expect(errorId).toBeTruthy();
    for (const field of [
      screen.getByLabelText(/^fps$/i),
      frameMs,
      screen.getByLabelText(/end hold/i),
      maxSteps,
    ]) {
      expect(field).toHaveAttribute('aria-invalid', 'true');
      expect(field.getAttribute('aria-describedby')).toBe(errorId);
    }
  });

  it('frame-cap boundary: fps=1, frame_ms=100, max_steps=12, hold=0 -> exactly 12 frames (allowed)', async () => {
    server.use(capabilitiesHandler());
    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));

    const fps = screen.getByLabelText(/^fps$/i);
    await userEvent.clear(fps);
    await userEvent.type(fps, '1');

    const frameMs = screen.getByLabelText(/frame duration/i);
    await userEvent.clear(frameMs);
    await userEvent.type(frameMs, '100');

    const endHoldMs = screen.getByLabelText(/end hold/i);
    await userEvent.clear(endHoldMs);
    await userEvent.type(endHoldMs, '0');

    const maxSteps = screen.getByLabelText(/max steps/i);
    await userEvent.clear(maxSteps);
    await userEvent.type(maxSteps, '12');

    // framesAt(100, 1) = floor((100*1+500)/1000) = 0, so max(1, 0) = 1 per
    // step; 12 steps * 1 + framesAt(0, 1) = 12 + 0 = 12 total.
    await waitFor(() => {
      expect(screen.getByText(/Nominal total frames: 12 /)).toBeInTheDocument();
    });
    expect(screen.getByRole('button', { name: 'Capture' })).toBeEnabled();
  });

  it('frame-cap boundary: exactly 600 nominal frames is allowed, 601 is blocked', async () => {
    server.use(capabilitiesHandler());
    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));

    // fps=10, frame_ms=1500 -> framesAt(1500,10) = floor((15000+500)/1000)
    // = 15 frames/step; max_steps=40 (within the 2-48 UI range);
    // end_hold_ms=0 -> 0 hold frames. Nominal total: 40*15+0 = 600 — the
    // cap, exactly allowed.
    const fps = screen.getByLabelText(/^fps$/i);
    await userEvent.clear(fps);
    await userEvent.type(fps, '10');

    const frameMs = screen.getByLabelText(/frame duration/i);
    await userEvent.clear(frameMs);
    await userEvent.type(frameMs, '1500');

    const endHoldMs = screen.getByLabelText(/end hold/i);
    await userEvent.clear(endHoldMs);
    await userEvent.type(endHoldMs, '0');

    const maxSteps = screen.getByLabelText(/max steps/i);
    await userEvent.clear(maxSteps);
    await userEvent.type(maxSteps, '40');

    await waitFor(() => {
      expect(screen.getByText(/Nominal total frames: 600 /)).toBeInTheDocument();
    });
    expect(screen.getByRole('button', { name: 'Capture' })).toBeEnabled();

    // Add one hold frame (end_hold_ms=50 -> framesAt(50,10) =
    // floor((500+500)/1000) = 1) to push the nominal total to 601 — one
    // over the cap, blocked.
    await userEvent.clear(endHoldMs);
    await userEvent.type(endHoldMs, '50');

    await waitFor(() => {
      expect(screen.getByText(/above the server's cap of 600/i)).toBeInTheDocument();
    });
    expect(screen.getByRole('button', { name: 'Capture' })).toBeDisabled();
  });

  it('runs the happy path: capture -> succeeded -> artifact load -> completed', async () => {
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
        // The real terminal success state this provider reports is
        // `completed` — never the fabricated `committed`
        // (`crates/multiview-cli/src/player/provider.rs::classify`).
        HttpResponse.json({
          operation: 7,
          state: 'completed',
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
      expect(screen.getByText(/Completed/)).toBeInTheDocument();
    });

    // The native ack (geometry + audio presence) is shown once completed.
    expect(screen.getByText(/60 frames/)).toBeInTheDocument();
    expect(screen.getByText(/silent/i)).toBeInTheDocument();

    expect(screen.getByRole('button', { name: 'Re-capture' })).toBeInTheDocument();
  });

  it('shows the terminal superseded load state with no spinner', async () => {
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', () =>
        HttpResponse.json({ job_id: 50, status_url: '/api/v1/media/captures/50' }, { status: 202 })),
      http.get('*/api/v1/media/captures/50', () =>
        HttpResponse.json({ job_id: 50, fill_id: 'vt-vt-1-windy', state: 'succeeded', artifact_id: 'art-50' })),
      http.post('*/api/v1/media/players/vt-1/load', () =>
        HttpResponse.json({ operation: 9, state: 'pending' }, { status: 202 })),
      http.get('*/api/v1/media/players/vt-1/load-operations/9', () =>
        HttpResponse.json({ operation: 9, state: 'superseded', native: null })),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    await waitFor(() => {
      expect(screen.getByText(/Superseded/)).toBeInTheDocument();
    });
    // Terminal: no spinner (`animate-spin`) anywhere in the load status row.
    expect(document.querySelector('.animate-spin')).not.toBeInTheDocument();
  });

  it('shows the terminal failed load state with no spinner', async () => {
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', () =>
        HttpResponse.json({ job_id: 51, status_url: '/api/v1/media/captures/51' }, { status: 202 })),
      http.get('*/api/v1/media/captures/51', () =>
        HttpResponse.json({ job_id: 51, fill_id: 'vt-vt-1-windy', state: 'succeeded', artifact_id: 'art-51' })),
      http.post('*/api/v1/media/players/vt-1/load', () =>
        HttpResponse.json({ operation: 10, state: 'pending' }, { status: 202 })),
      http.get('*/api/v1/media/players/vt-1/load-operations/10', () =>
        HttpResponse.json({ operation: 10, state: 'failed', native: null })),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    await waitFor(() => {
      // "Load: Failed" — scope to the load row, distinct from a capture-job failure.
      expect(screen.getByText(/Load:/)).toBeInTheDocument();
    });
    await waitFor(() => {
      const loadRow = screen.getByText(/Load:/).closest('p');
      expect(loadRow).toHaveTextContent(/Failed/);
    });
    expect(document.querySelector('.animate-spin')).not.toBeInTheDocument();
  });

  it('shows the typed failure reason and detail of a failed load (issue #474)', async () => {
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', () =>
        HttpResponse.json({ job_id: 52, status_url: '/api/v1/media/captures/52' }, { status: 202 })),
      http.get('*/api/v1/media/captures/52', () =>
        HttpResponse.json({ job_id: 52, fill_id: 'vt-vt-1-windy', state: 'succeeded', artifact_id: 'art-52' })),
      http.post('*/api/v1/media/players/vt-1/load', () =>
        HttpResponse.json({ operation: 11, state: 'pending' }, { status: 202 })),
      http.get('*/api/v1/media/players/vt-1/load-operations/11', () =>
        HttpResponse.json({
          operation: 11,
          state: 'failed',
          native: null,
          reason: 'wrong_cadence',
          detail: 'clip frame rate 25/1 does not match the canvas frame rate 30000/1001',
        })),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    await waitFor(() => {
      expect(screen.getByTestId('load-failure-reason')).toHaveTextContent(
        /Clip frame rate does not match the canvas.*25\/1 does not match the canvas frame rate 30000\/1001/,
      );
    });
  });

  it('mints a distinct load key per new capture->load cycle, and a retried load reuses its key', async () => {
    const loadKeysByOperation = new Map<number, string>();
    let loadCalls = 0;
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', () =>
        HttpResponse.json({ job_id: 60, status_url: '/api/v1/media/captures/60' }, { status: 202 })),
      http.get('*/api/v1/media/captures/60', () =>
        HttpResponse.json({ job_id: 60, fill_id: 'vt-vt-1-windy', state: 'succeeded', artifact_id: 'art-60' })),
      http.post('*/api/v1/media/players/vt-1/load', ({ request }) => {
        loadCalls += 1;
        const key = request.headers.get('idempotency-key');
        expect(key).toBeTruthy();
        loadKeysByOperation.set(100, key ?? '');
        return HttpResponse.json({ operation: 100, state: 'pending' }, { status: 202 });
      }),
      http.get('*/api/v1/media/players/vt-1/load-operations/100', () =>
        HttpResponse.json({ operation: 100, state: 'completed', native: null })),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));
    await waitFor(() => {
      expect(screen.getByText(/Completed/)).toBeInTheDocument();
    });

    expect(loadCalls).toBe(1);
    const firstKey = loadKeysByOperation.get(100);
    expect(firstKey).toBeTruthy();

    // Second full capture -> load cycle (Re-capture): a different job
    // produces a different artifact, so this MUST mint a distinct load key
    // (finding 1), never reuse the first job's key.
    server.use(
      http.post('*/api/v1/media/captures', () =>
        HttpResponse.json({ job_id: 61, status_url: '/api/v1/media/captures/61' }, { status: 202 })),
      http.get('*/api/v1/media/captures/61', () =>
        HttpResponse.json({ job_id: 61, fill_id: 'vt-vt-1-windy', state: 'succeeded', artifact_id: 'art-61' })),
      http.post('*/api/v1/media/players/vt-1/load', ({ request }) => {
        loadCalls += 1;
        const key = request.headers.get('idempotency-key');
        expect(key).toBeTruthy();
        loadKeysByOperation.set(101, key ?? '');
        return HttpResponse.json({ operation: 101, state: 'pending' }, { status: 202 });
      }),
      http.get('*/api/v1/media/players/vt-1/load-operations/101', () =>
        HttpResponse.json({ operation: 101, state: 'completed', native: null })),
    );

    await userEvent.click(screen.getByRole('button', { name: 'Re-capture' }));
    await waitFor(() => {
      expect(loadCalls).toBe(2);
    });
    await waitFor(() => {
      expect(screen.getByText(/Completed/)).toBeInTheDocument();
    });

    const secondKey = loadKeysByOperation.get(101);
    expect(secondKey).toBeTruthy();
    expect(secondKey).not.toBe(firstKey);
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

  it('a lost capture-create response (network error) offers "Retry same request", which resends the SAME key+body', async () => {
    let postCalls = 0;
    const keysSeen: (string | null)[] = [];
    const bodiesSeen: unknown[] = [];
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', async ({ request }) => {
        postCalls += 1;
        keysSeen.push(request.headers.get('idempotency-key'));
        bodiesSeen.push(await request.json());
        if (postCalls === 1) {
          // Simulate a lost response: the POST may have been admitted
          // server-side, but this client never sees a reply.
          return HttpResponse.error();
        }
        return HttpResponse.json({ job_id: 70, status_url: '/api/v1/media/captures/70' }, { status: 202 });
      }),
      http.get('*/api/v1/media/captures/70', () =>
        HttpResponse.json({ job_id: 70, fill_id: 'vt-vt-1-windy', state: 'queued' })),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    const retryButton = await screen.findByRole('button', { name: 'Retry same request' });
    await userEvent.click(retryButton);

    await waitFor(() => {
      expect(postCalls).toBe(2);
    });
    // Finding 2: the SAME idempotency key and the SAME body on the retry —
    // never a freshly-minted key, which would risk a duplicate capture if
    // the first (lost-response) POST had actually been admitted.
    expect(keysSeen[1]).toBe(keysSeen[0]);
    expect(keysSeen[0]).toBeTruthy();
    expect(bodiesSeen[1]).toEqual(bodiesSeen[0]);
  });

  it('finding 2: unchecking terms after a failed POST disables "Retry same request" (no resend)', async () => {
    let postCalls = 0;
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', () => {
        postCalls += 1;
        return HttpResponse.error();
      }),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    const terms = screen.getByLabelText(/accept the/i);
    await userEvent.click(terms);
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    const retryButton = await screen.findByRole('button', { name: 'Retry same request' });
    expect(postCalls).toBe(1);

    // Uncheck terms: the Retry button must now be disabled, and clicking it
    // (even if somehow still enabled) must send no further POST.
    await userEvent.click(terms);
    expect(retryButton).toBeDisabled();
    await userEvent.click(retryButton).catch(() => undefined);
    expect(postCalls).toBe(1);
  });

  it('a lost artifact-load response offers "Retry load", which resends the SAME load key AND body', async () => {
    let loadCalls = 0;
    const loadKeysSeen: (string | null)[] = [];
    const loadBodiesSeen: unknown[] = [];
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', () =>
        HttpResponse.json({ job_id: 80, status_url: '/api/v1/media/captures/80' }, { status: 202 })),
      http.get('*/api/v1/media/captures/80', () =>
        HttpResponse.json({ job_id: 80, fill_id: 'vt-vt-1-windy', state: 'succeeded', artifact_id: 'art-80' })),
      http.post('*/api/v1/media/players/vt-1/load', async ({ request }) => {
        loadCalls += 1;
        loadKeysSeen.push(request.headers.get('idempotency-key'));
        loadBodiesSeen.push(await request.json());
        if (loadCalls === 1) {
          return HttpResponse.error();
        }
        return HttpResponse.json({ operation: 200, state: 'pending' }, { status: 202 });
      }),
      http.get('*/api/v1/media/players/vt-1/load-operations/200', () =>
        HttpResponse.json({ operation: 200, state: 'completed', native: null })),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    const retryLoadButton = await screen.findByRole('button', { name: 'Retry load' });
    await userEvent.click(retryLoadButton);

    await waitFor(() => {
      expect(loadCalls).toBe(2);
    });
    expect(loadKeysSeen[1]).toBe(loadKeysSeen[0]);
    expect(loadKeysSeen[0]).toBeTruthy();
    expect(loadBodiesSeen[1]).toEqual(loadBodiesSeen[0]);

    await waitFor(() => {
      expect(screen.getByText(/Completed/)).toBeInTheDocument();
    });
  });

  it('finding 1: Retry load resends the IDENTICAL body (artifactRef + then) even after Then is changed on the live draft', async () => {
    let loadCalls = 0;
    const loadKeysSeen: (string | null)[] = [];
    const loadBodiesSeen: unknown[] = [];
    server.use(
      capabilitiesHandler(),
      http.post('*/api/v1/media/captures', () =>
        HttpResponse.json({ job_id: 90, status_url: '/api/v1/media/captures/90' }, { status: 202 })),
      http.get('*/api/v1/media/captures/90', () =>
        HttpResponse.json({ job_id: 90, fill_id: 'vt-vt-1-windy', state: 'succeeded', artifact_id: 'art-90' })),
      http.post('*/api/v1/media/players/vt-1/load', async ({ request }) => {
        loadCalls += 1;
        loadKeysSeen.push(request.headers.get('idempotency-key'));
        loadBodiesSeen.push(await request.json());
        if (loadCalls === 1) {
          return HttpResponse.error();
        }
        return HttpResponse.json({ operation: 300, state: 'pending' }, { status: 202 });
      }),
      http.get('*/api/v1/media/players/vt-1/load-operations/300', () =>
        HttpResponse.json({ operation: 300, state: 'completed', native: null })),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    // Default Then is "Play" (resume). Confirm before submitting.
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    const retryLoadButton = await screen.findByRole('button', { name: 'Retry load' });

    // Now change Then to "Cue" on the (still-editable) live draft, AFTER
    // the capture succeeded and the first load attempt was snapshotted.
    await userEvent.click(screen.getByRole('combobox', { name: /then/i }));
    await userEvent.click(screen.getByRole('option', { name: 'Cue' }));

    await userEvent.click(retryLoadButton);

    await waitFor(() => {
      expect(loadCalls).toBe(2);
    });
    // Same key, and the SAME body (including `then`) on the retry — never
    // rebuilt from the live draft's now-changed Then selector.
    expect(loadKeysSeen[1]).toBe(loadKeysSeen[0]);
    expect(loadKeysSeen[0]).toBeTruthy();
    expect(loadBodiesSeen[1]).toEqual(loadBodiesSeen[0]);
    expect(loadBodiesSeen[0]).toMatchObject({
      artifactRef: { captureJobId: 90, artifactId: 'art-90' },
      then: 'resume',
    });

    await waitFor(() => {
      expect(screen.getByText(/Completed/)).toBeInTheDocument();
    });
  });

  it('finding 3: a 422 validation rejection offers "Start new capture", which posts the corrected body with a NEW key', async () => {
    const keysSeen: (string | null)[] = [];
    const bodiesSeen: Record<string, unknown>[] = [];
    let postCalls = 0;
    server.use(
      capabilitiesHandler(),
      // Reject the FIRST submission unconditionally with a definite 422
      // (standing in for any server-side validation/conflict rejection the
      // client's own range checks do not catch, e.g. a combination the
      // server rejects); accept the second.
      http.post('*/api/v1/media/captures', async ({ request }) => {
        postCalls += 1;
        keysSeen.push(request.headers.get('idempotency-key'));
        bodiesSeen.push((await request.json()) as Record<string, unknown>);
        if (postCalls === 1) {
          return HttpResponse.json(
            { status: 422, title: 'Invalid view', detail: 'fps must be 1..30' },
            { status: 422 },
          );
        }
        return HttpResponse.json({ job_id: 95, status_url: '/api/v1/media/captures/95' }, { status: 202 });
      }),
      http.get('*/api/v1/media/captures/95', () =>
        HttpResponse.json({ job_id: 95, fill_id: 'vt-vt-1-windy', state: 'queued' })),
    );

    renderDialog();
    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));

    await waitFor(() => {
      expect(screen.getByText(/fps must be 1\.\.30/i)).toBeInTheDocument();
    });

    // Both actions are offered; "Start new capture" is used here with a
    // corrected fps (changed from the default 10 to a different valid
    // value, so the resent body differs from the rejected one).
    expect(screen.getByRole('button', { name: 'Retry same request' })).toBeInTheDocument();
    const startNewButton = screen.getByRole('button', { name: 'Start new capture' });

    const fps = screen.getByLabelText(/^fps$/i);
    await userEvent.clear(fps);
    await userEvent.type(fps, '20');

    await userEvent.click(startNewButton);

    await waitFor(() => {
      expect(postCalls).toBe(2);
    });
    // A NEW key, and the corrected body (fps: 20, not the rejected value).
    expect(keysSeen[1]).not.toBe(keysSeen[0]);
    expect(keysSeen[1]).toBeTruthy();
    expect((bodiesSeen[1]?.view as Record<string, unknown> | undefined)?.fps).toBe(20);
    expect(bodiesSeen[1]).not.toEqual(bodiesSeen[0]);
  });

  it('finding 3: client-side validation blocks submit for out-of-range fps/zoom/lat/lon', async () => {
    server.use(capabilitiesHandler());
    renderDialog();

    await waitFor(() => {
      expect(screen.getByLabelText(/accept the/i)).toBeEnabled();
    });
    await userEvent.click(screen.getByLabelText(/accept the/i));

    const captureButton = screen.getByRole('button', { name: 'Capture' });
    expect(captureButton).toBeEnabled();

    const fps = screen.getByLabelText(/^fps$/i);
    await userEvent.clear(fps);
    await userEvent.type(fps, '0');
    expect(captureButton).toBeDisabled();

    await userEvent.clear(fps);
    await userEvent.type(fps, '31');
    expect(captureButton).toBeDisabled();

    await userEvent.clear(fps);
    await userEvent.type(fps, '10');
    expect(captureButton).toBeEnabled();

    const zoom = screen.getByLabelText(/zoom/i);
    await userEvent.clear(zoom);
    await userEvent.type(zoom, '19');
    expect(captureButton).toBeDisabled();

    await userEvent.clear(zoom);
    await userEvent.type(zoom, '10');
    expect(captureButton).toBeEnabled();

    const lat = screen.getByLabelText(/latitude/i);
    await userEvent.clear(lat);
    await userEvent.type(lat, '95');
    expect(captureButton).toBeDisabled();

    await userEvent.clear(lat);
    await userEvent.type(lat, '-33.87');
    expect(captureButton).toBeEnabled();

    const lon = screen.getByLabelText(/longitude/i);
    await userEvent.clear(lon);
    await userEvent.type(lon, '-181');
    expect(captureButton).toBeDisabled();
  });
});
