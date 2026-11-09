// MediaPlayersPage — the VT transport panel (ADR-0057 / ADR-0097), adopted to
// the approved broadcast design: one compact VT card per configured player
// (TaskWorkspace), ONE focused slot in the phone fold (the others collapse to
// a 48px switch row), 48px TouchAction keys, vamp + config facts behind the
// per-card Details disclosure, and honest persistent inline operation
// feedback (OperationFeedback) instead of transient toasts.
//
// These tests run with NO WebSocket, so no `media.player_state` arrives on
// its own: cases that need live truth seed the realtime cache directly, and
// cases that need a command outcome feed FAKE envelopes into the SHARED
// QueryClient operation tracker exactly the way the shell-owned
// `useEngineEvents` subscription does (`getOperationTracker(client).onEnvelope`).
// Every submission is a REAL MSW HTTP request: the operation id comes from an
// actual 202 body, never a fixture-only shortcut.
//
// The preserved honesty contract (rule 27 — never fabricate state) holds: a
// boot-idle player with a configured default renders as NOT LOADED, the
// configured default shows as its own separate fact, and a missing runtime
// observation never infers a state. The tracker join adds: pending resolves to
// applied ONLY on exact terminal evidence; uncorrelated load/seek stay
// accepted-unobserved forever; a lost link is unknown, never failed.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';

import { MediaPlayersPage } from './MediaPlayersPage';
import { MEDIA_PLAYER_STATE_QUERY_KEY } from '../realtime/useEngineEvents';
import type { MediaPlayerEvent } from '../realtime/generated-types';
import type { Envelope } from '../realtime/envelope';
import { getOperationTracker } from '../components/broadcast/operationTracker';
import { messages } from '../locales/en/messages';

i18n.load('en', messages);
i18n.activate('en');

// Two configured players: vt-1 declares a default asset 'opener' (looping),
// vt-2 declares none. Nothing has loaded at runtime without a WebSocket.
const PLAYERS = [
  { id: 'vt-1', name: 'VT 1', body: { id: 'vt-1', default: 'opener', loop_default: true } },
  { id: 'vt-2', name: 'VT 2', body: { id: 'vt-2' } },
];

