// #419 item 3: SourcesPage opens the create dialog on load when visited as
// /sources?new=1 (the Desk's "Add source" entry point).
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SourcesPage } from './SourcesPage';
import { renderWithProviders } from '../test/render';

const server = setupServer(http.get('*/api/v1/sources', () => HttpResponse.json([])));

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'bypass' });
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

describe('SourcesPage ?new=1', () => {
  it('opens the New source dialog on load', async () => {
    renderWithProviders(
      <MemoryRouter initialEntries={['/sources?new=1']}>
        <SourcesPage />
      </MemoryRouter>,
    );
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'New source' })).toBeInTheDocument();
  });

  it('does not open the dialog without the query param', async () => {
    renderWithProviders(
      <MemoryRouter initialEntries={['/sources']}>
        <SourcesPage />
      </MemoryRouter>,
    );
    await screen.findByRole('button', { name: 'New source' });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });
});
