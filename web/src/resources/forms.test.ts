// Unit tests for the pure form-state <-> config-body mapping of the Sources /
// Outputs / Overlays management forms. The body shapes asserted here mirror the
// Rust config schema EXACTLY (crates/multiview-config/src/schema.rs: `Source`,
// `Output`, `Overlay`; audio.rs `OutputAudio`; placement.rs `DevicePin`) — the
// control plane is gaining 422 typed validation of these shapes (ADR-W015), so
// a wrong body is a rejected body. No DOM, no React.
import { describe, expect, it } from 'vitest';

import {
  emptyOutputForm,
  emptyOverlayForm,
  emptySourceForm,
  isKnownTimezone,
  isValidLocalDateTime,
  isValidTimeOfDay,
  isValidUrl,
  OUTPUT_RUNNABLE,
  outputFormFromRecord,
  outputFormToBody,
  overlayFormFromRecord,
  overlayFormToBody,
  sourceFormFromRecord,
  sourceFormToBody,
  validateOutputForm,
  validateOverlayForm,
  validateSourceForm,
  withOverlayKind,
  withSourceKind,
  withTimerTargetType,
} from './forms';
import type { OutputFormState, OverlayFormState, SourceFormState } from './forms';

/**
 * Narrow a parsed form: `*FormFromRecord` refuses an unknown kind with
 * `undefined` (an explicit non-editable sentinel), so the known-kind fixtures
 * here assert through this helper.
 */
function defined<T>(value: T | undefined): T {
  if (value === undefined) {
    throw new Error('expected a parsed form for a known kind');
  }
  return value;
}

function sourceForm(over: Partial<SourceFormState> = {}): SourceFormState {
  return { ...emptySourceForm(), id: 'cam1', name: 'Cam 1', ...over };
}

function outputForm(over: Partial<OutputFormState> = {}): OutputFormState {
  return { ...emptyOutputForm(), id: 'out1', name: 'Out 1', ...over };
}

function overlayForm(over: Partial<OverlayFormState> = {}): OverlayFormState {
  return { ...emptyOverlayForm(), id: 'ov1', name: 'Overlay 1', ...over };
}

describe('isValidUrl', () => {
  it('accepts the kind scheme and rejects others', () => {
    expect(isValidUrl('rtsp://cam.example/stream', ['rtsp'])).toBe(true);
    expect(isValidUrl('http://cam.example/stream', ['rtsp'])).toBe(false);
    expect(isValidUrl('srt://relay.example:7001', ['srt'])).toBe(true);
    expect(isValidUrl('rtmp://ingest.example/app/key', ['rtmp', 'rtmps'])).toBe(true);
    expect(isValidUrl('https://example.com/x.m3u8', ['http', 'https'])).toBe(true);
  });

  it('accepts bracketed IPv6 literal hosts', () => {
    expect(isValidUrl('rtsp://[2001:db8::1]:8554/cam', ['rtsp'])).toBe(true);
    expect(isValidUrl('srt://[2001:db8::2]:7001', ['srt'])).toBe(true);
    expect(isValidUrl('https://[2001:db8::3]/playlist.m3u8', ['http', 'https'])).toBe(true);
  });

  it('rejects an unbracketed IPv6 literal (the port would be ambiguous)', () => {
    expect(isValidUrl('rtsp://2001:db8::1/cam', ['rtsp'])).toBe(false);
  });

  it('rejects garbage and scheme-less strings', () => {
    expect(isValidUrl('not a url', ['rtsp'])).toBe(false);
    expect(isValidUrl('', ['rtsp'])).toBe(false);
    expect(isValidUrl('cam.example/stream', ['rtsp'])).toBe(false);
  });
});

