// #419 item 5: Start (via the failed-row Retry button) surfaces a 409 problem
// `detail` (e.g. "applies on restart" for a stored-not-applied source) rather
// than a generic failure. `SourceLifecycleActions` already did this; this
// covers the `RetryButton` path used from the failed-source callout.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SourcesSurface } from './SourcesSurface';
import type { SourceDemand } from '../../api/sourceStatus';
import type { SourceView } from '../../resources/types';
import { renderWithProviders } from '../../test/render';

const rows: SourceView[] = [
  {
    id: 'cam',
    name: 'Cam',
    kind: 'rtsp',
    rawKind: 'rtsp',
    editable: true,
    locator: 'rtsp://cam.test',
    eventObservation: 'not_supported',
  },
];
const status = new Map<string, SourceDemand>([
  [
    'cam',
    {
      id: 'cam',
      phase: 'failed',
      consumers: [],
      video_running: false,
      audio_running: false,
      video_wanted: false,
      audio_wanted: false,
      activation: 'on_demand',
      detail: 'ingest ended remotely',
    },
  ],
]);

const server = setupServer(
  http.post('*/api/v1/sources/:id/start', () =>
    HttpResponse.json(
      {
        type: '/problems/conflict',
        title: 'Conflict with current state',
        status: 409,
        detail: 'source "cam" applies on restart',
      },
      { status: 409, headers: { 'Content-Type': 'application/problem+json' } },
    ),
  ),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });

describe('SourcesSurface Retry — 409 detail surfaced', () => {
  it('shows the problem detail text, not a generic failure', async () => {
    renderWithProviders(
      <MemoryRouter>
        <SourcesSurface
          rows={rows}
          demand={status}
          tileFor={() => undefined}
          linkFor={() => undefined}
          onEdit={() => undefined}
          onDelete={() => undefined}
        />
      </MemoryRouter>,
    );
    const box = screen.getByTestId('source-failure-box-cam');
    await userEvent.click(within(box).getByRole('button', { name: 'Retry Cam' }));
    const refusal = await within(box).findByTestId('source-retry-refused-cam');
    expect(refusal).toHaveTextContent('source "cam" applies on restart');
  });
});
