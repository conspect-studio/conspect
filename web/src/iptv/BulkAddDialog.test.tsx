import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import type { IndexStream } from '../api/catalogue';
import index from '../test/fixtures/iptv-org/index.json';
import { renderWithProviders } from '../test/render';
import { BULK_ADD_MAX, BulkAddDialog } from './BulkAddDialog';

// Real fixture rows; only the key and URL are varied to make enough distinct
// streams for the cap.
const base = (index as unknown as { streams: IndexStream[] }).streams.find((s) => s.proto === 'hls')!;
const streams = (n: number): IndexStream[] =>
  Array.from({ length: n }, (_, i) => ({
    ...base,
    k: `s:cap${String(i)}`,
    url: `${base.url}?n=${String(i)}`,
    title: `${base.title} ${String(i)}`,
  }));

const created: string[] = [];
const server = setupServer(
  http.post('*/api/v1/sources/:id', ({ params }) => {
    created.push(String(params.id));
    return HttpResponse.json({ id: params.id, name: 'x', body: {} }, { status: 201 });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); created.length = 0; });
afterAll(() => { server.close(); });

const open = (n: number): void => {
  renderWithProviders(
    <BulkAddDialog open streams={streams(n)} takenIds={new Set()} onClose={vi.fn()} onDone={vi.fn()} />,
  );
};

describe('BulkAddDialog cap (sources start always on until activation lands)', () => {
  it('allows a bulk add at the cap and creates every source', async () => {
    open(BULK_ADD_MAX);
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/each opens a connection and decoder/i)).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('button', { name: new RegExp(`add ${String(BULK_ADD_MAX)} sources`, 'i') }));
    await within(dialog).findAllByText(/created/i);
    expect(created).toHaveLength(BULK_ADD_MAX);
  });

  it('refuses a bulk add over the cap, says why, and creates nothing', async () => {
    open(BULK_ADD_MAX + 1);
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByRole('alert')).toHaveTextContent(
      new RegExp(`at most ${String(BULK_ADD_MAX)}`, 'i'),
    );
    const add = within(dialog).getByRole('button', { name: /add \d+ sources/i });
    expect(add).toBeDisabled();
    await userEvent.click(add);
    expect(created).toHaveLength(0);
  });
});