describe('sourceFormToBody', () => {
  it('builds an rtsp body with id + display_name + url', () => {
    const body = sourceFormToBody(
      sourceForm({ kind: 'rtsp', url: 'rtsp://cam.example/live' }),
    );
    expect(body).toEqual({
      id: 'cam1',
      display_name: 'Cam 1',
      kind: 'rtsp',
      url: 'rtsp://cam.example/live',
    });
  });

  it('carries RtspOptions under the rtsp key when a transport is chosen', () => {
    const body = sourceFormToBody(
      sourceForm({ kind: 'rtsp', url: 'rtsp://cam.example/live', rtspTransport: 'tcp' }),
    );
    expect(body.rtsp).toEqual({ transport: 'tcp' });
  });

  it('omits the rtsp options block when transport is the default', () => {
    const body = sourceFormToBody(
      sourceForm({ kind: 'rtsp', url: 'rtsp://cam.example/live' }),
    );
    expect(body).not.toHaveProperty('rtsp');
  });

  it('builds each url-locator kind with its tag', () => {
    for (const kind of ['hls', 'youtube', 'ts', 'srt', 'rtmp'] as const) {
      const body = sourceFormToBody(sourceForm({ kind, url: 'srt://relay.example:7001' }));
      expect(body.kind).toBe(kind);
      expect(body.url).toBe('srt://relay.example:7001');
    }
  });

  it('builds ndi by source name and file by path', () => {
    expect(sourceFormToBody(sourceForm({ kind: 'ndi', ndiName: 'STUDIO (CAM 1)' }))).toMatchObject({
      kind: 'ndi',
      name: 'STUDIO (CAM 1)',
    });
    expect(sourceFormToBody(sourceForm({ kind: 'file', path: '/media/clip.mp4' }))).toMatchObject({
      kind: 'file',
      path: '/media/clip.mp4',
    });
  });

  it('builds the synthetic kinds (bars carries only its tag)', () => {
    const bars = sourceFormToBody(sourceForm({ kind: 'bars' }));
    expect(bars).toEqual({ id: 'cam1', display_name: 'Cam 1', kind: 'bars' });

    const solid = sourceFormToBody(sourceForm({ kind: 'solid', color: '#101014' }));
    expect(solid).toMatchObject({ kind: 'solid', color: '#101014' });

    const clock = sourceFormToBody(
      sourceForm({ kind: 'clock', clockFace: 'digital', clockTwelveHour: true, clockTzMinutes: '600' }),
    );
    expect(clock).toMatchObject({
      kind: 'clock',
      face: 'digital',
      twelve_hour: true,
      tz_offset_minutes: 600,
    });
  });

  it('carries the advanced blocks exactly per schema when set', () => {
    const body = sourceFormToBody(
      sourceForm({
        kind: 'hls',
        url: 'https://example.com/x.m3u8',
        authSecretRef: 'op://Servers/cam/credentials',
        colorOverrideEnabled: true,
        colorPrimaries: 'bt709',
        colorTransfer: 'auto',
        colorMatrix: 'auto',
        colorRange: 'limited',
        captionsMode: 'teletext_page',
        captionsPage: '801',
        gpuPinEnabled: true,
        gpuPinVendor: 'nvidia',
        gpuPinStableId: 'GPU-uuid-1',
        wallclock: 'use',
      }),
    );
    expect(body.auth).toEqual({ secret_ref: 'op://Servers/cam/credentials' });
    expect(body.color_override).toEqual({
      primaries: 'bt709',
      transfer: 'auto',
      matrix: 'auto',
      range: 'limited',
    });
    expect(body.captions).toEqual({ mode: 'teletext_page', page: 801 });
    expect(body.gpu_pin).toEqual({ vendor: 'nvidia', stable_id: 'GPU-uuid-1' });
    expect(body.wallclock).toEqual({ use: 'use' });
  });

  it('maps every caption mode onto its tagged payload', () => {
    const base = sourceForm({ kind: 'hls', url: 'https://example.com/x.m3u8' });
    expect(sourceFormToBody({ ...base, captionsMode: 'auto' }).captions).toEqual({ mode: 'auto' });
    expect(sourceFormToBody({ ...base, captionsMode: 'off' }).captions).toEqual({ mode: 'off' });
    expect(
      sourceFormToBody({ ...base, captionsMode: 'track', captionsTrack: 'eng' }).captions,
    ).toEqual({ mode: 'track', id: 'eng' });
    expect(
      sourceFormToBody({ ...base, captionsMode: 'embedded_cc', captionsField: 'cc1' }).captions,
    ).toEqual({ mode: 'embedded_cc', field: 'cc1' });
    expect(
      sourceFormToBody({ ...base, captionsMode: 'sidecar', captionsPath: '/subs/a.vtt' }).captions,
    ).toEqual({ mode: 'sidecar', path: '/subs/a.vtt' });
  });

  it('omits every optional block when unset (absent ≠ default-valued)', () => {
    const body = sourceFormToBody(sourceForm({ kind: 'rtsp', url: 'rtsp://h/x' }));
    for (const key of ['auth', 'color_override', 'captions', 'gpu_pin', 'wallclock']) {
      expect(body).not.toHaveProperty(key);
    }
  });

  it('preserves unknown-but-valid extra body fields across an edit round-trip', () => {
    const record = {
      id: 'cam1',
      name: 'Cam 1',
      body: {
        id: 'cam1',
        kind: 'rtsp',
        url: 'rtsp://h/x',
        future_field: { keep: true },
      },
    };
    const form = defined(sourceFormFromRecord(record));
    const body = sourceFormToBody(form);
    expect(body.future_field).toEqual({ keep: true });
  });

  it('round-trips an own __proto__ body key as plain data (no prototype swap)', () => {
    // A hostile/odd stored body can carry an OWN `__proto__` key (JSON allows
    // it). The extra-preservation path must treat it as data: never mutate the
    // accumulator's prototype, never drop the key.
    const storedBody: Record<string, unknown> = { id: 'cam1', kind: 'bars' };
    Object.defineProperty(storedBody, '__proto__', {
      value: { x: 1 },
      enumerable: true,
      configurable: true,
      writable: true,
    });
    const form = defined(
      sourceFormFromRecord({ id: 'cam1', name: 'Cam 1', body: storedBody }),
    );
    // Preserved as an own data key on the extra map…
    expect(Object.getOwnPropertyDescriptor(form.extra, '__proto__')?.value).toEqual({
      x: 1,
    });
    // …and re-emitted as an own data key on the body, whose prototype is the
    // ordinary Object.prototype (not the smuggled object).
    const body = sourceFormToBody(form);
    expect(Object.getOwnPropertyDescriptor(body, '__proto__')?.value).toEqual({ x: 1 });
    expect(Object.getPrototypeOf(body)).toBe(Object.prototype);
    expect(body.kind).toBe('bars');
  });
});

describe('sourceFormFromRecord', () => {
  it('refuses an unknown kind with the undefined sentinel (never folds)', () => {
    expect(
      sourceFormFromRecord({
        id: 'aoip',
        name: 'AoIP feed',
        body: { id: 'aoip', kind: 'aes67', url: 'aes67://x' },
      }),
    ).toBeUndefined();
    // An absent kind tag is equally unknown (the schema requires the tag).
    expect(
      sourceFormFromRecord({ id: 'x', name: 'X', body: { id: 'x' } }),
    ).toBeUndefined();
  });

  it('still canonicalizes the legacy test alias to bars (a KNOWN kind)', () => {
    const form = defined(
      sourceFormFromRecord({ id: 't', name: 'T', body: { id: 't', kind: 'test' } }),
    );
    expect(form.kind).toBe('bars');
  });

  it('seeds the name from an authored display_name, not the store name', () => {
    const form = defined(
      sourceFormFromRecord({
        id: 'cam1',
        name: 'Store name',
        body: { id: 'cam1', kind: 'bars', display_name: 'Authored name' },
      }),
    );
    expect(form.name).toBe('Authored name');
    // Round-trip is stable: the authored display_name is written back intact.
    expect(sourceFormToBody(form).display_name).toBe('Authored name');
  });

  it('falls back to the store name when no display_name was authored', () => {
    const form = defined(
      sourceFormFromRecord({
        id: 'cam1',
        name: 'Store name',
        body: { id: 'cam1', kind: 'bars' },
      }),
    );
    expect(form.name).toBe('Store name');
  });

  it('prefills the kind payload and the advanced blocks', () => {
    const form = defined(sourceFormFromRecord({
      id: 'cam1',
      name: 'Cam 1',
      body: {
        id: 'cam1',
        kind: 'rtsp',
        url: 'rtsp://h/x',
        rtsp: { transport: 'udp' },
        auth: { secret_ref: 'op://v/i/f' },
        captions: { mode: 'track', id: 'eng' },
        gpu_pin: { vendor: 'amd', stable_id: '0000:03:00.0' },
        wallclock: { use: 'discard' },
        color_override: { primaries: 'bt2020', transfer: 'auto', matrix: 'auto', range: 'auto' },
      },
    }));
    expect(form.kind).toBe('rtsp');
    expect(form.url).toBe('rtsp://h/x');
    expect(form.rtspTransport).toBe('udp');
    expect(form.authSecretRef).toBe('op://v/i/f');
    expect(form.captionsMode).toBe('track');
    expect(form.captionsTrack).toBe('eng');
    expect(form.gpuPinEnabled).toBe(true);
    expect(form.gpuPinVendor).toBe('amd');
    expect(form.gpuPinStableId).toBe('0000:03:00.0');
    expect(form.wallclock).toBe('discard');
    expect(form.colorOverrideEnabled).toBe(true);
    expect(form.colorPrimaries).toBe('bt2020');
  });

  it('parses the youtube kind', () => {
    const form = defined(sourceFormFromRecord({
      id: 'yt',
      name: 'YT',
      body: { id: 'yt', kind: 'youtube', url: 'https://www.youtube.com/watch?v=abc' },
    }));
    expect(form.kind).toBe('youtube');
    expect(form.url).toBe('https://www.youtube.com/watch?v=abc');
  });
});