const server = setupServer(
  http.get('*/api/v1/media/players', () => HttpResponse.json(PLAYERS)),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

interface RenderedPage {
  readonly client: QueryClient;
  readonly unmount: () => void;
}

function renderPage(
  options: {
    readonly live?: Record<string, MediaPlayerEvent>;
    readonly client?: QueryClient;
  } = {},
): RenderedPage {
  const client = options.client
    ?? new QueryClient({
      defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
    });
  if (options.live !== undefined) {
    // Seed the realtime latest-wins player-state cache exactly as
    // `media.player_state` events would have written it.
    client.setQueryData(MEDIA_PLAYER_STATE_QUERY_KEY, options.live);
  }
  const view = render(
    <QueryClientProvider client={client}>
      <I18nProvider i18n={i18n}>
        <MemoryRouter>
          <MediaPlayersPage />
        </MemoryRouter>
      </I18nProvider>
    </QueryClientProvider>,
  );
  return { client, unmount: view.unmount };
}

/** A fake `media.player_state` envelope, exactly as the shell would deliver it. */
function playerEvent(
  corr: string | undefined,
  playerId: string,
  state: MediaPlayerEvent['state'],
): Envelope {
  return {
    v: 1,
    t: 'media.player_state',
    topic: 'switcher',
    id: playerId,
    seq: 1,
    ts: 1,
    ...(corr === undefined ? {} : { corr }),
    data: { player: playerId, position_frames: 0, state },
  };
}

/** A fake `job.progress` envelope — never terminal evidence for anything. */
function progressEvent(corr: string, playerId: string): Envelope {
  return {
    v: 1,
    t: 'job.progress',
    topic: 'jobs',
    id: playerId,
    seq: 2,
    ts: 2,
    corr,
    data: { phase: 'loading', pct: 10 },
  };
}

/** One player's transport console (the always-rendered slot div). */
function consoleOf(id: string): HTMLElement {
  return screen.getByTestId(`player-console-${id}`);
}

/** The nth card's always-visible status line (render order = data order). */
function statusOf(index: number): HTMLElement {
  return screen.getAllByTestId('task-workspace-status')[index]!;
}

/** The feedback region element wrapping the given text. */
function regionOf(text: RegExp | string): HTMLElement {
  const line = screen.getByText(text);
  return line.closest<HTMLElement>('[data-phase]')!;
}

describe('MediaPlayersPage loaded-asset honesty (preserved)', () => {
  it('shows a boot-idle player with a configured default as NOT LOADED, not "loaded <default>"', async () => {
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    // The configured default asset id is visible as its own config fact...
    expect(screen.getByText('opener')).toBeInTheDocument();
    // ...but the LOADED slot reads "Not loaded" — no media.player_state has
    // confirmed a load, so the default is never presented as loaded.
    expect(within(statusOf(0)).getByText('Not loaded')).toBeInTheDocument();
    expect(within(statusOf(0)).queryByText('opener')).toBeNull();
  });

  it('does not present the configured default under a loaded/live label', async () => {
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    // No live state, so no playing/paused/vamping badge — the status line
    // reads "No live state" and never infers a state from absence.
    expect(within(statusOf(0)).getByText('No live state')).toBeInTheDocument();
    expect(within(statusOf(0)).queryByText('Stopped')).toBeNull();
    expect(within(statusOf(0)).queryByText('Playing')).toBeNull();
  });
});

describe('MediaPlayersPage responsive slot fold', () => {
  it('keeps one focused console in the phone fold and collapses the other slots', async () => {
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    // The first configured player is focused by default; its console shows.
    expect(consoleOf('vt-1').className).toMatch(/(^| )block( |$)/);
    // The other slot collapses on the phone and is in flow from md: up.
    expect(consoleOf('vt-2').className).toMatch(/(^| )hidden( |$)/);
    expect(consoleOf('vt-2').className).toMatch(/\bmd:block\b/);
    // The compact switch row exists only on narrow viewports.
    const focusRow = screen.getByRole('button', { name: 'Focus player transport: vt-2' });
    expect(focusRow.className).toMatch(/\bmd:hidden\b/);
    // The focused slot offers no switch row for itself.
    expect(screen.queryByRole('button', { name: 'Focus player transport: vt-1' })).toBeNull();
  });

  it('switches the focused slot through the controlled selection — and back', async () => {
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Focus player transport: vt-2' }));
    expect(consoleOf('vt-2').className).toMatch(/(^| )block( |$)/);
    expect(consoleOf('vt-1').className).toMatch(/(^| )hidden( |$)/);
    // Switching back: the previous slot returns to its compact row.
    await userEvent.click(screen.getByRole('button', { name: 'Focus player transport: vt-1' }));
    expect(consoleOf('vt-1').className).toMatch(/(^| )block( |$)/);
    expect(consoleOf('vt-2').className).toMatch(/(^| )hidden( |$)/);
  });
});

describe('MediaPlayersPage live truth vs configured default', () => {
  it('shows the measured live asset and playhead, separately from the configured default', async () => {
    renderPage({
      live: {
        'vt-1': {
          player: 'vt-1',
          asset: 'live-clip',
          position_frames: 1250,
          state: { kind: 'playing' },
        },
      },
    });
    await screen.findByTestId('player-console-vt-1');
    const status = statusOf(0);
    expect(within(status).getByText('Playing')).toBeInTheDocument();
    expect(within(status).getByText('live-clip')).toBeInTheDocument();
    // The playhead is raw integer frames from the event — the DTO carries no
    // cadence, so no timecode is invented.
    expect(status).toHaveTextContent(/1250 frames/);
    expect(status.textContent).not.toMatch(/\d{1,2}:\d{2}:\d{2}/);
    // The configured default stays its own separate fact.
    expect(within(status).queryByText('opener')).toBeNull();
    expect(screen.getByText('opener')).toBeInTheDocument();
    // The other player still honestly shows nothing.
    expect(within(statusOf(1)).getByText('Not loaded')).toBeInTheDocument();
  });

  it('labels missing live observations honestly on the vamp keys — never "not vamping"', async () => {
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    expect(screen.getByRole('button', { name: 'Arm vamp exit: vt-1' })).toBeDisabled();
    // Every gated exit key says the live truth is MISSING — the reason never
    // claims the player is "not vamping" or stopped.
    expect(
      screen.getAllByText("No live state yet — the player's vamp status is unknown.")
        .length,
    ).toBeGreaterThan(0);
    expect(screen.queryByText('Only while the player is vamping.')).toBeNull();
  });

  it('gates the vamp-exit triad on the live state only', async () => {
    renderPage({
      live: {
        'vt-1': {
          player: 'vt-1',
          position_frames: 12,
          state: { kind: 'vamping', exit_armed: true },
        },
      },
    });
    await screen.findByTestId('player-console-vt-1');
    expect(screen.getByRole('button', { name: 'Arm vamp exit: vt-1' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Take vamp exit: vt-1' })).toBeEnabled();
    expect(screen.getByRole('button', { name: 'Cancel vamp exit: vt-1' })).toBeEnabled();
    expect(within(statusOf(0)).getByText(/Vamping · exit armed/)).toBeInTheDocument();
  });
});

describe('MediaPlayersPage touch targets', () => {
  it('keeps every transport, vamp, switch and disclosure key at the 48px floor', async () => {
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    const names = [
      'Load asset into player: vt-1',
      'Cue player to in-point: vt-1',
      'Play player: vt-1',
      'Pause player: vt-1',
      'Stop player: vt-1',
      'Arm vamp exit: vt-1',
      'Take vamp exit: vt-1',
      'Cancel vamp exit: vt-1',
      'Focus player transport: vt-2',
    ];
    for (const name of names) {
      const key = screen.getByRole('button', { name });
      expect(key.className).toMatch(/\bh-12\b/);
      expect(key.className).toMatch(/\bmin-w-12\b/);
    }
    // The per-card Details disclosure is a 48px TouchAction too.
    for (const key of screen.getAllByRole('button', { name: 'Details' })) {
      expect(key.className).toMatch(/\bh-12\b/);
    }
  });
});

describe('MediaPlayersPage operation feedback', () => {
  it('says only "sending" while the POST is in flight — the accepted wording waits for the real 202', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', async () => {
        await new Promise((resolve) => {
          setTimeout(resolve, 80);
        });
        return HttpResponse.json({ operation_id: 'op-9001', kind: 'play' });
      }),
    );
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    // The pressed key spins in its own box; the player's other keys are
    // blocked against duplicate submission.
    const play = screen.getByRole('button', { name: 'Play player: vt-1' });
    expect(play).toBeDisabled();
    expect(play).toHaveAttribute('aria-busy', 'true');
    expect(screen.getByRole('button', { name: 'Pause player: vt-1' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Cue player to in-point: vt-1' })).toBeDisabled();
    // The other slot's transport stays usable (per-slot gating, not global).
    expect(screen.getByRole('button', { name: 'Play player: vt-2' })).toBeEnabled();
    // A separate local sending status — never the accepted wording.
    expect(consoleOf('vt-1')).toHaveTextContent(/Sending Play VT 1…/);
    expect(consoleOf('vt-1').textContent).not.toMatch(/accepted/i);
    // The real 202 is the only thing that earns the accepted wording.
    expect(
      await screen.findByText(/Play VT 1 accepted — waiting for the applied event\./),
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Play player: vt-1' })).toBeEnabled();
  });

  it('shows the accepted 202 persistently with its operation id — and never claims applied on its own', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () =>
        HttpResponse.json({ operation_id: 'op-9001', kind: 'play' }),
      ),
    );
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    const line = await screen.findByText(/Play VT 1 accepted — waiting for the applied event\./);
    const region = line.closest<HTMLElement>('[data-phase]');
    expect(region?.dataset.phase).toBe('pending');
    // The correlation id is retained for later reconciliation.
    expect(region).toHaveTextContent('op-9001');
    // No success or applied claim ever comes from the page's own state: only
    // the shared tracker's exact terminal evidence can produce `applied`.
    expect(region?.textContent).not.toMatch(/applied(?! event)/);
    expect(region?.textContent).not.toMatch(/Command accepted/);
    // The region is inline and persistent — it survives re-renders.
    await userEvent.click(screen.getAllByRole('button', { name: 'Details' })[0]!);
    expect(
      screen.getByText(/Play VT 1 accepted — waiting for the applied event\./),
    ).toBeInTheDocument();
    // An unresolved acceptance still deadlocks nothing.
    expect(screen.getByRole('button', { name: 'Play player: vt-1' })).toBeEnabled();
  });

  it('reports a definitive rejection as failed with the server reason, dismissable', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () =>
        HttpResponse.json(
          { status: 409, title: 'Player busy', detail: 'The deck is mid-load' },
          { status: 409 },
        ),
      ),
    );
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    const line = await screen.findByText(/Play VT 1 failed\./);
    const region = line.closest<HTMLElement>('[data-phase]');
    expect(region?.dataset.phase).toBe('failed');
    expect(region).toHaveTextContent(/Player busy: The deck is mid-load/);
    await userEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    await waitFor(() => {
      expect(screen.queryByText(/Play VT 1 failed\./)).toBeNull();
    });
  });

  it('keeps an unverifiable outcome unknown — blocking only the duplicate verb, never the page', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () => HttpResponse.error()),
    );
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    const line = await screen.findByText(/Play VT 1 outcome unknown\./);
    const region = line.closest<HTMLElement>('[data-phase]');
    expect(region?.dataset.phase).toBe('unknown');
    // The page's own sentence claims neither failure nor rollback; only the
    // appended raw transport detail (the browser's "Failed to fetch") is
    // allowed to carry the word, and it is browser text, not a UI claim.
    expect(region).toHaveTextContent(/may or may not have been applied/);
    // The page-authored reason sentence is the claim itself: it names the
    // uncertainty and never a failure or a rollback.
    expect(
      screen.getByText(
        /The control plane did not answer\. The command may or may not have been applied\./,
      ),
    ).toBeInTheDocument();
    // The duplicate of the possibly-in-flight verb is blocked...
    expect(screen.getByRole('button', { name: 'Play player: vt-1' })).toBeDisabled();
    // ...while every other verb stays usable — the page never deadlocks.
    expect(screen.getByRole('button', { name: 'Pause player: vt-1' })).toBeEnabled();
    expect(screen.getByRole('button', { name: 'Stop player: vt-1' })).toBeEnabled();
  });
});

