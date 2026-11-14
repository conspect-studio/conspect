// Runtime guards for the patch API (E3-07): every nested field the matrix
// dereferences is validated, so a malformed body is an error, not a crash.
import { describe, expect, it } from 'vitest';
import { isPatchPlanResponse, parseCrosspoints } from './patch';

describe('isPatchPlanResponse', () => {
  const good = {
    can_take: true, class: 'class2',
    crosspoints: [{ destination: { output: 'a', slot: 0 }, path: 'P1', on_air: false, refusal: null }],
  };
  it('accepts a well-formed plan and set plan', () => {
    expect(isPatchPlanResponse(good)).toBe(true);
    expect(isPatchPlanResponse({ ...good, set: { new_encoders: 1, fits: true } })).toBe(true);
  });
  it('rejects crosspoints: [null]', () => {
    expect(isPatchPlanResponse({ ...good, crosspoints: [null] })).toBe(false);
  });
  it('rejects a member without destination/path/on_air or with a malformed refusal', () => {
    expect(isPatchPlanResponse({ ...good, crosspoints: [{ path: 'P1', on_air: false }] })).toBe(false);
    expect(isPatchPlanResponse({ ...good, crosspoints: [{ destination: {}, path: 'P1', on_air: false }] })).toBe(false);
    expect(isPatchPlanResponse({ ...good, crosspoints: [{ destination: { output: 'a' }, path: 'P1', on_air: false, refusal: { type: 'x' } }] })).toBe(false);
  });
  it('rejects a plan with no class', () => {
    expect(isPatchPlanResponse({ can_take: true, crosspoints: [] })).toBe(false);
  });
});

describe('parseCrosspoints', () => {
  it('normalizes stored members and counts malformed ones', () => {
    expect(parseCrosspoints([
      { plane: 'video', destination: { output: 'a', slot: 0 }, source: { source: 'x' } },
      { plane: 'video', destination: { mixer: 'me1', input: 2 }, source: 'none' },
      null,
      { plane: 'video', destination: {}, source: 'none' },
    ])).toEqual({
      crosspoints: [
        { plane: 'video', destination: { output: 'a', slot: 0 }, source: { source: 'x' } },
        { plane: 'video', destination: { mixer: 'me1', input: 2 }, source: 'none' },
      ],
      invalid: 2,
    });
  });
});
