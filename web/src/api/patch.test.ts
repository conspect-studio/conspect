// Runtime guards for the patch API (E3-07): every nested field the matrix
// dereferences is validated, so a malformed body is an error, not a crash.
import { describe, expect, it } from 'vitest';
import { isPatchPlanResponse, isPatchView, parseCrosspoints } from './patch';

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

describe('isPatchView (E3-07 round 2)', () => {
  const input = { number: 1, label: 'in1', video: { source: 'x' }, on_program: false, on_preview: false };
  const view = (inputs: unknown[]) => ({ mixer: [{ me: 'me1', inputs, inputs_version: 1, tick: 3 }], outputs: [] });
  it('accepts a patched, redacted or unpatched (null) input', () => {
    expect(isPatchView(view([input, { ...input, video: { redacted: true } }, { ...input, video: null }]))).toBe(true);
  });
  it('rejects an input whose video field is omitted (never normalized to unpatched)', () => {
    const withoutVideo: Record<string, unknown> = { ...input };
    delete withoutVideo.video;
    expect(isPatchView(view([withoutVideo]))).toBe(false);
  });
  it('rejects an input whose video is undefined or a non-source object', () => {
    expect(isPatchView(view([{ ...input, video: undefined }]))).toBe(false);
    expect(isPatchView(view([{ ...input, video: {} }]))).toBe(false);
  });
});

describe('isPatchView output plane (E3 video/audio + patch-f subtitle)', () => {
  const video = { slot: 0, live: true, source: { source: 'cam-1' }, path: 'P1', pending: null, degraded: false };
  const sub = { slot: 0, format: 'webvtt', language: 'en', name: null, live: true, source: { source: 'cam-1' } };
  const view = (output: Record<string, unknown>) => ({ mixer: [], outputs: [{ output: 'o', video: [video], audio: [{ slot: 0, source: 'follow' }], ...output }] });
  it('accepts video, audio and subtitle slots together, and an output without subtitle', () => {
    expect(isPatchView(view({ subtitle: [sub, { ...sub, slot: 1, live: false, source: null }, { ...sub, slot: 2, source: { redacted: true } }] }))).toBe(true);
    expect(isPatchView(view({}))).toBe(true);
  });
  it('rejects a malformed subtitle slot (no live, string source, non-array)', () => {
    const noLive: Record<string, unknown> = { ...sub };
    delete noLive.live;
    expect(isPatchView(view({ subtitle: [noLive] }))).toBe(false);
    expect(isPatchView(view({ subtitle: [{ ...sub, source: 'none' }] }))).toBe(false);
    expect(isPatchView(view({ subtitle: [{ ...sub, source: {} }] }))).toBe(false);
    expect(isPatchView(view({ subtitle: null }))).toBe(false);
  });
  it('rejects an output missing its video or audio readback', () => {
    expect(isPatchView({ mixer: [], outputs: [{ output: 'o', audio: [], subtitle: [] }] })).toBe(false);
    expect(isPatchView({ mixer: [], outputs: [{ output: 'o', video: [], subtitle: [] }] })).toBe(false);
  });
});

describe('parseCrosspoints plane/destination shapes', () => {
  it('keeps a subtitle slot crosspoint and drops shapes no request can carry', () => {
    expect(parseCrosspoints([
      { plane: 'subtitle', destination: { output: 'o', slot: 1 }, source: { source: 'cam-1' } },
      { plane: 'subtitle', destination: { output: 'o', slot: 0 }, source: { bus: 'program' } },
      { plane: 'audio', destination: { mixer: 'me1', input: 1 }, source: 'follow' },
    ])).toEqual({
      crosspoints: [{ plane: 'subtitle', destination: { output: 'o', slot: 1 }, source: { source: 'cam-1' } }],
      invalid: 2,
    });
  });
});
