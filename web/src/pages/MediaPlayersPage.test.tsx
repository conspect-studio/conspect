// MediaPlayersPage — the VT transport panel (ADR-0057 / ADR-0097). These tests
// run with NO WebSocket, so there is no `media.player_state` — i.e. the
// authoritative LOADED-asset state is absent. A boot-idle player with a
// configured default must render as NOT LOADED (rule 27 — never fabricate the
// "loaded" state from config); the configured default is shown in its own
// distinct column.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { MediaPlayersPage } from './MediaPlayersPage';
import { renderWithProviders } from '../test/render';

// A player whose CONFIG declares a default asset 'opener', but which nothing has
// loaded at runtime (no media.player_state without a WebSocket).
const PLAYERS = [
  {
    id: 'vt-1',
    name: 'VT 1',
    body: { id: 'vt-1', default: 'opener', loop_default: true },
  },
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

function renderPage(): void {
  renderWithProviders(
    <MemoryRouter>
      <MediaPlayersPage />
    </MemoryRouter>,
  );
}

describe('MediaPlayersPage loaded-asset honesty', () => {
  it('shows a boot-idle player with a configured default as NOT LOADED, not "loaded <default>"', async () => {
    renderPage();
    // The player row renders.
    const idCode = await screen.findByText('vt-1');
    const row = idCode.closest('tr');
    expect(row).not.toBeNull();
    const cells = within(row as HTMLElement);

    // The configured default asset id appears (in its own "Configured default"
    // column) — but NOT as the loaded asset.
    expect(cells.getByText('opener')).toBeInTheDocument();

    // The LOADED-asset cell must say "Not loaded" — the configured default is
    // NEVER presented as loaded when no media.player_state confirms a load.
    expect(cells.getByText('Not loaded')).toBeInTheDocument();
  });

  it('does not present the configured default under a loaded/live label', async () => {
    renderPage();
    await screen.findByText('vt-1');
    // There is no live state, so no "playing/paused/vamping" badge — the live
    // state column reads "No live state".
    expect(screen.getByText('No live state')).toBeInTheDocument();
  });
});
