// InputStreamsInspector: a focused render test over MSW. Verifies the inspector
// stays idle until an input id is entered, then GETs the inventory and renders
// the elementary streams (codec / kind / language) read-only.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { InputStreamsInspector } from './InputStreamsInspector';
import { renderWithProviders } from '../test/render';

let requested = false;

const server = setupServer(
  http.get('http://localhost:3000/api/v1/inputs/:id/streams', () => {
    requested = true;
    return HttpResponse.json({
      input_id: 'cam-north',
      streams: [
        {
          kind: 'video',
          codec: 'h264',
          default: true,
          detail: { detail: 'video', params: { width: 1920, height: 1080 } },
          id: { key: 'v/pid:256', kind_scope: 'v', tier: 'hard' },
        },
        {
          kind: 'audio',
          codec: 'aac',
          default: false,
          detail: { detail: 'audio', params: { channels: 2, sample_rate: 48_000 } },
          id: { key: 'a/pid:257', kind_scope: 'a', tier: 'hard' },
          language: 'eng',
        },
      ],
    });
  }),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  requested = false;
});
afterAll(() => {
  server.close();
});

describe('InputStreamsInspector', () => {
  it('stays idle and does not fetch until an input id is entered', () => {
    renderWithProviders(<InputStreamsInspector />);
    expect(
      screen.getByText(/Enter a configured input id to inspect/i),
    ).toBeInTheDocument();
    expect(requested).toBe(false);
  });

  it('reads and renders the elementary streams once an id is entered', async () => {
    const user = userEvent.setup();
    renderWithProviders(<InputStreamsInspector suggestions={['cam-north']} />);

    await user.type(screen.getByLabelText('Input id'), 'cam-north');

    // The video + audio streams render with their codec + kind.
    expect(await screen.findByText(/h264/)).toBeInTheDocument();
    expect(screen.getByText(/aac/)).toBeInTheDocument();
    expect(screen.getByText('eng')).toBeInTheDocument();
    expect(requested).toBe(true);
  });
});