describe('MediaPlayersPage tracker join — shared QueryClient tracker, real MSW 202', () => {
  it('resolves pending to applied only on the exact terminal event, after the real 202', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () =>
        HttpResponse.json({ operation_id: 'op-9001', kind: 'play' }),
      ),
    );
    const { client } = renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    const line = await screen.findByText(/Play VT 1 accepted — waiting for the applied event\./);
    expect(line.closest<HTMLElement>('[data-phase]')?.dataset.phase).toBe('pending');
    // A different state on the same corr is a state change, NOT the play
    // command's outcome — and it is never a failure either.
    getOperationTracker(client).onEnvelope(playerEvent('op-9001', 'vt-1', { kind: 'paused' }));
    expect(regionOf(/Play VT 1 accepted — waiting for the applied event\./).dataset.phase)
      .toBe('pending');
    // The exact terminal state for the play verb settles it.
    getOperationTracker(client).onEnvelope(playerEvent('op-9001', 'vt-1', { kind: 'playing' }));
    expect(await screen.findByText(/Play VT 1 applied\./)).toBeInTheDocument();
    expect(screen.queryByText(/waiting for the applied event/)).toBeNull();
    expect(regionOf(/Play VT 1 applied\./).dataset.phase).toBe('applied');
  });

  it('joins an early socket frame that arrived before the real 202', async () => {
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
    });
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () => {
        // The shell's shared tracker sees the outcome frame BEFORE the 202
        // response resolves — the bounded early cache must join it.
        getOperationTracker(client).onEnvelope(playerEvent('op-9001', 'vt-1', { kind: 'playing' }));
        return HttpResponse.json({ operation_id: 'op-9001', kind: 'play' });
      }),
    );
    renderPage({ client });
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    // The operation id still comes from the actual 202 body, and the early
    // frame settles it straight to applied — no fabricated correlation.
    expect(await screen.findByText(/Play VT 1 applied\./)).toBeInTheDocument();
    expect(getOperationTracker(client).snapshot().records.get('op-9001')?.phase)
      .toBe('applied');
  });

  it('ignores a wrong-resource frame, a progress frame and a wrong-topic frame', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () =>
        HttpResponse.json({ operation_id: 'op-9001', kind: 'play' }),
      ),
    );
    const { client } = renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    await screen.findByText(/Play VT 1 accepted — waiting for the applied event\./);
    // Wrong resource (another player), job progress and a wrong topic are all
    // noise: none of them may settle or poison the waiting record.
    getOperationTracker(client).onEnvelope(playerEvent('op-9001', 'vt-2', { kind: 'playing' }));
    getOperationTracker(client).onEnvelope(progressEvent('op-9001', 'vt-1'));
    getOperationTracker(client).onEnvelope({
      ...playerEvent('op-9001', 'vt-1', { kind: 'playing' }),
      topic: 'devices',
    });
    expect(regionOf(/Play VT 1 accepted — waiting for the applied event\./).dataset.phase)
      .toBe('pending');
    // The exact frame afterwards still applies.
    getOperationTracker(client).onEnvelope(playerEvent('op-9001', 'vt-1', { kind: 'playing' }));
    expect(await screen.findByText(/Play VT 1 applied\./)).toBeInTheDocument();
  });

  it('distinguishes arm from disarm: only exit_armed=true applied the arm verb', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/exit/arm', () =>
        HttpResponse.json({ operation_id: 'op-arm', kind: 'arm_media_exit' }),
      ),
    );
    const { client } = renderPage({
      live: {
        'vt-1': {
          player: 'vt-1',
          position_frames: 3,
          state: { kind: 'vamping', exit_armed: false },
        },
      },
    });
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Arm vamp exit: vt-1' }));
    await screen.findByText(/Arm vamp exit on VT 1 accepted — waiting for the applied event\./);
    // Same state kind, wrong exit_armed: not the arm command's outcome.
    getOperationTracker(client).onEnvelope(
      playerEvent('op-arm', 'vt-1', { kind: 'vamping', exit_armed: false }),
    );
    expect(
      regionOf(/Arm vamp exit on VT 1 accepted — waiting for the applied event\./).dataset.phase,
    ).toBe('pending');
    getOperationTracker(client).onEnvelope(
      playerEvent('op-arm', 'vt-1', { kind: 'vamping', exit_armed: true }),
    );
    expect(await screen.findByText(/Arm vamp exit on VT 1 applied\./)).toBeInTheDocument();
  });

  it('keeps an uncorrelated load accepted-unobserved — never applied, even on a state event', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/load', () =>
        HttpResponse.json({ operation_id: 'op-77', kind: 'load' }),
      ),
    );
    const { client } = renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Load asset into player: vt-1' }));
    const input = screen.getByLabelText('Asset id');
    await userEvent.type(input, 'promo-clip');
    await userEvent.click(screen.getByRole('button', { name: 'Load' }));
    const line = await screen.findByText(/Load VT 1 accepted\./);
    const region = line.closest<HTMLElement>('[data-phase]');
    expect(region?.dataset.phase).toBe('accepted-unobserved');
    // The honest uncorrelated note replaces the "waiting for the applied
    // event" wording: no applied event will ever arrive for load.
    expect(screen.getByText(/does not correlate this command's outcome/)).toBeInTheDocument();
    expect(screen.queryByText(/waiting for the applied event/)).toBeNull();
    expect(region).toHaveTextContent('op-77');
    // Even a state event naming this operation id settles nothing (the server
    // never correlates load), and the real load outcome arrives uncorrelated.
    getOperationTracker(client).onEnvelope(playerEvent('op-77', 'vt-1', { kind: 'cued' }));
    getOperationTracker(client).onEnvelope(playerEvent(undefined, 'vt-1', { kind: 'cued' }));
    expect(screen.queryByText(/Load VT 1 applied\./)).toBeNull();
    expect(regionOf(/Load VT 1 accepted\./).dataset.phase).toBe('accepted-unobserved');
  });

  it('marks a lost realtime link unknown — gating only the duplicate verb, reconcilable late', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () =>
        HttpResponse.json({ operation_id: 'op-9001', kind: 'play' }),
      ),
    );
    const { client } = renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    await screen.findByText(/Play VT 1 accepted — waiting for the applied event\./);
    // A gap/disconnect loses the observation window: unknown, never failed.
    getOperationTracker(client).onGap();
    const line = await screen.findByText(/Play VT 1 outcome unknown\./);
    const region = line.closest<HTMLElement>('[data-phase]');
    expect(region?.dataset.phase).toBe('unknown');
    expect(region).toHaveTextContent(/applied event was not observed/);
    // The operation id stays visible for later reconciliation...
    expect(region).toHaveTextContent('op-9001');
    // ...the duplicate verb is gated, every other verb stays usable.
    expect(screen.getByRole('button', { name: 'Play player: vt-1' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Pause player: vt-1' })).toBeEnabled();
    // A late exact event still reconciles the unknown record to applied.
    getOperationTracker(client).onEnvelope(playerEvent('op-9001', 'vt-1', { kind: 'playing' }));
    expect(await screen.findByText(/Play VT 1 applied\./)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Play player: vt-1' })).toBeEnabled();
  });

  it('persists the latest operation across page navigation — and resolves it while away', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () =>
        HttpResponse.json({ operation_id: 'op-9001', kind: 'play' }),
      ),
    );
    const first = renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    await screen.findByText(/Play VT 1 accepted — waiting for the applied event\./);
    first.unmount();
    // While the operator is on another page, the shell keeps feeding the
    // shared tracker; returning to this page must show the resolved truth.
    getOperationTracker(first.client).onEnvelope(
      playerEvent('op-9001', 'vt-1', { kind: 'playing' }),
    );
    renderPage({ client: first.client });
    expect(await screen.findByText(/Play VT 1 applied\./)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Play player: vt-1' })).toBeEnabled();
  });

  it('settles an in-flight submission while the page is unmounted', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', async () => {
        await new Promise((resolve) => {
          setTimeout(resolve, 80);
        });
        return HttpResponse.json({ operation_id: 'op-9002', kind: 'play' });
      }),
    );
    const { client, unmount } = renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    expect(consoleOf('vt-1')).toHaveTextContent(/Sending Play VT 1…/);
    unmount();
    // The 202 lands while the page is away; the QueryClient-backed record
    // (not component state) carries it.
    await waitFor(() => {
      expect(getOperationTracker(client).snapshot().records.has('op-9002')).toBe(true);
    });
    renderPage({ client });
    expect(
      await screen.findByText(/Play VT 1 accepted — waiting for the applied event\./),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Sending Play VT 1…/)).toBeNull();
  });

  it('keeps a transport-unknown outcome visible across navigation — never silently forgotten', async () => {
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () => HttpResponse.error()),
    );
    const { client, unmount } = renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    await screen.findByText(/Play VT 1 outcome unknown\./);
    unmount();
    // The anonymous seat stays in the tracker and the page's own record
    // survives navigation: the unresolved command is not silently dropped.
    renderPage({ client });
    expect(await screen.findByText(/Play VT 1 outcome unknown\./)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Play player: vt-1' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Pause player: vt-1' })).toBeEnabled();
    expect(
      [...getOperationTracker(client).snapshot().provisional.values()].some(
        (seat) => seat.phase === 'unknown',
      ),
    ).toBe(true);
  });

  it('shows the localized capacity message before any POST — not a raw exception', async () => {
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
    });
    // Fill the shared tracker's bounded store with unresolved anonymous seats
    // (exactly the state a long ambiguous session leaves behind).
    const tracker = getOperationTracker(client);
    for (let i = 0; i < 256; i += 1) {
      await expect(
        tracker.submit(() => Promise.reject(new Error('network')), undefined),
      ).rejects.toThrow('network');
    }
    let posts = 0;
    server.use(
      http.post('*/api/v1/media/players/vt-1/play', () => {
        posts += 1;
        return HttpResponse.json({ operation_id: 'op-x', kind: 'play' });
      }),
    );
    renderPage({ client });
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Play player: vt-1' }));
    // The actionable, localized message — never the raw exception text.
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(
      /Too many unresolved commands\. Wait for an outcome or resolve an unknown command before retrying\./,
    );
    // The refusal happened before the POST: nothing was sent, no replay.
    expect(posts).toBe(0);
    // A refusal is not a command outcome: it never blocks the transport.
    expect(screen.getByRole('button', { name: 'Play player: vt-1' })).toBeEnabled();
    expect(screen.queryByText(/Play VT 1 failed\./)).toBeNull();
    expect(screen.queryByText(/Play VT 1 outcome unknown\./)).toBeNull();
  });
});

