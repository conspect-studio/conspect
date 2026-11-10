import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

vi.mock('../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../locales/en/messages';
import { SwitcherPage } from './SwitcherPage';
import { getOperationTracker } from '../components/broadcast/operationTracker';
import type { Envelope } from '../realtime/envelope';

i18n.load('en', messages);
i18n.activate('en');

const state = { program_source: 'cam-1', preview_source: 'cam-2', tick: 7, in_flight: false };
let commandCount = 0;
const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => HttpResponse.json(state)),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.post('*/api/v1/switcher/plan', () => HttpResponse.json({ can_take: true, class: 'cut', scene_version: 1 })),
  http.post('*/api/v1/switcher/commands/cut', () => {
    commandCount += 1;
    return HttpResponse.json({ operation_id: `op-${String(commandCount)}`, kind: 'cut' }, { status: 202 });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); commandCount = 0; vi.useRealTimers(); });
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const view = render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><SwitcherPage /></MemoryRouter></I18nProvider></QueryClientProvider>);
  return { client, ...view };
}
function frame(id: string, data: unknown, override: Partial<Envelope> = {}): Envelope {
  return { v: 1, t: 'switcher.operation', topic: 'switcher', id: 'me1', seq: 1, ts: 1, corr: id, data, ...override };
}
function outcome(id: string, completed: boolean, error?: string) {
  return { operation_id: id, me: 'me1', program: 'main', tick: 8, completed, ...(error ? { error } : {}) };
}

// Tracker events are supplied as the shell-owned socket supplies them; no second socket.
describe('SwitcherPage single-authority completion', () => {
  it('does not clear awaiting state on unrelated topic, M/E, corr or media loading', async () => {
    const { client } = page();
    await userEvent.click(await screen.findByRole('button', { name: 'Cut' }));
    await waitFor(() => { expect(commandCount).toBe(1); });
    const tracker = getOperationTracker(client);
    for (const event of [
      frame('op-1', outcome('op-1', true), { topic: 'outputs' }),
      frame('op-1', outcome('op-1', true), { id: 'me2' }),
      frame('wrong', outcome('wrong', true)),
      frame('op-1', { player: 'me1', state: { kind: 'loading' } }, { t: 'media.player_state' }),
    ]) act(() => { tracker.onEnvelope(event); });
    expect(screen.getByText(/waiting for the applied event/i)).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Cut' })).toHaveProperty('disabled', true);
    act(() => { tracker.onEnvelope(frame('op-1', outcome('op-1', false, 'engine rejected'))); });
    await waitFor(() => { expect(screen.getByText(/Switcher failed/i)).toBeTruthy(); });
    expect(screen.getByRole('button', { name: 'Cut' })).toHaveProperty('disabled', false);
  });

  it('keeps unknown outcome visibly unresolved and prevents a second cut', async () => {
    const { client } = page();
    await userEvent.click(await screen.findByRole('button', { name: 'Cut' }));
    await waitFor(() => { expect(commandCount).toBe(1); });
    act(() => { getOperationTracker(client).onGap(); });
    await waitFor(() => { expect(screen.getByText(/Switcher outcome unknown/i)).toBeTruthy(); });
    expect(screen.getByRole('button', { name: 'Cut' })).toHaveProperty('disabled', true);
    expect(commandCount).toBe(1);
  });

  it('accepts exact completed true output-tick event and allows the next cut', async () => {
    const { client } = page();
    await userEvent.click(await screen.findByRole('button', { name: 'Cut' }));
    await waitFor(() => { expect(commandCount).toBe(1); });
    act(() => { getOperationTracker(client).onEnvelope(frame('op-1', outcome('op-1', true))); });
    await waitFor(() => { expect(screen.getByText(/Switcher applied/i)).toBeTruthy(); });
    expect(screen.getByRole('button', { name: 'Cut' })).toHaveProperty('disabled', false);
  });
});
