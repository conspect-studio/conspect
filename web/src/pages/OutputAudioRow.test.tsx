// OutputAudioRow (ADR-0108 §8 "Audio [ Follow (program mix) ▾ ]"): the current
// value selected, plan (`stream: audio`) → audio cost line, a refusal as the
// Take key's disabled reason, and the audio-only take body.
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
import { OutputAudioRow } from './OutputAudioRow';
import type { OutputRouteState } from '../api/outputRouting';

let planBody: unknown = null;
let takeBody: unknown = null;
let takeKey: string | null = null;
let planResponse: Record<string, unknown> = {
  class: 'class1',
  cost: { new_encoders: 0, fits: true, audio_encoders: 1, audio_cost: '+1 audio encode (transient, CPU)' },
  refusals: [],
};

const server = setupServer(
  http.post('*/api/v1/routing/plan', async ({ request }) => {
    planBody = await request.json();
    return HttpResponse.json(planResponse);
  }),
  http.post('*/api/v1/routing/output/take', async ({ request }) => {
    takeKey = request.headers.get('Idempotency-Key');
    takeBody = await request.json();
    return HttpResponse.json({ operation_id: 'op-a', status: 'accepted', adopt: 'next_tick' }, { status: 202 });
  }),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  listeners.clear();
  planBody = null;
  takeBody = null;
  takeKey = null;
  planResponse = {
    class: 'class1',
    cost: { new_encoders: 0, fits: true, audio_encoders: 1, audio_cost: '+1 audio encode (transient, CPU)' },
    refusals: [],
  };
});
afterAll(() => {
  server.close();
});

const followRow: OutputRouteState = {
  output: 'out-1',
  video: { bus: 'program' } as unknown as OutputRouteState['video'],
  configured_video: { bus: 'program' } as unknown as OutputRouteState['configured_video'],
  audio: 'follow' as unknown as OutputRouteState['audio'],
  effective_audio: 'program' as unknown as OutputRouteState['effective_audio'],
  tally: 'program',
  state: 'on_air',
};

const sources = [
  { id: 'x', label: 'Camera X' },
  { id: 'bars', label: 'Bars' },
];

function emit(data: Record<string, unknown>): void {
  for (const listener of listeners) {
    listener({ v: 1, t: 'route.output', topic: 'outputs', seq: 1, ts: 0, id: 'out-1', data });
  }
}

