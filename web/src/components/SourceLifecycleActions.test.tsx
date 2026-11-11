import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { fireEvent, screen } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { renderWithProviders } from '../test/render';
import { SourceLifecycleActions } from './SourceLifecycleActions';

const calls: string[] = [];
const server = setupServer(
  http.post('*/api/v1/sources/:id/start', ({ params }) => {
    calls.push(`start ${String(params.id)}`);
    return HttpResponse.json({ id: params.id, phase: 'starting' }, { status: 202 });
  }),
  http.post('*/api/v1/sources/:id/stop', ({ params }) => {
    calls.push(`stop ${String(params.id)}`);
    return HttpResponse.json(
      {
        type: '/problems/conflict',
        title: 'Conflict with current state',
        status: 409,
        detail: 'source "cam" is in use by: layout_cell cell-1, always_on always_on',
      },
      { status: 409, headers: { 'Content-Type': 'application/problem+json' } },
    );
  }),
  http.get('*/api/v1/sources/status', () => HttpResponse.json([])),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); calls.length = 0; });
afterAll(() => { server.close(); });

const base = {
  video_wanted: false, audio_wanted: false, video_running: false, audio_running: false,
  activation: 'on_demand' as const,
};

describe('SourceLifecycleActions', () => {
  it('offers Start for an idle source and posts it', async () => {
    renderWithProviders(
      <SourceLifecycleActions id="lazy" name="Lazy" demand={{ ...base, id: 'lazy', phase: 'idle', consumers: [] }} />,
    );
    expect(screen.queryByRole('button', { name: 'Stop Lazy' })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Start Lazy' }));
    await screen.findByRole('button', { name: 'Start Lazy' });
    await expect.poll(() => calls).toEqual(['start lazy']);
  });

  it('shows a refused Stop with every user named', async () => {
    renderWithProviders(
      <SourceLifecycleActions
        id="cam"
        name="Cam"
        demand={{ ...base, id: 'cam', phase: 'ready', video_running: true, consumers: [{ kind: 'layout_cell', label: 'cell-1' }] }}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Stop Cam' }));
    const refused = await screen.findByTestId('source-stop-refused');
    expect(refused).toHaveTextContent('in use by: layout_cell cell-1, always_on always_on');
  });
});
