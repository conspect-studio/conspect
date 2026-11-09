import { afterEach, describe, expect, it, vi } from 'vitest';
import { clearStoredToken, getStoredToken, setStoredToken, subscribeStoredToken } from './token';

const cleanups: (() => void)[] = [];
afterEach(() => {
  for (const cleanup of cleanups.splice(0)) cleanup();
  vi.restoreAllMocks();
  clearStoredToken();
});

describe('stored token subscriptions', () => {
  it('notifies synchronously after writes with no secret payload, including both clear paths', () => {
    const snapshots: (string | undefined)[] = [];
    const notify = vi.fn(() => { snapshots.push(getStoredToken()); });
    cleanups.push(subscribeStoredToken(notify));
    setStoredToken('secret-a');
    setStoredToken('secret-b');
    setStoredToken('');
    setStoredToken('secret-c');
    clearStoredToken();
    expect(snapshots).toEqual(['secret-a', 'secret-b', undefined, 'secret-c', undefined]);
    expect(notify.mock.calls).toEqual([[], [], [], [], []]);
  });

  it('removes only the disposed subscriber and permits repeated cleanup', () => {
    const disposed = vi.fn();
    const live = vi.fn();
    const unsubscribe = subscribeStoredToken(disposed);
    cleanups.push(unsubscribe, subscribeStoredToken(live));
    unsubscribe();
    unsubscribe();
    setStoredToken('secret');
    expect(disposed).not.toHaveBeenCalled();
    expect(live).toHaveBeenCalledExactlyOnceWith();
  });

  it('keeps failed storage writes non-throwing and notifies without exposing the attempted token', () => {
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('disabled'); });
    vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => { throw new Error('disabled'); });
    const notify = vi.fn();
    cleanups.push(subscribeStoredToken(notify));
    expect(() => { setStoredToken('secret'); }).not.toThrow();
    expect(() => { clearStoredToken(); }).not.toThrow();
    expect(notify.mock.calls).toEqual([[], []]);
  });
});