describe('OutputAudioRow', () => {
  it('renders the current value selected, labelled with what follow carries', () => {
    const { unmount } = renderWithProviders(
      <OutputAudioRow output="out-1" outputName="Out 1" route={followRow} sources={sources} tracks={['eng']} />,
    );
    const select = screen.getByLabelText(/Audio feed for Out 1/);
    expect(select).toHaveValue('follow');
    expect(select).toHaveDisplayValue('Follow (program mix)');
    const options = Array.from(select.querySelectorAll('option')).map((o) => o.textContent);
    expect(options).toEqual(['Follow (program mix)', 'Program mix', 'Silence', 'Camera X', 'Bars', 'eng']);
    unmount();
    const onX: OutputRouteState = {
      ...followRow,
      audio: { source: 'x' } as unknown as OutputRouteState['audio'],
      effective_audio: { source: 'x' } as unknown as OutputRouteState['effective_audio'],
    };
    renderWithProviders(
      <OutputAudioRow output="out-1" outputName="Out 1" route={onX} sources={sources} tracks={undefined} />,
    );
    expect(screen.getByLabelText(/Audio feed for Out 1/)).toHaveValue('src:x');
    expect(screen.getByTestId('output-audio-no-tracks')).toBeInTheDocument();
  });

  it('selects and labels what is on air (effective_audio), noting a different configured audio', () => {
    // Configured `{source: x}`, but the engine carries the program mix (a
    // refused start-up take): the select shows the live value.
    const drifted: OutputRouteState = {
      ...followRow,
      audio: { source: 'x' } as unknown as OutputRouteState['audio'],
      effective_audio: 'program' as unknown as OutputRouteState['effective_audio'],
    };
    const { unmount } = renderWithProviders(
      <OutputAudioRow output="out-1" outputName="Out 1" route={drifted} sources={sources} tracks={['eng']} />,
    );
    const select = screen.getByLabelText(/Audio feed for Out 1/);
    expect(select).toHaveValue('program');
    expect(screen.getByTestId('output-audio')).toHaveTextContent('Program mix');
    expect(screen.getByTestId('output-configured-audio')).toHaveTextContent('configured: Camera X (not on air)');
    unmount();
    // On air a track and a source the page does not list; configured `none`.
    for (const [effective, key, label] of [
      [{ tracks: ['eng'] }, 'track:eng', 'eng'],
      [{ source: 'cam9' }, 'src:cam9', 'cam9'],
      ['none', 'none', 'Silence'],
    ] as const) {
      const row: OutputRouteState = {
        ...followRow,
        audio: (key === 'none' ? 'program' : 'none') as unknown as OutputRouteState['audio'],
        effective_audio: effective as unknown as OutputRouteState['effective_audio'],
      };
      const r = renderWithProviders(
        <OutputAudioRow output="out-1" outputName="Out 1" route={row} sources={sources} tracks={['eng']} />,
      );
      expect(screen.getByLabelText(/Audio feed for Out 1/)).toHaveValue(key);
      expect(screen.getByTestId('output-audio')).toHaveTextContent(label);
      expect(screen.getByTestId('output-configured-audio')).toHaveTextContent(/configured: .* \(not on air\)/);
      r.unmount();
    }
    // Configured `follow` carrying what follow resolves to: still `follow`,
    // no note; `follow` on Preview carries the PVW source.
    for (const [video, effective] of [
      [{ source: 'x' }, { source: 'x' }],
      [{ bus: 'preview' }, 'preview_source'],
    ] as const) {
      const row: OutputRouteState = {
        ...followRow,
        video: video as unknown as OutputRouteState['video'],
        effective_audio: effective as unknown as OutputRouteState['effective_audio'],
      };
      const r = renderWithProviders(
        <OutputAudioRow output="out-1" outputName="Out 1" route={row} sources={sources} tracks={['eng']} />,
      );
      expect(screen.getByLabelText(/Audio feed for Out 1/)).toHaveValue('follow');
      expect(screen.queryByTestId('output-configured-audio')).toBeNull();
      r.unmount();
    }
  });

  it('hides the current value when effective_audio is redacted, never falling back to the configured one (PR #352 round 2)', () => {
    // Configured a readable `{source: x}`, but the engine's effective_audio
    // is redacted (the row's video is also hidden to this caller): the
    // select must be empty/disabled, the label "Hidden", and "x" must never
    // appear as the current value.
    const hidden: OutputRouteState = {
      ...followRow,
      audio: { source: 'secret' } as unknown as OutputRouteState['audio'],
      effective_audio: { redacted: true } as unknown as OutputRouteState['effective_audio'],
    };
    renderWithProviders(
      <OutputAudioRow output="out-1" outputName="Out 1" route={hidden} sources={sources} tracks={['eng']} />,
    );
    const select = screen.getByLabelText(/Audio feed for Out 1/);
    expect(select).toBeDisabled();
    expect(select).toHaveValue('');
    expect(screen.getByTestId('output-audio')).toHaveTextContent('Hidden');
    expect(screen.queryByTestId('output-configured-audio')).toBeNull();
    expect(screen.queryByText('secret')).toBeNull();
  });

  it('plans stream audio, shows the cost line, and posts the audio-only take', async () => {
    renderWithProviders(
      <OutputAudioRow output="out-1" outputName="Out 1" route={followRow} sources={sources} tracks={['eng']} />,
    );
    const take = screen.getByRole('button', { name: /Take audio to Out 1/ });
    expect(take).toBeDisabled();
    fireEvent.change(screen.getByLabelText(/Audio feed for Out 1/), { target: { value: 'src:x' } });
    await waitFor(() => {
      expect(screen.getByTestId('output-audio-plan-cost')).toHaveTextContent(
        'Camera X → +1 audio encode (transient, CPU)',
      );
    });
    expect(planBody).toEqual({
      target: { kind: 'output', output: 'out-1', stream: 'audio' },
      audio: { source: 'x' },
    });
    fireEvent.click(screen.getByRole('button', { name: /Take audio to Out 1/ }));
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'pending');
    });
    expect(takeBody).toEqual({
      target: { kind: 'output', output: 'out-1', stream: 'audio' },
      audio: { source: 'x' },
    });
    expect(takeKey).not.toBeNull();
    emit({ output: 'out-1', operation_id: 'op-a', tick: 4, outcome: 'applied' });
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveAttribute('data-phase', 'applied');
    });
    expect(screen.getByLabelText(/Audio feed for Out 1/)).toHaveValue('src:x');
  });

  it('says "No extra encoder" when the plan costs none', async () => {
    planResponse = { class: 'class1', cost: { new_encoders: 0, fits: true, audio_encoders: 0 }, refusals: [] };
    renderWithProviders(
      <OutputAudioRow output="out-1" outputName="Out 1" route={followRow} sources={sources} tracks={['eng']} />,
    );
    fireEvent.change(screen.getByLabelText(/Audio feed for Out 1/), { target: { value: 'none' } });
    await waitFor(() => {
      expect(screen.getByTestId('output-audio-plan-cost')).toHaveTextContent('Silence → No extra encoder');
    });
  });

  it('shows a plan refusal as the Take key disabled reason', async () => {
    planResponse = {
      class: 'class1',
      cost: { new_encoders: 0, fits: true, audio_encoders: 0 },
      refusals: [{ type: 'audio-unsupported', detail: 'This output cannot carry a routed audio feed' }],
    };
    renderWithProviders(
      <OutputAudioRow output="out-1" outputName="Out 1" route={followRow} sources={sources} tracks={['eng']} />,
    );
    fireEvent.change(screen.getByLabelText(/Audio feed for Out 1/), { target: { value: 'track:eng' } });
    await waitFor(() => {
      expect(screen.getByText('This output cannot carry a routed audio feed')).toBeInTheDocument();
    });
    expect(planBody).toEqual({
      target: { kind: 'output', output: 'out-1', stream: 'audio' },
      audio: { tracks: ['eng'] },
    });
    expect(screen.getByRole('button', { name: /Take audio to Out 1/ })).toBeDisabled();
  });
});
