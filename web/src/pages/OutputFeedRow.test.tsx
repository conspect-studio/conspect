// OutputFeedRow (ADR-0108 §8): plan → cost line → take → accepted, then the
// verdict only from the correlated `route.output` event; refusals become the
// Take key's visible disabled reason; tally distinguishes PGM vs direct.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import type { Envelope } from '../realtime/envelope';

type Listener = (envelope: Envelope) => void;
const listeners = new Set<Listener>();
vi.mock('../realtime/shared', () => ({
  subscribeRealtime: (_topic: string, listener: Listener): (() => void) => {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
}));

import { renderWithProviders } from '../test/render';
import { OutputFeedRow } from './OutputFeedRow';
import type { OutputRouteState } from '../api/outputRouting';

let planBody: unknown = null;
let takeKey: string | null = null;
let planResponse: Record<string, unknown> = {
  class: 'class1',
  cost: { new_encoders: 1, fits: true },
  refusals: [],
};

const server = setupServer(
  http.post('*/api/v1/routing/plan', async ({ request }) => {
    planBody = await request.json();
    return HttpResponse.json(planResponse);
  }),
  http.post('*/api/v1/routing/output/take', ({ request }) => {
    takeKey = request.headers.get('Idempotency-Key');
    return HttpResponse.json({ operation_id: 'op-7', status: 'accepted', adopt: 'next_tick' }, { status: 202 });
  }),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  listeners.clear();
  planBody = null;
  takeKey = null;
  planResponse = { class: 'class1', cost: { new_encoders: 1, fits: true }, refusals: [] };
});
afterAll(() => {
  server.close();
});

const programRow: OutputRouteState = {
  output: 'out-1',
  video: { bus: 'program' } as unknown as OutputRouteState['video'],
  audio: 'follow' as unknown as OutputRouteState['audio'],
  effective_audio: 'program' as unknown as OutputRouteState['effective_audio'],
  tally: 'program',
  state: 'on_air',
};

const directRow: OutputRouteState = {
  ...programRow,
  video: { source: 'bars' } as unknown as OutputRouteState['video'],
  effective_audio: { source: 'bars' } as unknown as OutputRouteState['effective_audio'],
  tally: 'direct',
};

function emit(data: Record<string, unknown>): void {
  for (const listener of listeners) {
    listener({ v: 1, t: 'route.output', topic: 'outputs', seq: 1, ts: 0, id: 'out-1', data });
  }
}

const choices = [{ key: 'src:bars', label: 'Bars', group: 'generators' as const }];

