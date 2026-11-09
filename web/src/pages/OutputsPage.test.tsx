// OutputsPage — the broadcast-console adoption contract:
//   * every configured output renders as a responsive card (TaskWorkspace)
//     whose actions keep the 48px touch floor and whose secondary honesty
//     notes sit behind the caller-controlled Details disclosure;
//   * the card shows only CONFIGURED facts (requested destination/codec),
//     never a live/health claim: a connected push link is "peer attached",
//     not "publishing decoded media", the cumulative `written` counter is
//     never surfaced as health, and the freshness-absent honesty line stays;
//   * the current-error vs previous-session-history distinction survives the
//     card adoption, as does the exact CRUD flow (GET prefill + `If-Match`
//     delete, inline per-field validation, create POST body).
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { OutputsPage } from './OutputsPage';
import { renderWithProviders } from '../test/render';

const OUTPUTS = [
  {
    id: 'program',
    name: 'Program HLS',
    body: { kind: 'hls', path: '/srv/index.m3u8', codec: 'h264' },
  },
  {
    id: 'out-rtsp',
    name: 'Studio RTSP',
    body: { kind: 'rtsp_server', mount: '/multiview', codec: 'h264' },
  },
  {
    id: 'srt-main',
    name: 'Contribution SRT',
    body: { kind: 'srt', url: 'srt://[2001:db8::9]:7001', codec: 'h264' },
  },
  // A kind this UI has no form for: rendered + deletable, but NOT editable —
  // the authored document must never be folded onto a different kind.
  {
    id: 'legacy-thing',
    name: 'Mystery sink',
    body: { kind: 'aes67', url: 'aes67://x' },
  },
];

// The live link body for the SRT card: CONNECTED with a real slow-peer drop,
// a current error AND a previous-session error, plus a large cumulative
// `written` counter that must never be read as media health.
const SRT_LINK = {
  state: 'connected',
  mode: 'caller',
  transport: 'srt',
  url: 'srt://[2001:db8::9]:7001',
  clients: 1,
  sessions: 1,
  written: 987_654,
  dropped: 61,
  peers: ['[2001:db8::7]:5000'],
  last_error: 'connection lost',
  last_session_error: 'Input/output error',
  discarded_no_client: 0,
  discarded_awaiting_keyframe: 0,
  dropped_slow_peer: 61,
  dropped_upstream: 0,
};

const ROUTING = {
  configured: true,
  routing: { sample_rate_hz: 48000, routes: [] },
  selectable_tracks: ['prog', 'cam1-clean'],
};

let deleted: { id: string; ifMatch: string | null } | undefined;