describe('withSourceKind', () => {
  it('resets the kind payload so stale fields never leak into the body', () => {
    const rtsp = sourceForm({ kind: 'rtsp', url: 'rtsp://h/x', rtspTransport: 'tcp' });
    const solid = withSourceKind(rtsp, 'solid');
    expect(solid.kind).toBe('solid');
    const body = sourceFormToBody(solid);
    expect(body).not.toHaveProperty('url');
    expect(body).not.toHaveProperty('rtsp');
    expect(typeof body.color).toBe('string');
  });
});

// --- Clock + Timer synthetic sources (SYN-CLOCK-UI, ADR-0047) ----------------

describe('clock source (ADR-0047 metadata + dual face)', () => {
  it('writes the dual face, IANA timezone, label, and the metadata toggles', () => {
    const body = sourceFormToBody(
      sourceForm({
        kind: 'clock',
        clockFace: 'dual',
        clockTwelveHour: true,
        clockTimezone: 'Australia/Sydney',
        clockLabel: 'Sydney',
        clockShowOffset: true,
        clockShowReference: true,
        clockNumerals: true,
      }),
    );
    expect(body).toMatchObject({
      kind: 'clock',
      face: 'dual',
      twelve_hour: true,
      timezone: 'Australia/Sydney',
      label: 'Sydney',
      show_offset: true,
      show_reference: true,
      numerals: true,
    });
    // IANA mode wins over the fixed offset: the offset is omitted entirely.
    expect(body).not.toHaveProperty('tz_offset_minutes');
  });

  it('falls back to the fixed offset when no IANA zone is set', () => {
    const body = sourceFormToBody(
      sourceForm({ kind: 'clock', clockTimezone: '', clockTzMinutes: '600' }),
    );
    expect(body.tz_offset_minutes).toBe(600);
    expect(body).not.toHaveProperty('timezone');
  });

  it('omits the optional label but always writes the boolean toggles', () => {
    const body = sourceFormToBody(sourceForm({ kind: 'clock', clockLabel: '   ' }));
    expect(body).not.toHaveProperty('label');
    expect(body.show_offset).toBe(false);
    expect(body.show_reference).toBe(false);
    expect(body.numerals).toBe(false);
  });

  it('round-trips a stored clock body exactly (IANA + metadata)', () => {
    const record = {
      id: 'syd',
      name: 'Sydney',
      body: {
        id: 'syd',
        kind: 'clock',
        face: 'dual',
        twelve_hour: true,
        timezone: 'Australia/Sydney',
        label: 'Sydney',
        show_offset: true,
        show_reference: false,
        numerals: true,
      },
    };
    const form = defined(sourceFormFromRecord(record));
    expect(form.clockFace).toBe('dual');
    expect(form.clockTwelveHour).toBe(true);
    expect(form.clockTimezone).toBe('Australia/Sydney');
    expect(form.clockLabel).toBe('Sydney');
    expect(form.clockShowOffset).toBe(true);
    expect(form.clockShowReference).toBe(false);
    expect(form.clockNumerals).toBe(true);
    // The body the form re-emits keeps the same managed shape.
    expect(sourceFormToBody(form)).toMatchObject({
      kind: 'clock',
      face: 'dual',
      twelve_hour: true,
      timezone: 'Australia/Sydney',
      label: 'Sydney',
      show_offset: true,
      show_reference: false,
      numerals: true,
    });
  });

  it('validates an unknown IANA zone but accepts a known one / a fixed offset', () => {
    expect(
      validateSourceForm(sourceForm({ kind: 'clock', clockTimezone: 'Mars/Olympus' }), true)
        .clockTimezone,
    ).toBe('timezone');
    expect(
      validateSourceForm(sourceForm({ kind: 'clock', clockTimezone: 'UTC' }), true).clockTimezone,
    ).toBeUndefined();
    // With a zone set, the fixed offset is not range-checked (it is ignored).
    expect(
      validateSourceForm(
        sourceForm({ kind: 'clock', clockTimezone: 'UTC', clockTzMinutes: '9999' }),
        true,
      ).clockTzMinutes,
    ).toBeUndefined();
  });
});

