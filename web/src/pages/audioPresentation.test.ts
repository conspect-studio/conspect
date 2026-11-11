// audioPresentation tests — the pure derivations behind the Audio console:
// the ONE linear fader mapping (ticks, knob and readout share it), the exact
// ±1/±6 nudges (staged into the draft, never silently clamped — the document
// accepts any finite gain), the fader's display clamping for out-of-range
// typed values, and the draft-vs-saved document diff the console's "Draft /
// Saved" honesty rides on. The input-meter lookup is pinned too: it only ever
// returns REAL `audio.meter` telemetry, never a value derived from gain.
import { describe, expect, it } from 'vitest';

import {
  documentDraftDiff,
  faderDisplayDb,
  faderPos,
  formatDb,
  GAIN_FADER_MAX_DB,
  GAIN_FADER_MIN_DB,
  GAIN_NUDGE_STEPS,
  GAIN_SCALE_TICKS_DB,
  inputMeterSample,
  inputPeakDb,
  nudgeGainDb,
  parseGainDb,
} from './audioPresentation';
import type { AudioFormState, AudioRouteRow } from '../resources/audioForms';
import type { AudioMeterSample } from '../realtime/useAudioMeter';

function row(overrides: Partial<AudioRouteRow> = {}): AudioRouteRow {
  return {
    inputId: 'cam-1',
    channels: 'stereo',
    targetTrack: '',
    language: '',
    title: '',
    includeInProgramBus: true,
    gainDb: '0',
    mute: false,
    ...overrides,
  };
}

function form(routes: readonly AudioRouteRow[], sampleRateHz = '48000'): AudioFormState {
  return { sampleRateHz, routes };
}

/** A REAL `audio.meter` input sample (per-input peak/RMS telemetry). */
function sample(inputId: string, peakDb: readonly number[]): AudioMeterSample {
  return {
    track: 1,
    input_id: inputId,
    peak_db: peakDb,
    rms_db: peakDb,
    clip: false,
    overflow: false,
    sampled_hz: 30,
  };
}

describe('parseGainDb', () => {
  it('parses like the form validator: blank is unity, finite is exact', () => {
    expect(parseGainDb('')).toBe(0);
    expect(parseGainDb('  ')).toBe(0);
    expect(parseGainDb('-3')).toBe(-3);
    expect(parseGainDb(' -3.5 ')).toBe(-3.5);
    expect(parseGainDb('0')).toBe(0);
  });

  it('rejects what the server would reject', () => {
    expect(parseGainDb('abc')).toBeUndefined();
    expect(parseGainDb('1e999')).toBeUndefined();
    expect(parseGainDb('NaN')).toBeUndefined();
    expect(parseGainDb('Infinity')).toBeUndefined();
  });

  it('accepts f32-magnitudes the server accepts', () => {
    expect(parseGainDb('1e3')).toBe(1000);
    expect(parseGainDb('-1e30')).toBe(-1e30);
  });
});

describe('formatDb', () => {
  it('formats readouts minimally', () => {
    expect(formatDb(0)).toBe('0');
    expect(formatDb(-3)).toBe('-3');
    expect(formatDb(1.5)).toBe('1.5');
    expect(formatDb(-2.25)).toBe('-2.25');
    expect(formatDb(-12.0000000001)).toBe('-12');
  });
});