const server = setupServer(
  http.get('*/api/v1/outputs', () => HttpResponse.json(OUTPUTS)),
  http.get('*/api/v1/outputs/:id', ({ params }) => {
    const found = OUTPUTS.find((o) => o.id === String(params.id));
    return found
      ? HttpResponse.json(found, { headers: { ETag: '"1"' } })
      : new HttpResponse(null, { status: 404 });
  }),
  http.get('*/api/v1/outputs/:id/link', () => HttpResponse.json(SRT_LINK)),
  http.get('*/api/v1/outputs/:id/playback', () =>
    HttpResponse.json({ urls: [] }),
  ),
  http.get('*/api/v1/devices', () => HttpResponse.json([])),
  http.get('*/api/v1/audio-routing', () =>
    HttpResponse.json(ROUTING, { headers: { ETag: 'W/"3"' } }),
  ),
  http.delete('*/api/v1/outputs/:id', ({ request, params }) => {
    deleted = {
      id: String(params.id),
      ifMatch: request.headers.get('if-match'),
    };
    return new HttpResponse(null, { status: 204 });
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  deleted = undefined;
});
afterAll(() => {
  server.close();
});

function renderOutputs(): void {
  renderWithProviders(
    <MemoryRouter>
      <OutputsPage />
    </MemoryRouter>,
  );
}

describe('OutputsPage broadcast cards (configured facts)', () => {
  it('renders one responsive card per output with its configured facts', async () => {
    renderOutputs();
    for (const id of ['program', 'out-rtsp', 'srt-main', 'legacy-thing']) {
      expect(await screen.findByTestId(`output-card-${id}`)).toBeInTheDocument();
    }
    // The card title carries the operator name, the status line the raw wire
    // kind, the configured destination and the requested codec.
    expect(screen.getByText('Program HLS')).toBeInTheDocument();
    expect(screen.getByText('Contribution SRT')).toBeInTheDocument();
    expect(screen.getByText('hls')).toBeInTheDocument();
    expect(screen.getByText('rtsp_server')).toBeInTheDocument();
    expect(screen.getByText('aes67')).toBeInTheDocument();
    expect(screen.getByText('/srv/index.m3u8')).toBeInTheDocument();
    expect(screen.getByText('srt://[2001:db8::9]:7001')).toBeInTheDocument();
    expect(screen.getAllByText('h264').length).toBeGreaterThanOrEqual(3);
    // Runnability honesty survives the card adoption unchanged.
    expect(screen.getAllByText('Runnable').length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText('Not yet runnable in this build').length).toBeGreaterThanOrEqual(1);
    // The unknown-kind card shows the honest dash, never a folded runnability
    // claim for a kind this build does not know.
    const legacy = screen.getByTestId('output-card-legacy-thing');
    expect(
      within(legacy).queryByTestId('output-runnability'),
    ).not.toBeInTheDocument();
  });

  it('keeps a long output identifier breakable inside the phone card', async () => {
    server.use(http.get('*/api/v1/outputs', () => HttpResponse.json([
      { id: 'contribution-main-program-hls-primary-very-long-output-identifier', name: 'Long output', body: { kind: 'hls', path: '/srv/index.m3u8', codec: 'h264' } },
    ])));
    renderOutputs();
    const card = await screen.findByTestId('output-card-contribution-main-program-hls-primary-very-long-output-identifier');
    expect(within(card).getByText('contribution-main-program-hls-primary-very-long-output-identifier')).toHaveClass('min-w-0', 'break-all');
  });

  it('keeps the 48px touch floor on every card action and the phone disclosure classes', async () => {
    renderOutputs();
    await screen.findByTestId('output-card-program');
    const edit = screen.getByRole('button', { name: 'Edit output: Program HLS' });
    const del = screen.getByRole('button', { name: 'Delete output: Program HLS' });
    // TouchAction's own floor class: h-12 = the broadcast 48px minimum.
    expect(edit).toHaveClass('h-12');
    expect(del).toHaveClass('h-12');
    // The New output key keeps the same floor.
    expect(screen.getByRole('button', { name: 'New output' })).toHaveClass('h-12');

    // The Details disclosure is the phone-only (md:hidden) 48px key, fully
    // controlled: aria-expanded flips on press and the panel follows it.
    const detailsKeys = screen.getAllByRole('button', { name: 'Details' });
    expect(detailsKeys.length).toBe(OUTPUTS.length);
    const key = detailsKeys[0]!;
    expect(key).toHaveClass('h-12');
    expect(key).toHaveClass('md:hidden');
    expect(key).toHaveAttribute('aria-expanded', 'false');
    const panelId = key.getAttribute('aria-controls');
    expect(panelId).not.toBeNull();
    const panel = document.getElementById(panelId!);
    expect(panel).not.toBeNull();
    // Closed on narrow viewports, in flow from md: up.
    expect(panel).toHaveClass('hidden');
    expect(panel).toHaveClass('md:block');
    await userEvent.click(key);
    expect(key).toHaveAttribute('aria-expanded', 'true');
    expect(panel).toHaveClass('block');
    expect(panel).not.toHaveClass('hidden');
  });

  it('offers the derived HLS playlist link plus the honest active-output caveat', async () => {
    renderOutputs();
    await screen.findByTestId('output-card-program');
    const playlist = screen.getByRole('link', { name: 'Open HLS playlist' });
    expect(playlist).toHaveAttribute('href', '/hls/program/index.m3u8');
    expect(within(screen.getByTestId('output-card-program')).getByRole('link', { name: 'Watch program' })).toBeInTheDocument();
    // Freshness is absent from this surface by design: the configured
    // playlist link never claims the output is live. The caveat renders once
    // per card (in the details disclosure), so "exactly one per card" is the
    // honest count here.
    expect(
      screen.getAllByText(
        'Requires an active output; saved settings do not confirm playback.',
      ).length,
    ).toBe(OUTPUTS.length);
    // Only the unambiguous, non-colliding HLS mount got a link; the others
    // never invent one.
    expect(screen.getAllByRole('link', { name: 'Open HLS playlist' }).length).toBe(1);
  });

  it('reports the push link as a peer attached, never as media health', async () => {
    renderOutputs();
    const srt = await screen.findByTestId('output-card-srt-main');
    // The CURRENT link error reads as an alarm…
    const current = await within(srt).findByText('connection lost');
    expect(current).toHaveClass('text-destructive');
    // …while the previous session's error stays muted history.
    const history = within(srt).getByText('Last session ended: Input/output error');
    expect(history).toHaveClass('text-muted-foreground');
    expect(history).not.toHaveClass('text-destructive');
    // The real slow-peer drop stays an alarm (t10 / SRT-STATUS-01).
    const slow = within(srt).getByText(
      '61 packets dropped (slow peer, incl. keyframe resync)',
    );
    expect(slow).toHaveClass('text-destructive');
    // A connected link means a peer is attached — nothing more. The
    // cumulative written counter is never surfaced, and no producer/health
    // verdict is invented for a sink the outputs API reports no freshness
    // for.
    expect(screen.queryByText(/987654/)).not.toBeInTheDocument();
    expect(screen.queryByText('Producing')).not.toBeInTheDocument();
    expect(screen.queryByText('Healthy')).not.toBeInTheDocument();
  });

  it('refuses to edit an unknown-kind card, keeping the document as authored', async () => {
    renderOutputs();
    await screen.findByTestId('output-card-legacy-thing');
    const edit = screen.getByRole('button', {
      name: /Edit output: Mystery sink/,
    });
    expect(edit).toHaveAttribute('aria-disabled', 'true');
    expect(edit.getAttribute('aria-label')).toMatch(/isn't editable in this UI/i);
    await userEvent.click(edit);
    // No edit dialog opened — the authored document is preserved.
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(
      screen.getByRole('button', { name: 'Delete output: Mystery sink' }),
    ).toBeEnabled();
  });
});

describe('OutputsPage CRUD flow (card actions)', () => {
  it('uses the touch scroll surface and preserves the edit payload and ETag', async () => {
    let updated: { ifMatch: string | null; body: unknown } | undefined;
    server.use(http.put('*/api/v1/outputs/program', async ({ request }) => {
      updated = { ifMatch: request.headers.get('if-match'), body: await request.json() };
      return HttpResponse.json(OUTPUTS[0], { headers: { ETag: '"2"' } });
    }));
    const user = userEvent.setup();
    renderOutputs();
    await user.click(await screen.findByRole('button', { name: 'Edit output: Program HLS' }));
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toHaveClass('overflow-y-auto', 'max-h-[calc(100dvh-1rem)]');
    expect(within(dialog).getByRole('button', { name: 'Close' })).toHaveClass('size-12');
    const name = await within(dialog).findByLabelText('Name');
    await user.clear(name);
    await user.type(name, 'Renamed HLS');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => { expect(updated).toBeDefined(); });
    expect(updated).toEqual({ ifMatch: '"1"', body: { name: 'Renamed HLS', body: { kind: 'hls', path: '/srv/index.m3u8', codec: 'h264' } } });
    await waitFor(() => { expect(screen.queryByRole('dialog')).not.toBeInTheDocument(); });
  });

  it('edit loads the record via GET and prefills the kind-specific fields', async () => {
    renderOutputs();
    await screen.findByTestId('output-card-program');
    await userEvent.click(
      screen.getByRole('button', { name: 'Edit output: Program HLS' }),
    );
    const dialog = await screen.findByRole('dialog');
    // The kind-specific destination field prefills from the stored body…
    expect(await within(dialog).findByLabelText('Output path')).toHaveValue(
      '/srv/index.m3u8',
    );
    // …and the runnability honesty renders in the dialog too.
    expect(
      within(dialog).getByTestId('output-runnability'),
    ).toHaveAttribute('data-runnable', 'true');
  });

  it('validates inline per field on an invalid create, then posts the typed body', async () => {
    let posted:
      | { id: string; payload: { name: string; body: Record<string, unknown> } }
      | undefined;
    server.use(
      http.post('*/api/v1/outputs/:id', async ({ request, params }) => {
        posted = {
          id: String(params.id),
          payload: (await request.json()) as {
            name: string;
            body: Record<string, unknown>;
          },
        };
        return HttpResponse.json(
          {
            id: String(params.id),
            name: 'HLS two',
            body: { kind: 'hls', path: '/out.m3u8', codec: 'h264' },
          },
          { status: 201, headers: { ETag: '"2"' } },
        );
      }),
    );
    renderOutputs();
    await screen.findByTestId('output-card-program');
    await userEvent.click(screen.getByRole('button', { name: 'New output' }));
    const dialog = await screen.findByRole('dialog');
    // An empty submit shows inline, field-wired errors — never a far-away alert.
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    expect(
      (await within(dialog).findAllByText('This field is required.')).length,
    ).toBeGreaterThanOrEqual(2);
    const path = within(dialog).getByLabelText('Output path');
    expect(path).toHaveAttribute('aria-invalid', 'true');
    expect(path.getAttribute('aria-describedby') ?? '').not.toBe('');
    // Fill and save: the POSTed body is the typed form, kind-exact.
    await userEvent.type(within(dialog).getByLabelText('Identifier'), 'hls-2');
    await userEvent.type(within(dialog).getByLabelText('Name'), 'HLS two');
    await userEvent.type(path, '/out.m3u8');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    await waitFor(() => {
      expect(posted).toBeDefined();
    });
    const captured = posted as {
      id: string;
      payload: { name: string; body: Record<string, unknown> };
    };
    expect(captured.id).toBe('hls-2');
    expect(captured.payload.body.kind).toBe('hls');
    expect(captured.payload.body.path).toBe('/out.m3u8');
  });

  it('delete asks for confirmation and carries the fetched If-Match', async () => {
    renderOutputs();
    await screen.findByTestId('output-card-program');
    await userEvent.click(
      screen.getByRole('button', { name: 'Delete output: Program HLS' }),
    );
    const dialog = await screen.findByRole('dialog');
    expect(
      await within(dialog).findByText('Delete this resource?'),
    ).toBeInTheDocument();
    expect(within(dialog).getByText('Program HLS')).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('button', { name: 'Delete' }));
    await waitFor(() => {
      expect(deleted).toBeDefined();
    });
    const captured = deleted as { id: string; ifMatch: string | null };
    // The list response carries no per-item ETags, so the delete fetched the
    // record first and echoed its ETag as If-Match (optimistic concurrency).
    expect(captured.id).toBe('program');
    expect(captured.ifMatch).toBe('"1"');
  });
});