describe('timer source (ADR-0047)', () => {
  it('writes a time-of-day target with the tagged `target` discriminator', () => {
    const body = sourceFormToBody(
      sourceForm({
        kind: 'timer',
        timerTargetType: 'time_of_day',
        timerAt: '14:30:00',
        timerTimezone: 'Australia/Sydney',
        timerRecurDaily: true,
        timerDirection: 'down',
        timerOnTarget: 'recur',
        timerFormat: 'mm_ss',
        timerLabel: 'ON AIR IN',
        timerOverrunPrefix: 'OVER ',
        timerOverrunBadge: false,
      }),
    );
    expect(body).toMatchObject({
      kind: 'timer',
      target: 'time_of_day',
      at: '14:30:00',
      timezone: 'Australia/Sydney',
      recur_daily: true,
      direction: 'down',
      on_target: 'recur',
      format: 'mm_ss',
      label: 'ON AIR IN',
      overrun_prefix: 'OVER ',
      overrun_badge: false,
    });
    expect(body).not.toHaveProperty('tz_offset_minutes');
  });

  it('writes a date+time target and omits the time-of-day-only recur_daily', () => {
    const body = sourceFormToBody(
      sourceForm({
        kind: 'timer',
        timerTargetType: 'date_time',
        timerAt: '2026-07-01T09:00:00',
        timerTimezone: '',
        timerTzMinutes: '600',
        timerDirection: 'up',
        timerOnTarget: 'continue',
        timerFormat: 'hh_mm_ss_ff',
      }),
    );
    expect(body).toMatchObject({
      kind: 'timer',
      target: 'date_time',
      at: '2026-07-01T09:00:00',
      tz_offset_minutes: 600,
      direction: 'up',
      on_target: 'continue',
      format: 'hh_mm_ss_ff',
    });
    // recur_daily is a time_of_day-only field — never on a date_time body.
    expect(body).not.toHaveProperty('recur_daily');
    expect(body).not.toHaveProperty('timezone');
  });

  it('uses the schema defaults on a fresh timer form, badge on by default', () => {
    const body = sourceFormToBody(sourceForm({ kind: 'timer', timerAt: '00:00:00' }));
    expect(body).toMatchObject({
      kind: 'timer',
      target: 'time_of_day',
      direction: 'down',
      on_target: 'hold',
      format: 'd_hh_mm_ss',
      overrun_badge: true,
    });
    expect(body).not.toHaveProperty('label');
    expect(body).not.toHaveProperty('overrun_prefix');
  });

  it('round-trips a stored time-of-day timer body exactly', () => {
    const record = {
      id: 'onair',
      name: 'On air in',
      body: {
        id: 'onair',
        kind: 'timer',
        target: 'time_of_day',
        at: '14:30:00',
        timezone: 'Australia/Sydney',
        recur_daily: true,
        direction: 'down',
        on_target: 'recur',
        format: 'mm_ss',
        label: 'ON AIR IN',
        overrun_prefix: 'OVER ',
        overrun_badge: false,
      },
    };
    const form = defined(sourceFormFromRecord(record));
    expect(form.kind).toBe('timer');
    expect(form.timerTargetType).toBe('time_of_day');
    expect(form.timerAt).toBe('14:30:00');
    expect(form.timerTimezone).toBe('Australia/Sydney');
    expect(form.timerRecurDaily).toBe(true);
    expect(form.timerDirection).toBe('down');
    expect(form.timerOnTarget).toBe('recur');
    expect(form.timerFormat).toBe('mm_ss');
    expect(form.timerLabel).toBe('ON AIR IN');
    expect(form.timerOverrunPrefix).toBe('OVER ');
    expect(form.timerOverrunBadge).toBe(false);
    // The re-emitted body is byte-shape stable for the managed keys.
    expect(sourceFormToBody(form)).toMatchObject({
      kind: 'timer',
      target: 'time_of_day',
      at: '14:30:00',
      timezone: 'Australia/Sydney',
      recur_daily: true,
      direction: 'down',
      on_target: 'recur',
      format: 'mm_ss',
      label: 'ON AIR IN',
      overrun_prefix: 'OVER ',
      overrun_badge: false,
    });
  });

  it('round-trips a stored date+time timer body exactly (fixed offset)', () => {
    const record = {
      id: 'launch',
      name: 'Launch',
      body: {
        id: 'launch',
        kind: 'timer',
        target: 'date_time',
        at: '2026-07-01T09:00:00',
        tz_offset_minutes: 600,
        direction: 'down',
        on_target: 'zero_then_up',
        format: 'auto',
      },
    };
    const form = defined(sourceFormFromRecord(record));
    expect(form.timerTargetType).toBe('date_time');
    expect(form.timerAt).toBe('2026-07-01T09:00:00');
    expect(form.timerTimezone).toBe('');
    expect(form.timerTzMinutes).toBe('600');
    expect(form.timerOnTarget).toBe('zero_then_up');
    expect(form.timerFormat).toBe('auto');
    // Absent overrun_badge in a stored body means "draw it" (schema default true).
    expect(form.timerOverrunBadge).toBe(true);
  });

  it('treats an absent overrun_badge as the schema default (true)', () => {
    const form = defined(
      sourceFormFromRecord({
        id: 't',
        name: 'T',
        body: { id: 't', kind: 'timer', target: 'time_of_day', at: '00:00:00' },
      }),
    );
    expect(form.timerOverrunBadge).toBe(true);
  });

  it('validates the `at` string per target type', () => {
    expect(
      validateSourceForm(sourceForm({ kind: 'timer', timerAt: '' }), true).timerAt,
    ).toBe('required');
    expect(
      validateSourceForm(
        sourceForm({ kind: 'timer', timerTargetType: 'time_of_day', timerAt: '25:00:00' }),
        true,
      ).timerAt,
    ).toBe('time-of-day');
    expect(
      validateSourceForm(
        sourceForm({ kind: 'timer', timerTargetType: 'time_of_day', timerAt: '14:30:00' }),
        true,
      ).timerAt,
    ).toBeUndefined();
    expect(
      validateSourceForm(
        sourceForm({ kind: 'timer', timerTargetType: 'date_time', timerAt: '2026-02-30T09:00:00' }),
        true,
      ).timerAt,
    ).toBe('date-time');
    expect(
      validateSourceForm(
        sourceForm({ kind: 'timer', timerTargetType: 'date_time', timerAt: '2026-07-01T09:00:00' }),
        true,
      ).timerAt,
    ).toBeUndefined();
  });

  it('validates the timer timezone (mirrors the clock)', () => {
    expect(
      validateSourceForm(
        sourceForm({ kind: 'timer', timerAt: '14:30:00', timerTimezone: 'Mars/Olympus' }),
        true,
      ).timerTimezone,
    ).toBe('timezone');
    expect(
      validateSourceForm(
        sourceForm({ kind: 'timer', timerAt: '14:30:00', timerTimezone: 'UTC' }),
        true,
      ).timerTimezone,
    ).toBeUndefined();
  });
});