describe('nudgeGainDb', () => {
  it('adds the delta exactly, from the blank-unity default too', () => {
    expect(nudgeGainDb('0', 1)).toBe('1');
    expect(nudgeGainDb('', -1)).toBe('-1');
    expect(nudgeGainDb('-3', -1)).toBe('-4');
    expect(nudgeGainDb('-3', -6)).toBe('-9');
    expect(nudgeGainDb('-3', 6)).toBe('3');
  });

  it('does not convert an invalid transient gain to fabricated zero', () => {
    for (const value of ['-', 'NaN', 'Infinity', '1e999']) {
      expect(nudgeGainDb(value, 1)).toBe(value);
    }
  });

  it('keeps fractional drafts exact', () => {
    expect(nudgeGainDb('-3.5', -1)).toBe('-4.5');
    expect(nudgeGainDb('0.5', 1)).toBe('1.5');
  });

  it('never silently clamps: the fader range is a display bound, not a document bound', () => {
    // +6 from 0 dB is a legal draft (the wire gain is any finite f32); the
    // fader pins visually while the readout carries the exact value.
    expect(nudgeGainDb('0', 6)).toBe('6');
    expect(nudgeGainDb('-59', -6)).toBe('-65');
    expect(nudgeGainDb('1e3', -1)).toBe('999');
  });

  it('round-trips through parse: a nudged draft stays a valid gain string', () => {
    for (const start of ['', '0', '-3', '-60', '0.5', '6']) {
      for (const delta of GAIN_NUDGE_STEPS.flatMap((step) => [step, -step])) {
        expect(parseGainDb(nudgeGainDb(start, delta))).toBe(
          (parseGainDb(start) ?? 0) + delta,
        );
      }
    }
  });
});

describe('fader mapping', () => {
  it('exposes the approved broadcast direction: 0 dB at the top, −60 dB at the bottom', () => {
    expect(GAIN_FADER_MIN_DB).toBe(-60);
    expect(GAIN_FADER_MAX_DB).toBe(0);
    expect(GAIN_SCALE_TICKS_DB[0]).toBe(0);
    expect(GAIN_SCALE_TICKS_DB).toContain(-60);
  });

  it('maps dB to ONE shared 0..1 position used by ticks, knob and readout', () => {
    expect(faderPos(0)).toBe(1);
    expect(faderPos(-60)).toBe(0);
    expect(faderPos(-30)).toBeCloseTo(0.5, 10);
    expect(faderPos(-45)).toBeCloseTo(0.25, 10);
    // every scale tick sits exactly where the knob reads that value
    for (const tick of GAIN_SCALE_TICKS_DB) {
      expect(faderPos(tick)).toBeCloseTo((tick - GAIN_FADER_MIN_DB) / (GAIN_FADER_MAX_DB - GAIN_FADER_MIN_DB), 10);
    }
  });

  it('clamps only the DISPLAY value for out-of-range typed gains', () => {
    expect(faderDisplayDb(6)).toBe(0);
    expect(faderDisplayDb(-80)).toBe(-60);
    expect(faderDisplayDb(-12)).toBe(-12);
  });
});

