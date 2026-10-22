// LogsPage: a focused render test over MSW. Verifies the page reads the log tail
// and renders the level + target + message, and that choosing a level filter
// re-requests with the `level` query parameter.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { LogsPage } from './LogsPage';
import { I18nProvider as AppI18nProvider } from '../i18n/I18nProvider';
import { renderWithProviders } from '../test/render';

/** Render LogsPage inside the app i18n provider (supplies the locale context). */
function renderLogs(): void {
  renderWithProviders(
    <AppI18nProvider>
      <LogsPage />
    </AppI18nProvider>,
  );
}

let lastLevel: string | null = null;

const server = setupServer(
  http.get('http://localhost:3000/api/v1/logs', ({ request }) => {
    lastLevel = new URL(request.url).searchParams.get('level');
    return HttpResponse.json([
      {
        level: 'warn',
        message: 'tile fell back to last-good frame',
        seq: 7,
        target: 'multiview_engine',
        timestamp_ms: 1_700_000_000_000,
        resource_id: 'cam-north',
        resource_kind: 'source',
      },
    ]);
  }),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  lastLevel = null;
});
afterAll(() => {
  server.close();
});

describe('LogsPage', () => {
  it('lists a log record with its level, target and message', async () => {
    renderLogs();

    expect(
      await screen.findByText('tile fell back to last-good frame'),
    ).toBeInTheDocument();
    expect(screen.getByText('multiview_engine')).toBeInTheDocument();
    expect(screen.getByText('source/cam-north')).toBeInTheDocument();
    // The level pill carries the level as text (never colour alone).
    expect(screen.getByText('warn')).toBeInTheDocument();
  });

  it('re-requests with a level filter when one is chosen', async () => {
    const user = userEvent.setup();
    renderLogs();
    await screen.findByText('tile fell back to last-good frame');

    // Open the "Minimum level" select and pick `error`.
    await user.click(screen.getByRole('combobox', { name: /Minimum level/i }));
    const listbox = await screen.findByRole('listbox');
    await user.click(within(listbox).getByText('error'));

    await waitFor(() => {
      expect(lastLevel).toBe('error');
    });
  });
});