describe('withTimerTargetType', () => {
  it('drops the recur policy + recur_daily when switching to a date+time target', () => {
    const tod = sourceForm({
      kind: 'timer',
      timerTargetType: 'time_of_day',
      timerAt: '14:30:00',
      timerRecurDaily: true,
      timerOnTarget: 'recur',
    });
    const dt = withTimerTargetType(tod, 'date_time');
    expect(dt.timerTargetType).toBe('date_time');
    expect(dt.timerAt).toBe('');
    expect(dt.timerRecurDaily).toBe(false);
    expect(dt.timerOnTarget).toBe('hold');
  });

  it('keeps a non-recur policy when switching target type', () => {
    const tod = sourceForm({
      kind: 'timer',
      timerTargetType: 'time_of_day',
      timerOnTarget: 'continue',
    });
    expect(withTimerTargetType(tod, 'date_time').timerOnTarget).toBe('continue');
  });

  it('is a no-op when the target type is unchanged', () => {
    const form = sourceForm({ kind: 'timer', timerTargetType: 'time_of_day', timerAt: '14:30:00' });
    expect(withTimerTargetType(form, 'time_of_day')).toBe(form);
  });
});

describe('timezone / time validators', () => {
  it('isKnownTimezone accepts UTC and rejects garbage when a tz db is present', () => {
    expect(isKnownTimezone('UTC')).toBe(true);
    expect(isKnownTimezone('Mars/Olympus')).toBe(false);
    expect(isKnownTimezone('')).toBe(false);
  });

  it('isValidTimeOfDay enforces HH:MM:SS 24-hour', () => {
    expect(isValidTimeOfDay('00:00:00')).toBe(true);
    expect(isValidTimeOfDay('23:59:59')).toBe(true);
    expect(isValidTimeOfDay('24:00:00')).toBe(false);
    expect(isValidTimeOfDay('14:60:00')).toBe(false);
    expect(isValidTimeOfDay('14:30')).toBe(false);
  });

  it('isValidLocalDateTime enforces a real calendar date+time', () => {
    expect(isValidLocalDateTime('2026-07-01T09:00:00')).toBe(true);
    expect(isValidLocalDateTime('2026-07-01 09:00:00')).toBe(true);
    expect(isValidLocalDateTime('2026-02-30T09:00:00')).toBe(false);
    expect(isValidLocalDateTime('2026-13-01T09:00:00')).toBe(false);
    expect(isValidLocalDateTime('2026-07-01T09:00')).toBe(false);
  });
});

describe('validateSourceForm', () => {
  it('requires id on create and name always', () => {
    expect(validateSourceForm(sourceForm({ id: '' }), true).id).toBe('required');
    expect(validateSourceForm(sourceForm({ id: '' }), false).id).toBeUndefined();
    expect(validateSourceForm(sourceForm({ name: '' }), true).name).toBe('required');
  });

  it('validates the locator per kind scheme', () => {
    expect(
      validateSourceForm(sourceForm({ kind: 'rtsp', url: 'http://h/x' }), true).url,
    ).toBe('scheme-rtsp');
    expect(
      validateSourceForm(sourceForm({ kind: 'srt', url: 'rtsp://h/x' }), true).url,
    ).toBe('scheme-srt');
    expect(
      validateSourceForm(sourceForm({ kind: 'hls', url: 'rtsp://h/x' }), true).url,
    ).toBe('scheme-http');
    expect(
      validateSourceForm(sourceForm({ kind: 'youtube', url: 'ftp://h/x' }), true).url,
    ).toBe('scheme-http');
    expect(
      validateSourceForm(sourceForm({ kind: 'rtmp', url: 'srt://h:1' }), true).url,
    ).toBe('scheme-rtmp');
    expect(
      validateSourceForm(sourceForm({ kind: 'ts', url: 'not a url' }), true).url,
    ).toBe('url-invalid');
    expect(
      validateSourceForm(
        sourceForm({ kind: 'rtsp', url: 'rtsp://[2001:db8::1]:8554/cam' }),
        true,
      ).url,
    ).toBeUndefined();
  });

  it('requires the ndi name / file path', () => {
    expect(validateSourceForm(sourceForm({ kind: 'ndi' }), true).ndiName).toBe('required');
    expect(validateSourceForm(sourceForm({ kind: 'file' }), true).path).toBe('required');
  });

  it('checks the solid colour and the clock tz range', () => {
    expect(
      validateSourceForm(sourceForm({ kind: 'solid', color: 'red' }), true).color,
    ).toBe('hex-color');
    expect(
      validateSourceForm(sourceForm({ kind: 'clock', clockTzMinutes: '900' }), true)
        .clockTzMinutes,
    ).toBe('int-range');
    expect(
      validateSourceForm(sourceForm({ kind: 'clock', clockTzMinutes: '600' }), true)
        .clockTzMinutes,
    ).toBeUndefined();
  });

  it('checks the caption mode parameters', () => {
    expect(
      validateSourceForm(
        sourceForm({ kind: 'bars', captionsMode: 'teletext_page', captionsPage: '42' }),
        true,
      ).captionsPage,
    ).toBe('int-range');
    expect(
      validateSourceForm(
        sourceForm({ kind: 'bars', captionsMode: 'track', captionsTrack: '' }),
        true,
      ).captionsTrack,
    ).toBe('required');
  });

  it('requires a stable id when a GPU pin is enabled', () => {
    expect(
      validateSourceForm(
        sourceForm({ kind: 'bars', gpuPinEnabled: true, gpuPinStableId: '  ' }),
        true,
      ).gpuPinStableId,
    ).toBe('required');
  });

  it('passes a complete, correct form', () => {
    expect(
      validateSourceForm(sourceForm({ kind: 'rtsp', url: 'rtsp://h/x' }), true),
    ).toEqual({});
  });
});