describe('MediaPlayersPage load dialog — asset id only, honest capability', () => {
  it('labels the missing library capability honestly — no fake browse, save or unload keys', async () => {
    let loaded: string | undefined;
    server.use(
      http.post('*/api/v1/media/players/vt-1/load', async ({ request }) => {
        const body = (await request.json()) as { asset: string };
        loaded = body.asset;
        return HttpResponse.json({ operation_id: 'op-77', kind: 'load' });
      }),
    );
    renderPage();
    await screen.findByTestId('player-console-vt-1');
    await userEvent.click(screen.getByRole('button', { name: 'Load asset into player: vt-1' }));
    const dialog = screen.getByRole('dialog');
    // The honest capability note: no inventory exists to browse here.
    expect(within(dialog).getByText(/No asset browser is available here yet/)).toBeInTheDocument();
    // No fake affordances: browse/save/unload ship with the library backend
    // workstream (ML4), not now.
    expect(within(dialog).queryByRole('button', { name: /browse|save|unload/i })).toBeNull();
    // The draft is prefilled with the CONFIGURED default — as a starting
    // point for the typed id, never as a loaded-asset claim.
    const input = within(dialog).getByLabelText('Asset id');
    expect(input).toHaveValue('opener');
    // An empty id is refused inline: no POST, no fake success.
    await userEvent.clear(input);
    await userEvent.click(within(dialog).getByRole('button', { name: 'Load' }));
    expect(within(dialog).getByRole('alert')).toHaveTextContent(/An asset id is required/);
    expect(loaded).toBeUndefined();
    // A real id submits; the dialog hands over to the persistent region.
    await userEvent.type(input, 'promo-clip');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Load' }));
    await waitFor(() => {
      expect(loaded).toBe('promo-clip');
    });
    await screen.findByText(/Load VT 1 accepted\./);
    expect(screen.queryByRole('dialog')).toBeNull();
  });
});

describe('MediaPlayersPage list honesty (preserved)', () => {
  it('keeps the honest empty-config hint when no player is declared', async () => {
    server.use(http.get('*/api/v1/media/players', () => HttpResponse.json([])));
    renderPage();
    expect(await screen.findByText(/No media players are configured/)).toBeInTheDocument();
  });

  it('surfaces a players-list error honestly', async () => {
    server.use(
      http.get('*/api/v1/media/players', () =>
        HttpResponse.json({ status: 500, title: 'Boom' }, { status: 500 }),
      ),
    );
    renderPage();
    expect(await screen.findByText(/Could not load media players:/)).toBeInTheDocument();
  });
});
