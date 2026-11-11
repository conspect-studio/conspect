import { describe, expect, it } from 'vitest';
import { observeTick, tickConfirmed } from './tickObservation';

const fresh = () => ({ lastTick: undefined as number | undefined, advancedAt: 0, observations: 0 });
describe('tick observation', () => {
  it('first observation and frozen ticks never confirm', () => {
    const o = fresh();
    observeTick(o, 5, 1000); observeTick(o, 5, 3000); observeTick(o, 5, 5000);
    expect(o.advancedAt).toBe(0);
    expect(tickConfirmed(o.advancedAt, 5000, 5000, false)).toBe(false);
  });
  it('an increasing tick confirms while fresh and expires', () => {
    const o = fresh();
    observeTick(o, 5, 1000); observeTick(o, 6, 3000);
    expect(tickConfirmed(o.advancedAt, 3000, 4000, false)).toBe(true);
    observeTick(o, 6, 9000); // same tick does not refresh
    expect(tickConfirmed(o.advancedAt, 9000, 9500, false)).toBe(false);
    expect(tickConfirmed(o.advancedAt, 3000, 4000, true)).toBe(false);
  });
  it('a tick reset drops confirmation', () => {
    const o = fresh();
    observeTick(o, 5, 1000); observeTick(o, 6, 2000); observeTick(o, 1, 3000);
    expect(o.advancedAt).toBe(0);
  });
});