describe('outputFormToBody', () => {
  it('builds an rtsp_server body (mount + codec + optional latency profile)', () => {
    const body = outputFormToBody(
      outputForm({ kind: 'rtsp', mount: '/multiview', codec: 'h264', latencyProfile: 'low_latency' }),
    );
    expect(body).toEqual({
      kind: 'rtsp_server',
      mount: '/multiview',
      codec: 'h264',
      latency_profile: 'low_latency',
    });
  });

  it('NEVER writes the routable config id from the store id', () => {
    // The output config-level `id` is OPTIONAL, label-derived when absent, and
    // a DIFFERENT namespace from the resource/store id (seeded stores use
    // `output-0..n`): crosspoints/OutputRefs bind to the config id, so writing
    // the store id over it would silently re-route. The form must not emit it.
    const body = outputFormToBody(
      outputForm({ id: 'output-0', kind: 'hls', path: '/hls/m', codec: 'h264' }),
    );
    expect(body).not.toHaveProperty('id');
  });

  it('preserves an AUTHORED config-level id verbatim across an edit', () => {
    const form = defined(
      outputFormFromRecord({
        id: 'output-0',
        name: 'Program HLS',
        body: { kind: 'hls', id: 'svc-main', path: '/hls/m', codec: 'h264' },
      }),
    );
    // The form addresses the STORE record…
    expect(form.id).toBe('output-0');
    // …while the routable config id rides the extra-preservation path intact.
    const body = outputFormToBody(form);
    expect(body.id).toBe('svc-main');
  });

  it('builds an ll_hls body with part/segment/gop in ms', () => {
    const body = outputFormToBody(
      outputForm({
        kind: 'll-hls',
        path: '/hls/multiview',
        codec: 'h264',
        partTargetMs: '200',
        segmentMs: '2000',
        gopMs: '1000',
      }),
    );
    expect(body).toEqual({
      kind: 'll_hls',
      path: '/hls/multiview',
      codec: 'h264',
      part_target_ms: 200,
      segment_ms: 2000,
      gop_ms: 1000,
    });
  });

  it('omits the optional durations when blank', () => {
    const body = outputFormToBody(
      outputForm({ kind: 'hls', path: '/hls/multiview', codec: 'h264' }),
    );
    expect(body).toEqual({ kind: 'hls', path: '/hls/multiview', codec: 'h264' });
  });

  it('builds hls with segment_ms, ndi by name (no codec), rtmp/srt by url', () => {
    expect(
      outputFormToBody(outputForm({ kind: 'hls', path: '/hls/m', codec: 'hevc', segmentMs: '4000' })),
    ).toMatchObject({ kind: 'hls', segment_ms: 4000 });
    const ndi = outputFormToBody(outputForm({ kind: 'ndi', ndiName: 'Multiview PGM' }));
    expect(ndi).toEqual({ kind: 'ndi', name: 'Multiview PGM' });
    expect(
      outputFormToBody(outputForm({ kind: 'rtmp', url: 'rtmp://i.example/app/key', codec: 'h264' })),
    ).toEqual({ kind: 'rtmp', url: 'rtmp://i.example/app/key', codec: 'h264' });
    expect(
      outputFormToBody(outputForm({ kind: 'srt', url: 'srt://[2001:db8::1]:7001', codec: 'h264' })),
    ).toEqual({ kind: 'srt', url: 'srt://[2001:db8::1]:7001', codec: 'h264' });
  });

  it('carries the audio selection and gpu pin per schema when set', () => {
    const program = outputFormToBody(
      outputForm({ kind: 'hls', path: '/h', codec: 'h264', audioMode: 'program' }),
    );
    expect(program.audio).toEqual({ mode: 'program', tracks: [] });

    const tracks = outputFormToBody(
      outputForm({
        kind: 'hls',
        path: '/h',
        codec: 'h264',
        audioMode: 'tracks',
        audioTracks: 'prog, commentary',
        gpuPinEnabled: true,
        gpuPinVendor: 'intel',
        gpuPinStableId: '0000:00:02.0',
      }),
    );
    expect(tracks.audio).toEqual({ mode: 'tracks', tracks: ['prog', 'commentary'] });
    expect(tracks.gpu_pin).toEqual({ vendor: 'intel', stable_id: '0000:00:02.0' });
  });
});

describe('outputFormFromRecord', () => {
  it('refuses an unknown kind with the undefined sentinel (never folds)', () => {
    expect(
      outputFormFromRecord({
        id: 'out-x',
        name: 'Mystery',
        body: { kind: 'aes67', url: 'aes67://x' },
      }),
    ).toBeUndefined();
  });

  it('round-trips an ll_hls record including the advanced fields', () => {
    const form = defined(outputFormFromRecord({
      id: 'llh',
      name: 'LL-HLS',
      body: {
        kind: 'll_hls',
        id: 'llh',
        path: '/hls/m',
        codec: 'hevc',
        part_target_ms: 250,
        segment_ms: 2000,
        gop_ms: 1000,
        audio: { mode: 'tracks', tracks: ['prog'] },
        gpu_pin: { vendor: 'nvidia', stable_id: 'GPU-1' },
      },
    }));
    expect(form.kind).toBe('ll-hls');
    expect(form.path).toBe('/hls/m');
    expect(form.codec).toBe('hevc');
    expect(form.partTargetMs).toBe('250');
    expect(form.segmentMs).toBe('2000');
    expect(form.gopMs).toBe('1000');
    expect(form.audioMode).toBe('tracks');
    expect(form.audioTracks).toBe('prog');
    expect(form.gpuPinEnabled).toBe(true);
    expect(outputFormToBody(form)).toMatchObject({
      kind: 'll_hls',
      part_target_ms: 250,
      audio: { mode: 'tracks', tracks: ['prog'] },
    });
  });
});