describe('OutputFeedRow', () => {
  it('plans, shows the cost, takes, and waits for the applied event', async () => {
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={directRow} choices={choices} />,
    );
    const take = screen.getByRole('button', { name: /Take feed to Out 1/ });
    expect(take).toBeDisabled();
    fireEvent.change(screen.getByLabelText(/Video feed for Out 1/), {
      target: { value: 'bus:program' },
    });
    await waitFor(() => {
      expect(screen.getByTestId('output-plan-cost')).toHaveTextContent('+1 encoder');
    });
    expect(planBody).toEqual({
      target: { kind: 'output', output: 'out-1', stream: 'video' },
      feed: { bus: 'program' },
    });
    expect(screen.getByTestId('output-plan-cost')).not.toHaveTextContent('sessions');
    fireEvent.click(screen.getByRole('button', { name: /Take feed to Out 1/ }));
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'pending');
    });
    expect(takeKey).not.toBeNull();
    // An event for another operation changes nothing.
    emit({ output: 'out-1', operation_id: 'op-other', tick: 9, outcome: 'applied' });
    expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'pending');
    emit({ output: 'out-1', operation_id: 'op-7', tick: 10, outcome: 'applied' });
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'applied');
    });
  });

  it('reports a refused outcome as failed with its reason', async () => {
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={directRow} choices={choices} />,
    );
    fireEvent.change(screen.getByLabelText(/Video feed for Out 1/), {
      target: { value: 'bus:program' },
    });
    await screen.findByTestId('output-plan-cost');
    fireEvent.click(screen.getByRole('button', { name: /Take feed to Out 1/ }));
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'pending');
    });
    emit({ output: 'out-1', operation_id: 'op-7', tick: 10, outcome: 'refused:feed-not-fresh' });
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'failed');
    });
    expect(screen.getByRole('status')).toHaveTextContent('refused:feed-not-fresh');
  });

  it('shows a plan refusal as the Take key disabled reason', async () => {
    planResponse = {
      class: 'class1',
      cost: { new_encoders: 1, fits: false },
      refusals: [{ type: 'capacity', detail: 'No free encoder session for this output' }],
    };
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={programRow} choices={choices} />,
    );
    fireEvent.change(screen.getByLabelText(/Video feed for Out 1/), {
      target: { value: 'src:bars' },
    });
    await waitFor(() => {
      expect(screen.getByText('No free encoder session for this output')).toBeInTheDocument();
    });
    expect(screen.getByRole('button', { name: /Take feed to Out 1/ })).toBeDisabled();
  });

  it('starts the select on the feed on air, and moves it on an applied take', async () => {
    const { unmount } = renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={directRow} choices={choices} />,
    );
    const select = screen.getByLabelText(/Video feed for Out 1/);
    expect(select).toHaveValue('src:bars');
    expect(select).toHaveDisplayValue('Bars');
    expect(screen.getByRole('button', { name: /Take feed to Out 1/ })).toBeDisabled();
    fireEvent.change(select, { target: { value: 'bus:program' } });
    await screen.findByTestId('output-plan-cost');
    fireEvent.click(screen.getByRole('button', { name: /Take feed to Out 1/ }));
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'pending');
    });
    emit({ output: 'out-1', operation_id: 'op-7', tick: 10, outcome: 'applied' });
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'applied');
    });
    // Program is now on air: the select shows it and the Take key says so.
    expect(screen.getByLabelText(/Video feed for Out 1/)).toHaveValue('bus:program');
    await waitFor(() => {
      expect(screen.getByRole('button', { name: /Take feed to Out 1/ })).toBeDisabled();
    });
    unmount();
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={programRow} choices={choices} />,
    );
    expect(screen.getByLabelText(/Video feed for Out 1/)).toHaveDisplayValue(
      'Program (follows the switcher)',
    );
  });

  it('names the feed in the cost line; session counts only when the API returns them', async () => {
    // A scoped principal: the API omits encoder_sessions.
    const { unmount } = renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={programRow} choices={choices} />,
    );
    fireEvent.change(screen.getByLabelText(/Video feed for Out 1/), {
      target: { value: 'src:bars' },
    });
    await waitFor(() => {
      expect(screen.getByTestId('output-plan-cost')).toHaveTextContent(/^Bars → \+1 encoder$/);
    });
    unmount();
    // A whole-system principal: the API returns the session count.
    planResponse = {
      class: 'class1',
      cost: { new_encoders: 1, fits: true, encoder_sessions: '6 of 12 in use' },
      refusals: [],
    };
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={programRow} choices={choices} />,
    );
    fireEvent.change(screen.getByLabelText(/Video feed for Out 1/), {
      target: { value: 'src:bars' },
    });
    await waitFor(() => {
      expect(screen.getByTestId('output-plan-cost')).toHaveTextContent(
        'Bars → +1 encoder (6 of 12 encoder sessions in use)',
      );
    });
  });

  it('shows PGM for Program and direct: <id> for a source', () => {
    const { unmount } = renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={programRow} choices={choices} />,
    );
    expect(screen.getByTestId('output-tally')).toHaveTextContent('PGM');
    expect(screen.getByTestId('output-audio')).toHaveTextContent('Follow (program mix)');
    unmount();
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={directRow} choices={choices} />,
    );
    expect(screen.getByTestId('output-tally')).toHaveTextContent('direct: bars');
    expect(screen.getByTestId('output-audio')).toHaveTextContent('Follow (bars)');
  });

  it('offers Clean next to Program and Preview, plans it as the clean bus, and tallies it', async () => {
    const { unmount } = renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={directRow} choices={choices} />,
    );
    const select = screen.getByLabelText(/Video feed for Out 1/);
    const options = Array.from(select.querySelectorAll('option')).map((o) => o.textContent);
    expect(options).toContain('Clean (Program without labels and tally)');
    expect(options.indexOf('Clean (Program without labels and tally)')).toBe(
      options.indexOf('Preview') + 1,
    );
    fireEvent.change(select, { target: { value: 'bus:clean' } });
    await waitFor(() => {
      expect(planBody).toEqual({
        target: { kind: 'output', output: 'out-1', stream: 'video' },
        feed: { bus: 'clean' },
      });
    });
    unmount();
    const cleanRow: OutputRouteState = {
      ...programRow,
      video: { bus: 'clean' } as unknown as OutputRouteState['video'],
      tally: 'clean',
    };
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={cleanRow} choices={choices} />,
    );
    expect(screen.getByTestId('output-tally')).toHaveTextContent('CLEAN');
    expect(screen.getByLabelText(/Video feed for Out 1/)).toHaveValue('bus:clean');
  });

  it('lists layouts in their own group, plans one with its canvas and audio, and shows the layout tally', async () => {
    planResponse = {
      class: 'class1',
      cost: { new_encoders: 1, fits: true, new_aux_canvases: 1, follow_audio: 'program' },
      refusals: [],
    };
    const withLayout = [
      ...choices,
      { key: 'aux:NewsOne', label: 'NewsOne', group: 'layouts' as const },
    ];
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={programRow} choices={withLayout} />,
    );
    const group = screen.getByRole('group', { name: 'Layouts' });
    expect(group).toHaveTextContent('NewsOne');
    fireEvent.change(screen.getByLabelText(/Video feed for Out 1/), {
      target: { value: 'aux:NewsOne' },
    });
    await waitFor(() => {
      expect(screen.getByTestId('output-plan-cost')).toHaveTextContent('+1 encoder, +1 layout canvas');
    });
    expect(planBody).toEqual({
      target: { kind: 'output', output: 'out-1', stream: 'video' },
      feed: { layout: 'NewsOne' },
    });
    expect(screen.getByTestId('output-plan-audio')).toHaveTextContent('Audio follows: program mix');
  });

  it('names an on-air layout in the tally', () => {
    const layoutRow: OutputRouteState = {
      ...programRow,
      video: { layout: 'NewsOne' } as unknown as OutputRouteState['video'],
      tally: 'direct',
    };
    renderWithProviders(
      <OutputFeedRow output="out-1" outputName="Out 1" route={layoutRow} choices={choices} />,
    );
    expect(screen.getByTestId('output-tally')).toHaveTextContent('layout: NewsOne');
    expect(screen.getByLabelText(/Video feed for Out 1/)).toHaveValue('aux:NewsOne');
  });
});
