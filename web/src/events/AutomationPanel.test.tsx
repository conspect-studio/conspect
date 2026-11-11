import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { renderWithProviders } from '../test/render';
import { AutomationPanel } from './AutomationPanel';
import type { AutomationView } from './sourceEvents';

const base = (changes: Partial<AutomationView> = {}): AutomationView => ({
  rule: { armed: false, on_break_start: { action: 'swap_cell', target: 'bars', cells: null }, restore_on_end: true, max_duration_s: 600, webhook_url: null },
  status: { phase: 'disarmed', armed: false, test: false, replaced: [], event_id: null, since_unix_ms: null, audio_replaced: null, last_error: null },
  log: [],
  bars_sources: ['lineup'],
  executor_running: true,
  ...changes,
});
const server = setupServer();
beforeAll(() => { server.listen({ onUnhandledRequest: 'error' }); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });
const endpoint = '*/api/v1/inputs/:id/events/automation';

describe('event automation panel', () => {
  it('requires confirmation before arming', async () => {
    let armed = 0;
    server.use(
      http.get(endpoint, () => HttpResponse.json(base())),
      http.post(`${endpoint}/arm`, () => {
        armed += 1;
        return HttpResponse.json(base({ rule: { ...base().rule!, armed: true }, status: { ...base().status, phase: 'idle', armed: true } }));
      }),
    );
    renderWithProviders(<AutomationPanel id="news" />);
    expect(await screen.findByText('Disarmed — no automatic action will be taken.')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Arm' }));
    expect(armed).toBe(0);
    expect(screen.getByText(/Arm automation\?/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Confirm' }));
    await waitFor(() => { expect(armed).toBe(1); });
  });

  it('shows the live break status, enables Restore now and lists the log', async () => {
    let restored = 0;
    server.use(
      http.get(endpoint, () => HttpResponse.json(base({
        rule: { ...base().rule!, armed: true },
        status: {
          phase: 'in_break', armed: true, test: true, event_id: 'test:break:1', since_unix_ms: 0,
          replaced: [{ cell: 'c1', original: 'news', replacement: 'lineup' }], audio_replaced: null, last_error: null,
        },
        log: [{ at_unix_ms: 0, action: 'take', detail: 'Replaced c1: news → lineup' }],
      }))),
      http.post(`${endpoint}/restore`, () => { restored += 1; return HttpResponse.json(base()); }),
    );
    renderWithProviders(<AutomationPanel id="news" />);
    expect(await screen.findByText(/In break: replaced c1: news → lineup since/)).toBeInTheDocument();
    expect(screen.getByText('Replaced c1: news → lineup')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Restore now' }));
    await waitFor(() => { expect(restored).toBe(1); });
  });

  it('saves an edited rule through PUT', async () => {
    let saved: unknown;
    server.use(
      http.get(endpoint, () => HttpResponse.json(base({ rule: null }))),
      http.put(endpoint, async ({ request }) => { saved = await request.json(); return HttpResponse.json(base()); }),
    );
    renderWithProviders(<AutomationPanel id="news" />);
    await userEvent.click(await screen.findByText(/^Rule/));
    const target = screen.getByLabelText('Replacement');
    await userEvent.clear(target);
    await userEvent.type(target, 'bars_tone');
    await userEvent.click(screen.getByRole('button', { name: 'Save rule' }));
    await waitFor(() => { expect(saved).toMatchObject({ rule: { armed: false, on_break_start: { target: 'bars_tone', action: 'swap_cell' } } }); });
  });
});
