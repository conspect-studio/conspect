// Unit tests for the audio-routing form mapping + validation (pure logic).
//
// The form manages the DOCUMENT-level `[audio]` block
// (crates/multiview-config/src/audio.rs): a working sample rate plus one route
// per input (program-bus membership/gain/mute + optional discrete track). The
// mapping must round-trip the wire document exactly (internally-tagged
// `channels.kind`, omitted optionals) and the validator must mirror the
// control plane's 422 rules so errors surface inline before a PUT.
import { describe, expect, it } from 'vitest';

import {
  audioFormFromDocument,
  audioFormToDocument,
  declaredTracks,
  emptyAudioForm,
  emptyAudioRoute,
  PROGRAM_TRACK,
  validateAudioForm,
} from './audioForms';
import type { AudioRoutingDocument } from './audioForms';

const WIRE_DOC: AudioRoutingDocument = {
  sample_rate_hz: 48000,
  routes: [
    {
      input_id: 'cam1',
      channels: { kind: 'stereo' },
      target_track: 'cam1-clean',
      language: 'eng',
      title: 'Camera 1',
      include_in_program_bus: true,
      gain_db: -3,
      mute: false,
    },
    {
      input_id: 'comms',
      channels: { kind: 'mono' },
      include_in_program_bus: false,
      gain_db: 0,
      mute: true,
    },
  ],
};

describe('audioFormFromDocument', () => {
  it('maps a wire document onto the form state', () => {
    const form = audioFormFromDocument(WIRE_DOC);
    expect(form.sampleRateHz).toBe('48000');
    expect(form.routes).toHaveLength(2);
    expect(form.routes[0]).toMatchObject({
      inputId: 'cam1',
      channels: 'stereo',
      targetTrack: 'cam1-clean',
      language: 'eng',
      title: 'Camera 1',
      includeInProgramBus: true,
      gainDb: '-3',
      mute: false,
    });
    // Absent optionals become empty form strings, never the text "undefined".
    expect(form.routes[1]).toMatchObject({
      inputId: 'comms',
      channels: 'mono',
      targetTrack: '',
      language: '',
      title: '',
      mute: true,
    });
  });

  it('maps an unconfigured document (null) onto the 48 kHz default', () => {
    const form = audioFormFromDocument(null);
    expect(form.sampleRateHz).toBe('48000');
    expect(form.routes).toHaveLength(0);
  });
});

describe('audioFormToDocument', () => {
  it('round-trips the wire document exactly', () => {
    expect(audioFormToDocument(audioFormFromDocument(WIRE_DOC))).toEqual(WIRE_DOC);
  });

  it('omits blank optional fields instead of sending empty strings', () => {
    const form = {
      ...emptyAudioForm(),
      routes: [
        {
          ...emptyAudioRoute(),
          inputId: '  cam2  ',
          targetTrack: '   ',
          language: '',
          title: ' ',
        },
      ],
    };
    const doc = audioFormToDocument(form);
    const route = doc.routes[0];
    expect(route).toBeDefined();
    expect(route?.input_id).toBe('cam2');
    expect(route).not.toHaveProperty('target_track');
    expect(route).not.toHaveProperty('language');
    expect(route).not.toHaveProperty('title');
  });

  it('defaults a blank gain to unity (0 dB)', () => {
    const form = {
      ...emptyAudioForm(),
      routes: [{ ...emptyAudioRoute(), inputId: 'cam1', gainDb: '' }],
    };
    expect(audioFormToDocument(form).routes[0]?.gain_db).toBe(0);
  });
});

describe('declaredTracks', () => {
  it('always leads with the program bus', () => {
    expect(declaredTracks(emptyAudioForm())).toEqual([PROGRAM_TRACK]);
  });

  it('lists each named track once, in declaration order', () => {
    const form = audioFormFromDocument(WIRE_DOC);
    const withDuplicate = {
      ...form,
      routes: [
        ...form.routes,
        { ...emptyAudioRoute(), inputId: 'x', targetTrack: 'cam1-clean' },
      ],
    };
    expect(declaredTracks(withDuplicate)).toEqual([
      PROGRAM_TRACK,
      'cam1-clean',
    ]);
  });
});

describe('validateAudioForm', () => {
  it('accepts the round-tripped document', () => {
    const errors = validateAudioForm(audioFormFromDocument(WIRE_DOC));
    expect(errors.hasErrors).toBe(false);
  });

  it('requires a positive integer sample rate', () => {
    expect(
      validateAudioForm({ ...emptyAudioForm(), sampleRateHz: '' }).sampleRateHz,
    ).toBe('required');
    expect(
      validateAudioForm({ ...emptyAudioForm(), sampleRateHz: '44.1' }).sampleRateHz,
    ).toBe('positive-int');
    expect(
      validateAudioForm({ ...emptyAudioForm(), sampleRateHz: '0' }).sampleRateHz,
    ).toBe('positive-int');
  });

  it('requires an input id on every route', () => {
    const form = { ...emptyAudioForm(), routes: [emptyAudioRoute()] };
    const errors = validateAudioForm(form);
    expect(errors.hasErrors).toBe(true);
    expect(errors.routes[0]?.inputId).toBe('required');
  });

  it('rejects a non-numeric gain', () => {
    const form = {
      ...emptyAudioForm(),
      routes: [{ ...emptyAudioRoute(), inputId: 'cam1', gainDb: 'loud' }],
    };
    expect(validateAudioForm(form).routes[0]?.gainDb).toBe('finite-number');
  });

  it('rejects the reserved program track name', () => {
    const form = {
      ...emptyAudioForm(),
      routes: [{ ...emptyAudioRoute(), inputId: 'cam1', targetTrack: PROGRAM_TRACK }],
    };
    expect(validateAudioForm(form).routes[0]?.targetTrack).toBe('reserved-track');
  });

  it('rejects two routes claiming the same discrete track', () => {
    const form = {
      ...emptyAudioForm(),
      routes: [
        { ...emptyAudioRoute(), inputId: 'a', targetTrack: 'clean' },
        { ...emptyAudioRoute(), inputId: 'b', targetTrack: 'clean' },
      ],
    };
    const errors = validateAudioForm(form);
    expect(errors.routes[1]?.targetTrack).toBe('duplicate-track');
  });

  it('rejects two routes for the same input', () => {
    const form = {
      ...emptyAudioForm(),
      routes: [
        { ...emptyAudioRoute(), inputId: 'a' },
        { ...emptyAudioRoute(), inputId: 'a' },
      ],
    };
    const errors = validateAudioForm(form);
    expect(errors.routes[1]?.inputId).toBe('duplicate-input');
  });

  it('rejects a program bus whose every member is muted', () => {
    const form = {
      ...emptyAudioForm(),
      routes: [
        { ...emptyAudioRoute(), inputId: 'a', includeInProgramBus: true, mute: true },
      ],
    };
    expect(validateAudioForm(form).program).toBe('program-bus-muted');
    // One unmuted contributor clears it.
    const ok = {
      ...emptyAudioForm(),
      routes: [
        { ...emptyAudioRoute(), inputId: 'a', includeInProgramBus: true, mute: true },
        { ...emptyAudioRoute(), inputId: 'b', includeInProgramBus: true, mute: false },
      ],
    };
    expect(validateAudioForm(ok).program).toBeUndefined();
  });
});