describe('OutputsPage collision honesty (derived mounts)', () => {
  const COLLIDING = [
    {
      id: 'program',
      name: 'Program HLS',
      body: { kind: 'hls', path: '/a/index.m3u8', codec: 'h264' },
    },
    {
      id: 'a b',
      name: 'First colliding HLS',
      body: { kind: 'hls', path: '/b/index.m3u8', codec: 'h264' },
    },
    {
      id: 'a-b',
      name: 'Second colliding HLS',
      body: { kind: 'hls', path: '/c/index.m3u8', codec: 'h264' },
    },
    {
      id: 'a-b-2',
      name: 'Runtime-suffixed HLS',
      body: { kind: 'hls', path: '/d/index.m3u8', codec: 'h264' },
    },
  ];

  it('keeps the stable mount link and withholds the ambiguous duplicate', async () => {
    server.use(
      http.get('*/api/v1/outputs', () => HttpResponse.json(COLLIDING)),
    );
    renderOutputs();
    await screen.findByTestId('output-card-program');
    await screen.findByTestId('output-card-a-b-2');
    // The FIRST id keeps its stable mount: its derived playlist link stays…
    const stable = screen.getByRole('link', { name: 'Open HLS playlist' });
    expect(stable).toHaveAttribute('href', '/hls/program/index.m3u8');
    // …while the normalized duplicates and the runtime-suffixed mount get
    // no link: their mounts depend on configuration order.
    for (const id of ['a b', 'a-b', 'a-b-2']) {
      const collidingCard = within(screen.getByTestId(`output-card-${id}`));
      expect(collidingCard.queryByRole('link', { name: 'Open HLS playlist' })).not.toBeInTheDocument();
      expect(
        collidingCard.getByText(
          'HLS link unavailable: output IDs resolve to colliding mounts; the runtime mount depends on configuration order.',
        ),
      ).toBeInTheDocument();
    }
    // Only one playlist link exists, exactly as with the old table.
    expect(screen.getAllByRole('link', { name: 'Open HLS playlist' }).length).toBe(1);
  });
});
