import { StrictMode, useLayoutEffect } from 'react';
import { act, cleanup, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { clearStoredToken, setStoredToken } from '../../api/token';
import { getOperationTracker, OPERATION_TRACKER_QUERY_KEY } from './operationTracker';
import { useOperationFeedback } from './useOperationFeedback';

const expectation = { topic: 'switcher' as const, resource: 'p1', event: 'media.player_state' as const,
  terminal: { kind: 'player-state' as const, state: 'playing' as const } };
const accepted = { operation_id: 'private-op', kind: 'media_play' };

async function seed(client: QueryClient) {
  setStoredToken('actor-a');
  const tracker = getOperationTracker(client);
  await tracker.submit(() => Promise.resolve(accepted), expectation);
  tracker.onEnvelope({ v: 1, t: 'media.player_state', topic: 'switcher', id: 'p1', seq: 1, ts: 1,
    corr: accepted.operation_id, data: { player: 'p1', position_frames: 0, state: { kind: 'playing' } } });
  return tracker;
}

function Probe({ commits }: { commits: (string | undefined)[] }) {
  const record = useOperationFeedback(accepted.operation_id);
  useLayoutEffect(() => { commits.push(record?.phase); });
  return <span>{record?.phase ?? 'hidden'}</span>;
}
const view = (client: QueryClient, commits: (string | undefined)[]) =>
  <StrictMode><QueryClientProvider client={client}><Probe commits={commits} /></QueryClientProvider></StrictMode>;

afterEach(() => { cleanup(); clearStoredToken(); vi.restoreAllMocks(); });

describe('credential-safe shared operation feedback', () => {
  it('keeps snapshots stable and pure when the credential changes', async () => {
    const client = new QueryClient();
    const tracker = await seed(client);
    expect(tracker.snapshot()).toBe(tracker.snapshot());
    const cached = client.getQueryData(OPERATION_TRACKER_QUERY_KEY);
    const publish = vi.spyOn(client, 'setQueryData');
    setStoredToken('actor-b');
    expect(tracker.snapshot().records.size).toBe(0);
    expect(tracker.snapshot()).toBe(tracker.snapshot());
    expect(client.getQueryData(OPERATION_TRACKER_QUERY_KEY)).toBe(cached);
    expect(publish).not.toHaveBeenCalled();
  });

  it('hides cached evidence on the first remount commit before passive effects', async () => {
    const client = new QueryClient();
    await seed(client);
    const oldCommits: (string | undefined)[] = [];
    const mounted = render(view(client, oldCommits));
    expect(screen.getByText('applied')).toBeInTheDocument();
    mounted.unmount();
    setStoredToken('actor-b');
    const commits: (string | undefined)[] = [];
    render(view(client, commits));
    expect(commits.length).toBeGreaterThan(0);
    expect(commits.every((phase) => phase === undefined)).toBe(true);
    expect(screen.getByText('hidden')).toBeInTheDocument();
  });

  it.each(['same-document', 'storage', 'clear'] as const)('updates a quiet mounted consumer on %s changes', async (source) => {
    const client = new QueryClient();
    await seed(client);
    const commits: (string | undefined)[] = [];
    render(view(client, commits));
    expect(screen.getByText('applied')).toBeInTheDocument();
    commits.length = 0;
    act(() => {
      if (source === 'same-document') setStoredToken('actor-b');
      else if (source === 'clear') clearStoredToken();
      else {
        localStorage.setItem('multiview.apiToken', 'actor-b');
        window.dispatchEvent(new StorageEvent('storage', { key: 'multiview.apiToken' }));
      }
    });
    expect(screen.getByText('hidden')).toBeInTheDocument();
    expect(commits.length).toBeGreaterThan(0);
    expect(commits.every((phase) => phase === undefined)).toBe(true);
  });

  it('does not publish or warn across components during a transition render', async () => {
    const client = new QueryClient();
    await seed(client);
    const commits: (string | undefined)[] = [];
    const mounted = render(view(client, commits));
    // A token write without notification models the interval before a storage event.
    localStorage.setItem('multiview.apiToken', 'actor-b');
    const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const publish = vi.spyOn(client, 'setQueryData');
    function NewConsumer() {
      const before = publish.mock.calls.length;
      const record = useOperationFeedback(accepted.operation_id);
      expect(publish.mock.calls.length).toBe(before);
      return <span>{record?.phase ?? 'new-hidden'}</span>;
    }
    mounted.rerender(<QueryClientProvider client={client}><Probe commits={commits} /><NewConsumer /></QueryClientProvider>);
    expect(screen.getByText('new-hidden')).toBeInTheDocument();
    expect(errors).not.toHaveBeenCalled();
  });

  it('preserves evidence across navigation with the same credential', async () => {
    const client = new QueryClient();
    await seed(client);
    const first = render(view(client, []));
    first.unmount();
    render(view(client, []));
    expect(screen.getByText('applied')).toBeInTheDocument();
  });

  it('observes new-credential evidence after the reset without an extra socket', async () => {
    const client = new QueryClient();
    const tracker = await seed(client);
    render(view(client, []));
    act(() => { setStoredToken('actor-b'); });
    expect(screen.getByText('hidden')).toBeInTheDocument();
    await act(async () => { await tracker.submit(() => Promise.resolve(accepted), expectation); });
    expect(screen.getByText('accepted')).toBeInTheDocument();
    act(() => { tracker.onEnvelope({ v: 1, t: 'media.player_state', topic: 'switcher', id: 'p1', seq: 2, ts: 2,
      corr: accepted.operation_id, data: { player: 'p1', position_frames: 0, state: { kind: 'playing' } } }); });
    expect(screen.getByText('applied')).toBeInTheDocument();
  });

  it('rejects an earlier generation even when the original credential returns', async () => {
    const client = new QueryClient();
    setStoredToken('actor-a');
    const tracker = getOperationTracker(client);
    render(view(client, []));
    let resolve!: (body: typeof accepted) => void;
    let pending!: ReturnType<typeof tracker.submit>;
    act(() => {
      pending = tracker.submit(() => new Promise<typeof accepted>((done) => { resolve = done; }), expectation);
      setStoredToken('actor-b');
      setStoredToken('actor-a');
    });
    await act(async () => {
      resolve(accepted);
      await expect(pending).resolves.toEqual({ kind: 'accepted-unobserved', operationId: accepted.operation_id });
    });
    expect(screen.getByText('hidden')).toBeInTheDocument();
    expect(tracker.snapshot().provisional.size).toBe(0);
  });

  it('does not admit an in-flight response after a quiet credential change', async () => {
    const client = new QueryClient();
    setStoredToken('actor-a');
    const tracker = getOperationTracker(client);
    let resolve!: (body: typeof accepted) => void;
    const pending = tracker.submit(() => new Promise<typeof accepted>((done) => { resolve = done; }), expectation);
    setStoredToken('actor-b');
    resolve(accepted);
    await expect(pending).resolves.toEqual({ kind: 'accepted-unobserved', operationId: accepted.operation_id });
    expect(tracker.snapshot().records.size).toBe(0);
    expect(tracker.snapshot().provisional.size).toBe(0);
  });
});