describe('validateOutputForm', () => {
  it('requires the per-kind destination', () => {
    expect(validateOutputForm(outputForm({ kind: 'rtsp', codec: 'h264' }), true).mount).toBe(
      'required',
    );
    expect(
      validateOutputForm(outputForm({ kind: 'rtsp', mount: 'multiview', codec: 'h264' }), true)
        .mount,
    ).toBe('mount-slash');
    expect(validateOutputForm(outputForm({ kind: 'hls', codec: 'h264' }), true).path).toBe(
      'required',
    );
    expect(validateOutputForm(outputForm({ kind: 'ndi' }), true).ndiName).toBe('required');
    expect(
      validateOutputForm(outputForm({ kind: 'rtmp', url: 'srt://h:1', codec: 'h264' }), true).url,
    ).toBe('scheme-rtmp');
    expect(
      validateOutputForm(outputForm({ kind: 'srt', url: 'rtmp://h/a', codec: 'h264' }), true).url,
    ).toBe('scheme-srt');
  });

  it('requires a codec on the codec-bearing kinds only', () => {
    expect(
      validateOutputForm(outputForm({ kind: 'hls', path: '/h', codec: '' }), true).codec,
    ).toBe('required');
    expect(
      validateOutputForm(outputForm({ kind: 'ndi', ndiName: 'X', codec: '' }), true).codec,
    ).toBeUndefined();
  });

  it('rejects non-positive-integer durations and a trackless tracks mode', () => {
    expect(
      validateOutputForm(
        outputForm({ kind: 'll-hls', path: '/h', codec: 'h264', partTargetMs: '-2' }),
        true,
      ).partTargetMs,
    ).toBe('positive-int');
    expect(
      validateOutputForm(
        outputForm({ kind: 'hls', path: '/h', codec: 'h264', segmentMs: 'abc' }),
        true,
      ).segmentMs,
    ).toBe('positive-int');
    expect(
      validateOutputForm(
        outputForm({ kind: 'hls', path: '/h', codec: 'h264', audioMode: 'tracks', audioTracks: ' ' }),
        true,
      ).audioTracks,
    ).toBe('tracks-required');
  });
});

describe('OUTPUT_RUNNABLE', () => {
  it('mirrors build_outputs in multiview-cli pipeline.rs', () => {
    // hls / ll_hls / rtmp / srt are runnable today; rtsp_server and ndi are
    // accepted by config but warned + skipped by build_outputs; display is
    // built by build_outputs ONLY in a `display-kms` build (a default build
    // fails the run with a clear error rather than skipping it — DEV-B1).
    expect(OUTPUT_RUNNABLE.hls).toBe('runnable');
    expect(OUTPUT_RUNNABLE['ll-hls']).toBe('runnable');
    expect(OUTPUT_RUNNABLE.rtmp).toBe('runnable');
    expect(OUTPUT_RUNNABLE.srt).toBe('runnable');
    expect(OUTPUT_RUNNABLE.rtsp).toBe('unbuilt');
    expect(OUTPUT_RUNNABLE.ndi).toBe('unbuilt');
    expect(OUTPUT_RUNNABLE.display).toBe('requires-feature');
  });
});

describe('display output form (DEV-B1 / ADR-0044)', () => {
  it('builds a minimal display body (connector only, no codec field)', () => {
    const body = outputFormToBody(outputForm({ kind: 'display', connector: 'DP-1' }));
    expect(body).toEqual({ kind: 'display', connector: 'DP-1' });
  });

  it('builds the mode override table from the exact-rational refresh', () => {
    const body = outputFormToBody(
      outputForm({
        kind: 'display',
        connector: 'HDMI-A-1',
        displayModeChoice: 'override',
        displayModeWidth: '1920',
        displayModeHeight: '1080',
        displayModeRefresh: '60000/1001',
      }),
    );
    expect(body).toEqual({
      kind: 'display',
      connector: 'HDMI-A-1',
      mode: { width: 1920, height: 1080, refresh: '60000/1001' },
    });
  });

  it('builds the forced_mode table for EDID-less heads, normalizing integer refresh', () => {
    const body = outputFormToBody(
      outputForm({
        kind: 'display',
        connector: 'DP-2',
        displayModeChoice: 'forced',
        displayModeWidth: '1920',
        displayModeHeight: '1080',
        displayModeRefresh: '50',
      }),
    );
    expect(body).toEqual({
      kind: 'display',
      connector: 'DP-2',
      forced_mode: { width: 1920, height: 1080, refresh: '50/1' },
    });
  });

  it('round-trips a stored display record including mode + advanced fields', () => {
    const form = defined(
      outputFormFromRecord({
        id: 'output-3',
        name: 'Left monitor',
        body: {
          kind: 'display',
          connector: 'DP-1',
          mode: { width: 1920, height: 1080, refresh: '60000/1001' },
          audio: { mode: 'program', tracks: [] },
          gpu_pin: { vendor: 'amd', stable_id: '0000:00:01.0' },
        },
      }),
    );
    expect(form.kind).toBe('display');
    expect(form.connector).toBe('DP-1');
    expect(form.displayModeChoice).toBe('override');
    expect(form.displayModeWidth).toBe('1920');
    expect(form.displayModeHeight).toBe('1080');
    expect(form.displayModeRefresh).toBe('60000/1001');
    expect(form.audioMode).toBe('program');
    expect(form.gpuPinEnabled).toBe(true);
    expect(outputFormToBody(form)).toMatchObject({
      kind: 'display',
      connector: 'DP-1',
      mode: { width: 1920, height: 1080, refresh: '60000/1001' },
      audio: { mode: 'program', tracks: [] },
      gpu_pin: { vendor: 'amd', stable_id: '0000:00:01.0' },
    });
  });

  it('round-trips a forced_mode record onto the forced choice', () => {
    const form = defined(
      outputFormFromRecord({
        id: 'output-4',
        name: 'EDID-less head',
        body: {
          kind: 'display',
          connector: 'DP-2',
          forced_mode: { width: 1920, height: 1080, refresh: '50/1' },
        },
      }),
    );
    expect(form.displayModeChoice).toBe('forced');
    expect(form.displayModeRefresh).toBe('50/1');
    expect(outputFormToBody(form)).toEqual({
      kind: 'display',
      connector: 'DP-2',
      forced_mode: { width: 1920, height: 1080, refresh: '50/1' },
    });
  });

  it('validates the connector and the mode fields', () => {
    expect(
      validateOutputForm(outputForm({ kind: 'display', connector: '  ' }), true).connector,
    ).toBe('required');
    const bad = validateOutputForm(
      outputForm({
        kind: 'display',
        connector: 'DP-1',
        displayModeChoice: 'override',
        displayModeWidth: '0',
        displayModeHeight: '',
        displayModeRefresh: '59.94',
      }),
      true,
    );
    expect(bad.displayModeWidth).toBe('positive-int');
    expect(bad.displayModeHeight).toBe('positive-int');
    // A float refresh is rejected: refresh is an exact-rational string
    // (`60000/1001`) or a bare integer, never a float (invariant #3).
    expect(bad.displayModeRefresh).toBe('rational-fps');
    // A display output never carries a codec; codec validation must not fire.
    expect(bad.codec).toBeUndefined();
  });

  it('passes a complete, correct display form', () => {
    expect(
      validateOutputForm(
        outputForm({
          kind: 'display',
          connector: 'auto',
          displayModeChoice: 'forced',
          displayModeWidth: '1920',
          displayModeHeight: '1080',
          displayModeRefresh: '60000/1001',
        }),
        true,
      ),
    ).toEqual({});
  });
});

