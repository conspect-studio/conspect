import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import type { IndexStream } from '../api/catalogue';
import index from '../test/fixtures/iptv-org/index.json';
import { renderWithProviders } from '../test/render';
import { LARGE_ALWAYS_ON } from './ActivationChoice';
import { BulkAddDialog } from './BulkAddDialog';

// Real fixture rows; only the key and URL are varied to make enough distinct
// streams for the batch sizes under test.
const base = (index as unknown as { streams: IndexStream[] }).streams.find((s) => s.proto === 'hls')!;
const streams = (n: number): IndexStream[] =>
  Array.from({ length: n }, (_, i) => ({
    ...base,
    k: `s:cap${String(i)}`,
    url: `${base.url}?n=${String(i)}`,
    title: `${base.title} ${String(i)}`,
  }));

interface SentEntry {
  readonly id: string;
  readonly body: { readonly activation?: string };
}

/** Every batch the dialog successfully POSTed, in order. */
let sent: SentEntry[][] = [];
const server = setupServer(
  http.post('*/api/v1/catalogues/iptv-org/sources', async ({ request }) => {
    const { sources } = (await request.json()) as { sources: SentEntry[] };
    sent.push(sources);
    return HttpResponse.json(
      { results: sources.map((x) => ({ id: x.id, status: 'created' })) },
      { status: 201 },
    );
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); sent = []; });
afterAll(() => { server.close(); });

const open = (n: number): void => {
  renderWithProviders(
    <BulkAddDialog open streams={streams(n)} takenIds={new Set()} onClose={vi.fn()} onDone={vi.fn()} />,
  );
};

const addButton = (dialog: HTMLElement): HTMLElement =>
  within(dialog).getByRole('button', { name: /add \d+ sources/i });
const onDemand = (dialog: HTMLElement): HTMLElement =>
  within(dialog).getByRole('radio', { name: /on demand: idle and free/i });
const alwaysOn = (dialog: HTMLElement): HTMLElement =>
  within(dialog).getByRole('radio', { name: /always on: each opens a connection/i });

describe('BulkAddDialog activation choice', () => {
  it('preselects nothing and refuses to add until a choice is made', async () => {
    open(2);
    const dialog = await screen.findByRole('dialog');
    expect(onDemand(dialog)).not.toBeChecked();
    expect(alwaysOn(dialog)).not.toBeChecked();
    const add = addButton(dialog);
    expect(add).toBeDisabled();
    await userEvent.click(add);
    expect(sent).toHaveLength(0);
  });

  it('sends the on-demand choice in every created body', async () => {
    open(2);
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(onDemand(dialog));
    await userEvent.click(addButton(dialog));
    await within(dialog).findAllByText(/created/i);
    expect(sent).toHaveLength(1);
    expect(sent[0]).toHaveLength(2);
    for (const entry of sent[0]!) {
      expect(entry.body.activation).toBe('on_demand');
    }
  });

  it('sends an explicit always-on choice in every created body', async () => {
    open(2);
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(alwaysOn(dialog));
    await userEvent.click(addButton(dialog));
    await within(dialog).findAllByText(/created/i);
    expect(sent).toHaveLength(1);
    expect(sent[0]).toHaveLength(2);
    for (const entry of sent[0]!) {
      expect(entry.body.activation).toBe('always_on');
    }
  });

  it(`warns once ${String(LARGE_ALWAYS_ON)}+ always-on sources are chosen`, async () => {
    open(LARGE_ALWAYS_ON + 1);
    const dialog = await screen.findByRole('dialog');
    // Nothing chosen yet: no warning, and no silent always-on default.
    expect(within(dialog).queryByRole('alert')).toBeNull();
    await userEvent.click(alwaysOn(dialog));
    expect(within(dialog).getByRole('alert')).toHaveTextContent(
      new RegExp(`${String(LARGE_ALWAYS_ON + 1)} always-on sources`),
    );
    // On demand lifts the warning: those sources cost nothing until used.
    await userEvent.click(onDemand(dialog));
    expect(within(dialog).queryByRole('alert')).toBeNull();
  });

  it(`does not warn for an always-on batch of ${String(LARGE_ALWAYS_ON)}`, async () => {
    open(LARGE_ALWAYS_ON);
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(alwaysOn(dialog));
    expect(within(dialog).queryByRole('alert')).toBeNull();
  });

  it('accepts a batch above the old cap of 8 when it is on demand', async () => {
    open(9);
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(onDemand(dialog));
    await userEvent.click(addButton(dialog));
    await within(dialog).findAllByText(/created/i);
    expect(sent).toHaveLength(1);
    expect(sent[0]).toHaveLength(9);
    expect(sent[0]!.every((entry) => entry.body.activation === 'on_demand')).toBe(true);
  });

  it('reports a server-side refusal (422) as failed for every item', async () => {
    server.use(
      http.post('*/api/v1/catalogues/iptv-org/sources', () =>
        HttpResponse.json(
          { title: 'above 8 sources every entry must state its `activation`' },
          { status: 422 },
        ),
      ),
    );
    open(2);
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(onDemand(dialog));
    await userEvent.click(addButton(dialog));
    expect(await within(dialog).findAllByText(/must state its/i)).toHaveLength(2);
    expect(sent).toHaveLength(0);
  });
});
