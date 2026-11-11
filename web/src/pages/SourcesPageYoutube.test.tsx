// `youtube` source-kind form: the live-only scope note (always shown) and the
// honest capability warning that reflects `GET /api/v1/system/capabilities`
// (ADR-0015 / ADR-W030). These are ADDITIVE to the clock/timer kind-form tests
// — only the `youtube` kind fields are covered here.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SourcesPage } from './SourcesPage';
import { renderWithProviders } from '../test/render';

const server = setupServer(
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'bypass' });
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function mockCapabilities(youtube: unknown): void {
  server.use(
    http.get('*/api/v1/system/capabilities', () =>
      HttpResponse.json({
        backends: [{ kind: 'software', stage: 'decode', available: true }],
        compositor: { class: 'none' },
        build: { effective_license: 'LGPL-clean', redistributable: true, features: [] },
        youtube,
      }),
    ),
  );
}

/** Open the create dialog and pick the `youtube` kind. */
async function openYoutubeForm(): Promise<void> {
  renderWithProviders(
    <MemoryRouter>
      <SourcesPage />
    </MemoryRouter>,
  );
  await userEvent.click(await screen.findByRole('button', { name: 'New source' }));
  await screen.findByRole('dialog');
  const triggers = screen.getAllByRole('combobox');
  const kindTrigger = triggers[0];
  if (kindTrigger === undefined) {
    throw new Error('no Kind select trigger found');
  }
  await userEvent.click(kindTrigger);
  await userEvent.click(await screen.findByRole('option', { name: 'youtube' }));
}

describe('YouTube source form (ADR-0015)', () => {
  it('always shows the live-only scope note, naming the VT clip alternative', async () => {
    mockCapabilities({ compiled: true, available: true, version: 'yt-dlp 2026.01.01' });
    await openYoutubeForm();

    expect(
      await screen.findByText(/Live streams only/i),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/never a pre-recorded\/past video/i),
    ).toBeInTheDocument();
    expect(screen.getByText(/VT clip instead \(not built yet\)/i)).toBeInTheDocument();
  });

  it('warns when yt-dlp was not found on an otherwise youtube-capable build', async () => {
    mockCapabilities({ compiled: true, available: false });
    await openYoutubeForm();

    expect(
      await screen.findByText(/`yt-dlp` was not found/i),
    ).toBeInTheDocument();
  });

  it('warns that the build lacks the youtube feature when compiled is false', async () => {
    mockCapabilities({ compiled: false, available: false });
    await openYoutubeForm();

    expect(
      await screen.findByText(/does not include YouTube live ingest/i),
    ).toBeInTheDocument();
  });

  it('shows no capability warning when yt-dlp is available', async () => {
    mockCapabilities({ compiled: true, available: true, version: 'yt-dlp 2026.01.01' });
    await openYoutubeForm();

    // The scope note still renders, but no capability warning.
    await screen.findByText(/Live streams only/i);
    expect(screen.queryByText(/yt-dlp.*not found/i)).not.toBeInTheDocument();
    expect(
      screen.queryByText(/does not include YouTube live ingest/i),
    ).not.toBeInTheDocument();
  });
});
