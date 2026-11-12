// OutputsPage track selector (ADR-R005 §4.3 UX): the discrete-track picker in
// the Advanced section must offer ONLY the tracks the audio-routing document
// currently declares as selectable — never a free-text field that could name
// a track that does not exist.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { OutputsPage } from './OutputsPage';
import { renderWithProviders } from '../test/render';

const ROUTING = {
  configured: true,
  routing: { sample_rate_hz: 48000, routes: [] },
  selectable_tracks: ['prog', 'cam1-clean', 'commentary'],
};

const ROUTE_ROWS: unknown[] = [];

const server = setupServer(
  http.get('*/api/v1/outputs', () => HttpResponse.json([])),
  // Output feed routing (ADR-0108 §8): the card's Video/Audio feed row.
  http.get('*/api/v1/routing', () => HttpResponse.json({ outputs: ROUTE_ROWS })),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.get('*/api/v1/media/players', () => HttpResponse.json([])),
  http.get('*/api/v1/devices', () => HttpResponse.json([])),
  http.get('*/api/v1/audio-routing', () =>
    HttpResponse.json(ROUTING, { headers: { ETag: 'W/"3"' } }),
  ),
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

function renderOutputs(): void {
  renderWithProviders(
    <MemoryRouter>
      <OutputsPage />
    </MemoryRouter>,
  );
}

/** Open the "Audio" select and pick "Explicit track list". */
async function chooseExplicitTrackList(
  user: ReturnType<typeof userEvent.setup>,
  dialog: HTMLElement,
): Promise<void> {
  await user.click(within(dialog).getByRole('combobox', { name: /^audio$/i }));
  const listbox = await screen.findByRole('listbox');
  await user.click(within(listbox).getByText(/explicit track list/i));
}

describe('OutputsPage audio track picker', () => {
  it('offers only the currently selectable tracks, as checkboxes', async () => {
    const user = userEvent.setup();
    renderOutputs();
    await user.click(await screen.findByRole('button', { name: 'New output' }));
    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByText(/advanced/i));
    await chooseExplicitTrackList(user, dialog);

    // Exactly the server's selectable tracks are offered as checkboxes — no
    // free-text entry that could name a track that doesn't exist.
    expect(within(dialog).getByLabelText('prog')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('cam1-clean')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('commentary')).toBeInTheDocument();
    expect(within(dialog).queryByPlaceholderText('prog, commentary')).not.toBeInTheDocument();
  });

  it('checking a track adds it to the saved selection', async () => {
    const user = userEvent.setup();
    let posted: unknown;
    server.use(
      http.post('*/api/v1/outputs/:id', async ({ request, params }) => {
        posted = { id: params.id, payload: await request.json() };
        return HttpResponse.json(
          { id: String(params.id), name: 'n', body: { kind: 'hls', path: '/p', codec: 'h264' } },
          { status: 201, headers: { ETag: '"1"' } },
        );
      }),
    );
    renderOutputs();
    await user.click(await screen.findByRole('button', { name: 'New output' }));
    const dialog = await screen.findByRole('dialog');
    await user.type(within(dialog).getByLabelText('Identifier'), 'hls-1');
    await user.type(within(dialog).getByLabelText('Name'), 'HLS one');

    // Pick the "hls" transport kind.
    await user.click(within(dialog).getByRole('combobox', { name: /transport/i }));
    const kindListbox = await screen.findByRole('listbox');
    await user.click(within(kindListbox).getByText(/^hls$/i));

    await user.type(await within(dialog).findByLabelText(/output path/i), '/out.m3u8');
    await user.click(within(dialog).getByText(/advanced/i));
    await chooseExplicitTrackList(user, dialog);
    await user.click(await within(dialog).findByLabelText('cam1-clean'));
    await user.click(within(dialog).getByRole('button', { name: 'Create' }));

    await waitFor(() => {
      expect(posted).toBeDefined();
    });
    const captured = posted as { id: string; payload: { body: Record<string, unknown> } };
    expect(captured.payload.body.audio).toMatchObject({
      mode: 'tracks',
      tracks: ['cam1-clean'],
    });
  });
});
