// AlarmsPage: a focused render test over MSW. Verifies the page reads the alarm
// list and renders the severity + kind as text, and that the Acknowledge button
// POSTs the ack and reflects the acknowledged state after a refetch.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { AlarmsPage } from './AlarmsPage';
import { renderWithProviders } from '../test/render';

let acked = false;

const server = setupServer(
  http.get('http://localhost:3000/api/v1/alarms', () =>
    HttpResponse.json([
      {
        id: 'a1',
        kind: 'Black',
        severity: 'Major',
        scope: { kind: 'tile', index: 2 },
        raised_at: 0,
        dwell: 0,
        latched: true,
        ack: acked
          ? { state: 'Acked', who: 'op', when: 1 }
          : { state: 'Unacked' },
      },
    ]),
  ),
  http.post('http://localhost:3000/api/v1/alarms/:id/ack', () => {
    acked = true;
    return HttpResponse.json(
      {
        id: 'a1',
        kind: 'Black',
        severity: 'Major',
        scope: { kind: 'tile', index: 2 },
        raised_at: 0,
        dwell: 0,
        latched: true,
        ack: { state: 'Acked', who: 'op', when: 1 },
      },
      { headers: { ETag: 'W/"2"' } },
    );
  }),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  acked = false;
});
afterAll(() => {
  server.close();
});

describe('AlarmsPage', () => {
  it('lists an alarm and acknowledges it', async () => {
    const user = userEvent.setup();
    renderWithProviders(<AlarmsPage />);

    // The alarm's kind + severity render as text.
    expect(await screen.findByText('Black')).toBeInTheDocument();
    expect(screen.getByText('Major')).toBeInTheDocument();
    expect(screen.getByText('tile #2')).toBeInTheDocument();

    const ackButton = await screen.findByRole('button', {
      name: /Acknowledge alarm: Black/i,
    });
    // Before the ack, the row reports the alarm is unacknowledged.
    expect(screen.getByText('Unacknowledged')).toBeInTheDocument();
    await user.click(ackButton);

    // After the ack refetch, the row no longer shows "Unacknowledged" and the
    // Acknowledge button is disabled (the alarm is acknowledged).
    await waitFor(() => {
      expect(screen.queryByText('Unacknowledged')).not.toBeInTheDocument();
    });
    expect(
      screen.getByRole('button', { name: /Acknowledge alarm: Black/i }),
    ).toBeDisabled();
  });
});