describe('overlayFormToBody', () => {
  it('builds a clock overlay with its consumed params (face / tz / placement)', () => {
    const body = overlayFormToBody(
      overlayForm({
        kind: 'clock',
        target: 'canvas',
        z: '100',
        clockFace: 'analog',
        clockTzMinutes: '600',
        clockX: '1800',
        clockY: '1000',
        clockRadius: '120',
      }),
    );
    expect(body).toEqual({
      id: 'ov1',
      kind: 'clock',
      target: 'canvas',
      z: 100,
      face: 'analog',
      tz_minutes: 600,
      x: 1800,
      y: 1000,
      radius: 120,
    });
  });

  it('omits unset optional clock params', () => {
    const body = overlayFormToBody(
      overlayForm({ kind: 'clock', target: 'canvas', z: '10', clockFace: 'digital' }),
    );
    expect(body).toEqual({ id: 'ov1', kind: 'clock', target: 'canvas', z: 10, face: 'digital' });
  });

  it('builds a tally_border overlay (width_px / color / binding)', () => {
    const body = overlayFormToBody(
      overlayForm({
        kind: 'tally_border',
        target: 'cell_big',
        z: '50',
        tallyWidthPx: '6',
        tallyColor: '#FF0000',
        tallyBinding: 'tally://cell_big',
      }),
    );
    expect(body).toEqual({
      id: 'ov1',
      kind: 'tally_border',
      target: 'cell_big',
      z: 50,
      width_px: 6,
      color: '#FF0000',
      binding: 'tally://cell_big',
    });
  });

  it('builds label / image / subtitle with no invented params', () => {
    for (const kind of ['label', 'image', 'subtitle'] as const) {
      const body = overlayFormToBody(overlayForm({ kind, target: 'canvas', z: '1' }));
      expect(body).toEqual({ id: 'ov1', kind, target: 'canvas', z: 1 });
    }
  });

  it('preserves verbatim params it does not render when the kind is unchanged', () => {
    const record = {
      id: 'ov_clock',
      name: 'Clock',
      body: {
        id: 'ov_clock',
        kind: 'clock',
        target: 'canvas',
        z: 100,
        face: 'analog',
        format: '%H:%M:%S',
        anchor: 'bottom_right',
        offset: { x: -20, y: -16 },
      },
    };
    const form = defined(overlayFormFromRecord(record));
    const body = overlayFormToBody(form);
    expect(body.format).toBe('%H:%M:%S');
    expect(body.anchor).toBe('bottom_right');
    expect(body.offset).toEqual({ x: -20, y: -16 });
  });

  it('preserves another kind\'s param NAMES on a label overlay (kind-scoped stripping)', () => {
    // `color` and `x` are managed keys for tally_border/clock — but on a LABEL
    // overlay the writer never re-emits them, so stripping them would lose the
    // authored document on every edit. Stripping must be scoped to the keys the
    // CURRENT kind's writer actually re-emits.
    const record = {
      id: 'ov_label',
      name: 'Label',
      body: {
        id: 'ov_label',
        kind: 'label',
        target: 'canvas',
        z: 10,
        color: '#00FF00',
        x: 42,
      },
    };
    const form = defined(overlayFormFromRecord(record));
    const body = overlayFormToBody(form);
    expect(body.color).toBe('#00FF00');
    expect(body.x).toBe(42);
    expect(body).toEqual({
      id: 'ov_label',
      kind: 'label',
      target: 'canvas',
      z: 10,
      color: '#00FF00',
      x: 42,
    });
  });

  it('drops the previous kind params when the kind switches', () => {
    const record = {
      id: 'ov_clock',
      name: 'Clock',
      body: { id: 'ov_clock', kind: 'clock', target: 'canvas', z: 100, face: 'analog', format: '%H' },
    };
    const switched = withOverlayKind(defined(overlayFormFromRecord(record)), 'label');
    const body = overlayFormToBody(switched);
    expect(body).toEqual({ id: 'ov_clock', kind: 'label', target: 'canvas', z: 100 });
  });
});

describe('overlayFormFromRecord', () => {
  it('refuses an unknown kind with the undefined sentinel (never folds)', () => {
    expect(
      overlayFormFromRecord({
        id: 'ov-x',
        name: 'Mystery',
        body: { id: 'ov-x', kind: 'scoreboard', target: 'canvas', z: 1 },
      }),
    ).toBeUndefined();
  });
});

describe('validateOverlayForm', () => {
  it('requires id (create) / name / target and an integer z', () => {
    expect(validateOverlayForm(overlayForm({ id: '' }), true).id).toBe('required');
    expect(validateOverlayForm(overlayForm({ target: '' }), true).target).toBe('required');
    expect(validateOverlayForm(overlayForm({ z: 'x' }), true).z).toBe('int');
  });

  it('checks the tally border params', () => {
    expect(
      validateOverlayForm(
        overlayForm({ kind: 'tally_border', tallyWidthPx: '0' }),
        true,
      ).tallyWidthPx,
    ).toBe('positive-int');
    expect(
      validateOverlayForm(
        overlayForm({ kind: 'tally_border', tallyWidthPx: '4', tallyColor: 'red' }),
        true,
      ).tallyColor,
    ).toBe('hex-color');
  });

  it('checks the clock numeric params', () => {
    expect(
      validateOverlayForm(overlayForm({ kind: 'clock', clockTzMinutes: '1.5' }), true)
        .clockTzMinutes,
    ).toBe('int');
    expect(
      validateOverlayForm(overlayForm({ kind: 'clock', clockRadius: '-3' }), true).clockRadius,
    ).toBe('positive-int');
  });
});