describe('documentDraftDiff', () => {
  it('reports no changes when the draft matches the saved document', () => {
    const saved = form([row({ gainDb: '-3', targetTrack: 'cam1-clean' })]);
    const draft = form([row({ gainDb: '-3.0', targetTrack: ' cam1-clean ' })]);
    const diff = documentDraftDiff(saved, draft);
    expect(diff.anyChanged).toBe(false);
    expect(diff.changedRouteCount).toBe(0);
    expect(diff.sampleRateChanged).toBe(false);
    expect(diff.routes[0]?.changed).toBe(false);
  });

  it('flags gain, mute and program-mix edits separately', () => {
    const saved = form([row({ gainDb: '-3' })]);
    const draft = form([row({ gainDb: '-4', mute: true, includeInProgramBus: false })]);
    const diff = documentDraftDiff(saved, draft);
    expect(diff.routes[0]?.gainChanged).toBe(true);
    expect(diff.routes[0]?.muteChanged).toBe(true);
    expect(diff.routes[0]?.includeChanged).toBe(true);
    expect(diff.routes[0]?.changed).toBe(true);
    expect(diff.changedRouteCount).toBe(1);
    expect(diff.anyChanged).toBe(true);
  });

  it('flags track, language, title, channels and input edits as changed routes', () => {
    const saved = form([row()]);
    expect(documentDraftDiff(saved, form([row({ targetTrack: 'x' })])).routes[0]?.changed).toBe(true);
    expect(documentDraftDiff(saved, form([row({ language: 'eng' })])).routes[0]?.changed).toBe(true);
    expect(documentDraftDiff(saved, form([row({ title: 'T' })])).routes[0]?.changed).toBe(true);
    expect(documentDraftDiff(saved, form([row({ channels: 'mono' })])).routes[0]?.changed).toBe(true);
    expect(documentDraftDiff(saved, form([row({ inputId: 'cam-2' })])).routes[0]?.changed).toBe(true);
  });

  it('treats newly added routes as new (unsaved) drafts', () => {
    const saved = form([]);
    const diff = documentDraftDiff(saved, form([row()]));
    expect(diff.routes[0]?.isNew).toBe(true);
    expect(diff.routes[0]?.changed).toBe(true);
    expect(diff.changedRouteCount).toBe(1);
    expect(diff.anyChanged).toBe(true);
  });

  it('keeps positional alignment when routes are removed or added mid-list', () => {
    const saved = form([row({ inputId: 'a' }), row({ inputId: 'b' })]);
    const removed = documentDraftDiff(saved, form([row({ inputId: 'a' })]));
    expect(removed.routes).toHaveLength(1);
    expect(removed.routes[0]?.changed).toBe(false);

    const added = documentDraftDiff(
      form([row({ inputId: 'a' })]),
      form([row({ inputId: 'a' }), row({ inputId: 'b' })]),
    );
    expect(added.routes[0]?.isNew).toBe(false);
    expect(added.routes[1]?.isNew).toBe(true);
    expect(added.changedRouteCount).toBe(1);
  });

  it('associates saved rows by local identity, never duplicate input names or shifted positions', () => {
    const saved = form([row({ inputId: 'dup', gainDb: '-3' }), row({ inputId: 'dup', gainDb: '-9' })]);
    const draft = form([row({ inputId: 'dup', gainDb: '-9' }), row({ inputId: 'dup', gainDb: '0' })]);
    const diff = documentDraftDiff(saved, draft, [1, undefined]);
    expect(diff.routes[0]?.changed).toBe(false);
    expect(diff.routes[1]?.isNew).toBe(true);
    expect(diff.changedRouteCount).toBe(2); // one new row and one removed row
  });

  it('compares the sample rate semantically, not lexically', () => {
    const saved = form([row()], '48000');
    expect(documentDraftDiff(saved, form([row()], ' 48000 ')).sampleRateChanged).toBe(false);
    expect(documentDraftDiff(saved, form([row()], '048000')).sampleRateChanged).toBe(false);
    expect(documentDraftDiff(saved, form([row()], '44100')).sampleRateChanged).toBe(true);
    expect(documentDraftDiff(saved, form([row()], 'abc')).sampleRateChanged).toBe(true);
  });
});

describe('input meter lookup', () => {
  it('finds the real telemetry sample for an input id', () => {
    const inputs = [sample('cam-1', [-12, -9]), sample('cam-2', [-42])];
    expect(inputMeterSample(inputs, 'cam-1')?.input_id).toBe('cam-1');
    expect(inputMeterSample(inputs, 'nope')).toBeUndefined();
    expect(inputMeterSample([], 'cam-1')).toBeUndefined();
  });

  it('reduces a sample to the loudest channel peak (input presence), or undefined', () => {
    expect(inputPeakDb(sample('cam-1', [-12, -9]))).toBe(-9);
    expect(inputPeakDb(sample('cam-1', [-33]))).toBe(-33);
  });

  it('never invents a level for an input with no telemetry', () => {
    expect(inputPeakDb(sample('cam-1', []))).toBeUndefined();
    expect(inputMeterSample([sample('cam-2', [-1])], 'cam-1')).toBeUndefined();
  });
});
