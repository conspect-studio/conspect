import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';
import { SourcesPage } from './SourcesPage';
import { renderWithProviders } from '../test/render';

const records = [
  { id: 'camera', name: 'Studio camera', body: { kind: 'hls', url: 'https://example.test/live.m3u8' } },
  { id: 'future', name: 'Future input', body: { kind: 'future-transport' } },
];
const server = setupServer(
  http.get('*/api/v1/sources', () => HttpResponse.json(records)),
  http.get('*/api/v1/sources/status', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({})),
  http.get('*/api/v1/sources/camera', () => HttpResponse.json(records[0], { headers: { ETag: '"7"' } })),
  http.get('*/api/v1/devices', () => HttpResponse.json([])),
  http.get('*/api/v1/catalogues/iptv-org/links', () => HttpResponse.json({ links: [] })),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });
const renderSources = () => renderWithProviders(<MemoryRouter><SourcesPage /></MemoryRouter>);

describe('Sources touch cards', () => {
  it('keeps source facts and all existing fast paths in a responsive touch card', async () => {
    renderSources();
    const card = await screen.findByTestId('source-card-camera');
    expect(screen.queryByRole('table')).not.toBeInTheDocument();
    expect(within(card).getByText('Studio camera')).toBeInTheDocument();
    expect(within(card).getByText('https://example.test/live.m3u8')).toHaveClass('break-all');
    expect(within(card).getByRole('button', { name: 'Edit source: Studio camera' })).toHaveClass('min-h-12');
    expect(within(card).getByRole('button', { name: 'Delete source: Studio camera' })).toBeInTheDocument();
    expect(within(card).getByRole('button', { name: /preview/i })).toBeInTheDocument();
    expect(within(card).getByRole('button', { name: /events/i })).toBeInTheDocument();
    expect(within(card).getByText('Not linked to IPTV-ORG')).toBeInTheDocument();
    // An absent demand record is not evidence of either Idle or Ready.
    expect(within(card).queryByText('Idle')).not.toBeInTheDocument();
    expect(within(card).queryByText('Ready')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'New source' })).toBeInTheDocument();
  });

  it('refuses unknown-kind editing but preserves delete confirmation', async () => {
    renderSources();
    const card = await screen.findByTestId('source-card-future');
    const edit = within(card).getByRole('button', { name: /edit source: future input/i });
    expect(edit).toHaveAttribute('aria-disabled', 'true');
    await userEvent.click(edit);
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    await userEvent.click(within(card).getByRole('button', { name: 'Delete source: Future input' }));
    expect(await screen.findByRole('dialog', { name: 'Delete this resource?' })).toBeInTheDocument();
  });

  it('deletes through the original resource mutation after confirmation', async () => {
    let deleted = false;
    server.use(http.delete('*/api/v1/sources/camera', ({ request }) => {
      expect(request.headers.get('If-Match')).toBe('"7"');
      deleted = true;
      return new HttpResponse(null, { status: 204 });
    }));
    renderSources();
    const card = await screen.findByTestId('source-card-camera');
    await userEvent.click(within(card).getByRole('button', { name: 'Delete source: Studio camera' }));
    expect(deleted).toBe(false);
    const dialog = await screen.findByRole('dialog', { name: 'Delete this resource?' });
    await userEvent.click(within(dialog).getByRole('button', { name: 'Delete' }));
    await waitFor(() => { expect(deleted).toBe(true); });
  });

  it('retains the empty state and create fast path', async () => {
    server.use(http.get('*/api/v1/sources', () => HttpResponse.json([])));
    renderSources();
    expect(await screen.findByText('No sources configured.')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'New source' }));
    expect(await screen.findByRole('dialog', { name: 'New source' })).toBeInTheDocument();
  });

  it('retains a list error without fabricated cards', async () => {
    server.use(http.get('*/api/v1/sources', () => new HttpResponse(null, { status: 500 })));
    renderSources();
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not load sources:');
    expect(screen.queryByTestId('source-card-camera')).not.toBeInTheDocument();
  });
});
