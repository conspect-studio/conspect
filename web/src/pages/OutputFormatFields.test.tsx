// OutputFormatFields (#342 B3, #345 part F3): presets, width/height/fit/anchor
// into the stored `format`, the video-slot bitrate into `slots.video[0]`,
// read-only audio slots, the subtitle slot editor (`slots.subtitle`),
// validation, capability messages, and the live-reset vs restart warning per
// kind.
import { useState } from 'react';
import type { JSX } from 'react';
import { describe, expect, it } from 'vitest';
import { fireEvent, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders } from '../test/render';
import { emptyOutputForm, outputFormToBody } from '../resources/forms';
import type { OutputFormState } from '../resources/forms';
import { OutputFormatFields } from './OutputFormatFields';
import {
  formatError,
  resetsLive,
  subtitleCapabilityOf,
  subtitleSlotError,
  withBitrate,
  withFormat,
  withSubtitleSlots,
} from '../resources/outputFormat';
import type { SubtitleSlotRow } from '../resources/outputFormat';

/** A change handler for renders that do not inspect the form. */
function ignore(next: OutputFormState): void {
  void next;
}

function base(patch: Partial<OutputFormState> = {}): OutputFormState {
  return { ...emptyOutputForm(), id: 'out-1', name: 'Out', path: '/srv/hls', codec: 'h264', ...patch };
}

function Harness({
  initial,
  editing,
  onChange,
  sources,
}: {
  readonly initial: OutputFormState;
  readonly editing: boolean;
  readonly onChange: (next: OutputFormState) => void;
  readonly sources?: readonly { readonly id: string; readonly label: string }[];
}): JSX.Element {
  const [form, setForm] = useState(initial);
  return (
    <OutputFormatFields
      form={form}
      editing={editing}
      sources={sources}
      setForm={(next): void => {
        setForm(next);
        onChange(next);
      }}
    />
  );
}

describe('OutputFormatFields', () => {
  it('writes the vertical cover preset into the stored format', () => {
    let latest = base();
    renderWithProviders(
      <Harness
        initial={latest}
        editing
        onChange={(next): void => {
          latest = next;
        }}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: /1080×1920 cover centre/ }));
    expect(outputFormToBody(latest).format).toEqual({
      width: 1080,
      height: 1920,
      fit: { mode: 'cover' },
    });
    fireEvent.click(screen.getByRole('button', { name: /Program canvas/ }));
    expect(outputFormToBody(latest).format).toBeUndefined();
  });

  it('edits width and height and keeps an anchor other than centre', () => {
    const next = withFormat(base(), { width: '1280', height: '720', mode: 'contain', anchor: 'top' });
    expect(next.extra.format).toEqual({ width: 1280, height: 720, fit: { mode: 'contain', anchor: 'top' } });
    expect(formatError(next)).toBeUndefined();
    expect(formatError(withFormat(base(), { width: '1281', height: '720', mode: 'contain', anchor: 'center' }))).toBe('size');
    expect(formatError(withFormat(base(), { width: '1280', height: '', mode: 'contain', anchor: 'center' }))).toBe('size');
    // The area cap (3840 × 2160), as the config enforces it.
    expect(formatError(withFormat(base(), { width: '3840', height: '2160', mode: 'contain', anchor: 'center' }))).toBeUndefined();
    expect(formatError(withFormat(base(), { width: '8192', height: '8192', mode: 'contain', anchor: 'center' }))).toBe('size');
    expect(formatError(withFormat(base(), { width: '2160', height: '3842', mode: 'contain', anchor: 'center' }))).toBe('size');
  });

  it('writes the video slot bitrate and removes an emptied slot list', () => {
    const set = withBitrate(base(), '6000');
    expect(outputFormToBody(set).slots).toEqual({ video: [{ bitrate_kbps: 6000 }] });
    expect(outputFormToBody(withBitrate(set, '')).slots).toBeUndefined();
    expect(formatError(withBitrate(base(), '0'))).toBe('bitrate');
    expect(formatError(withBitrate(base(), '1000001'))).toBe('bitrate');
    // Audio slots survive a bitrate edit untouched.
    const withAudio = base({ extra: { slots: { audio: [{ language: 'eng' }, { track: 'spa', language: 'spa' }] } } });
    expect(outputFormToBody(withBitrate(withAudio, '5000')).slots).toEqual({
      audio: [{ language: 'eng' }, { track: 'spa', language: 'spa' }],
      video: [{ bitrate_kbps: 5000 }],
    });
  });

  it('lists audio slots read-only', () => {
    renderWithProviders(
      <Harness
        initial={base({ extra: { slots: { audio: [{ language: 'eng' }, { track: 'spa', language: 'spa' }] } } })}
        editing
        onChange={ignore}
      />,
    );
    const list = screen.getByRole('list', { name: /Audio slots/ });
    expect(list.textContent).toContain('Audio slot 0: program mix');
    expect(list.textContent).toContain('Audio slot 1: track spa');
    expect(list.textContent).toContain('(spa)');
  });

  it('warns of a live reset on HLS and of a restart elsewhere (SRT/RIST included)', () => {
    expect(resetsLive('hls')).toBe(true);
    expect(resetsLive('srt')).toBe(false);
    expect(resetsLive('rist')).toBe(false);
    expect(resetsLive('ll-hls')).toBe(false);
    expect(resetsLive('rtmp')).toBe(false);
    const { unmount } = renderWithProviders(
      <Harness initial={base({ kind: 'hls' })} editing onChange={ignore} />,
    );
    expect(screen.getByTestId('output-format-warning').textContent).toContain(
      'Applying resets this output (about 1 s); Program and other outputs are not affected',
    );
    unmount();
    renderWithProviders(<Harness initial={base({ kind: 'rtmp', url: 'rtmp://x' })} editing onChange={ignore} />);
    expect(screen.getByTestId('output-format-warning').textContent).toContain('Applies on the next restart');
  });

  it('is hidden for raw outputs and shows no warning when creating', () => {
    const { container, unmount } = renderWithProviders(
      <Harness initial={base({ kind: 'ndi' })} editing onChange={ignore} />,
    );
    expect(container.querySelector('[data-testid="output-format-fields"]')).toBeNull();
    unmount();
    renderWithProviders(<Harness initial={base()} editing={false} onChange={ignore} />);
    expect(screen.queryByTestId('output-format-warning')).toBeNull();
  });

  describe('subtitle slots (#345 part F3)', () => {
    const SOURCES = [{ id: 'cam-1', label: 'Camera 1' }, { id: 'cam-2', label: 'Camera 2' }];

    it('round-trips a row through extra.slots.subtitle', () => {
      const withRow = withSubtitleSlots(base(), [
        { format: 'webvtt', language: 'en', name: '', source: 'cam-1' },
      ]);
      expect(outputFormToBody(withRow).slots).toEqual({
        subtitle: [{ format: 'webvtt', language: 'en', source: 'cam-1' }],
      });
      // An empty row list removes `slots.subtitle` (and `slots` itself when
      // nothing else remains), same convention as the video/audio slots.
      expect(outputFormToBody(withSubtitleSlots(withRow, [])).slots).toBeUndefined();
    });

    it('capability: hls builds webvtt; ll-hls/srt/rist/rtmp carry but do not build; others carry none', () => {
      expect(subtitleCapabilityOf('hls')).toEqual({ max: 8, mediumFormats: ['webvtt'], builtFormats: ['webvtt'] });
      expect(subtitleCapabilityOf('ll-hls').builtFormats).toEqual([]);
      expect(subtitleCapabilityOf('ll-hls').mediumFormats).toEqual(['webvtt']);
      expect(subtitleCapabilityOf('srt').mediumFormats).toEqual(['cea608', 'dvb']);
      expect(subtitleCapabilityOf('srt').builtFormats).toEqual([]);
      expect(subtitleCapabilityOf('rist').mediumFormats).toEqual(['cea608', 'dvb']);
      expect(subtitleCapabilityOf('rtmp').mediumFormats).toEqual(['cea608', 'dvb']);
      expect(subtitleCapabilityOf('ndi')).toEqual({ max: 0, mediumFormats: [], builtFormats: [] });
      expect(subtitleCapabilityOf('display').max).toBe(0);
      expect(subtitleCapabilityOf('webrtc').max).toBe(0);
      expect(subtitleCapabilityOf('rtsp').max).toBe(0);
    });

    it('validates language, name and duplicate effective names like the server', () => {
      const rows: SubtitleSlotRow[] = [
        { format: 'webvtt', language: 'en', name: '', source: undefined },
        { format: 'webvtt', language: 'en', name: '', source: undefined },
      ];
      expect(subtitleSlotError('hls', rows, 0)).toBe('duplicate-name');
      expect(subtitleSlotError('hls', rows, 1)).toBe('duplicate-name');
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: 'not valid!', name: '', source: undefined }], 0),
      ).toBe('language');
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: 'a'.repeat(36), name: '', source: undefined }], 0),
      ).toBe('language');
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: 'a'.repeat(65), source: undefined }], 0),
      ).toBe('name');
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: 'has "quote"', source: undefined }], 0),
      ).toBe('name');
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: 'en', name: 'ok', source: undefined }], 0),
      ).toBeUndefined();
    });

    it('format capability: unsupported-yet vs medium-mismatch', () => {
      // ll_hls carries webvtt but does not build it yet: slot-format-unsupported.
      expect(
        subtitleSlotError('ll-hls', [{ format: 'webvtt', language: '', name: '', source: undefined }], 0),
      ).toBe('format');
      // srt carries cea608/dvb but does not build them yet.
      expect(
        subtitleSlotError('srt', [{ format: 'cea608', language: '', name: '', source: undefined }], 0),
      ).toBe('format');
      // srt's medium does not carry webvtt at all: slot-medium-mismatch.
      expect(
        subtitleSlotError('srt', [{ format: 'webvtt', language: '', name: '', source: undefined }], 0),
      ).toBe('format');
      // hls builds webvtt: valid.
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: '', source: undefined }], 0),
      ).toBeUndefined();
    });

    it('renders rows, add/remove, and capability-aware refusal text with follow-up issue numbers', () => {
      renderWithProviders(
        <Harness
          initial={base({
            kind: 'hls',
            extra: { slots: { subtitle: [{ format: 'webvtt', language: 'en', source: 'cam-1' }] } },
          })}
          editing
          onChange={ignore}
        />,
      );
      expect(screen.getByTestId('subtitle-slot-0')).toBeTruthy();
      fireEvent.click(screen.getByRole('button', { name: /Add subtitle slot/ }));
      expect(screen.getByTestId('subtitle-slot-1')).toBeTruthy();
      fireEvent.click(
        within(screen.getByTestId('subtitle-slot-1')).getByRole('button', { name: /Remove/ }),
      );
      expect(screen.queryByTestId('subtitle-slot-1')).toBeNull();
    });

    it('shows the "not supported yet" refusal with the follow-up issue number on ll-hls/srt', () => {
      renderWithProviders(
        <Harness
          initial={base({
            kind: 'll-hls',
            path: '',
            extra: { slots: { subtitle: [{ format: 'webvtt' }] } },
          })}
          editing
          onChange={ignore}
        />,
      );
      expect(screen.getByTestId('subtitle-slot-0').textContent).toContain('#398');
    });

    it("shows the output's medium carries no subtitles, disabled, for a medium-mismatched kind", () => {
      renderWithProviders(
        <Harness
          initial={base({
            kind: 'ndi',
            path: '',
            url: '',
            extra: { slots: { subtitle: [{ format: 'webvtt' }] } },
          })}
          editing
          onChange={ignore}
        />,
      );
      // NDI has no encoded video slot at all: the whole fieldset (and the
      // subtitle editor inside it) is hidden, not merely disabled.
      expect(screen.queryByTestId('output-format-fields')).toBeNull();
    });

    it('offers the declared sources, plus Unpatched, in the source select', () => {
      renderWithProviders(
        <Harness
          initial={base({
            kind: 'hls',
            extra: { slots: { subtitle: [{ format: 'webvtt', source: 'cam-1' }] } },
          })}
          editing
          onChange={ignore}
          sources={SOURCES}
        />,
      );
      const select = within(screen.getByTestId('subtitle-slot-0')).getByTestId('subtitle-slot-0-source');
      expect(select.textContent).toContain('Camera 1');
    });

    // UI-2 (review): every keystroke used to rewrite the controlled name
    // through trim(), so typing a space-separated name lost every space the
    // instant it was typed.
    it('types a multiword name keeping spaces (UI-2 fix)', async () => {
      let latest = base({
        kind: 'hls',
        extra: { slots: { subtitle: [{ format: 'webvtt' }] } },
      });
      renderWithProviders(
        <Harness
          initial={latest}
          editing
          onChange={(next): void => {
            latest = next;
          }}
        />,
      );
      const input = within(screen.getByTestId('subtitle-slot-0')).getByLabelText('Name');
      await userEvent.type(input, 'English captions');
      expect(input).toHaveValue('English captions');
      expect(outputFormToBody(latest).slots).toEqual({
        subtitle: [{ format: 'webvtt', name: 'English captions' }],
      });
    });

    // UI-2 (review): a save-time trim must not silently discard an
    // existing, server-valid name/language that legitimately carries
    // whitespace (the server never trims either).
    it('preserves a stored name/language with interior/edge whitespace on an unrelated edit', () => {
      const firstRow: SubtitleSlotRow = { format: 'webvtt', language: 'en', name: '  spaced name  ', source: undefined };
      const withRow = withSubtitleSlots(base(), [firstRow]);
      // Editing (adding) a second row must not retroactively mangle the first.
      const withSecondRow = withSubtitleSlots(withRow, [
        firstRow,
        { format: 'webvtt', language: 'es', name: '', source: undefined },
      ]);
      expect(outputFormToBody(withSecondRow).slots).toEqual({
        subtitle: [
          { format: 'webvtt', language: 'en', name: '  spaced name  ' },
          { format: 'webvtt', language: 'es' },
        ],
      });
    });

    // UI-3 (review): JS string length counts UTF-16 code units, so 33
    // supplementary-plane characters (e.g. an emoji) looked like 66 and
    // were wrongly rejected as over the 64-character limit, despite being
    // 33 valid Rust `chars()`.
    it('counts Unicode scalar values, not UTF-16 units, for the name length (UI-3 fix)', () => {
      const name33 = '\u{1F600}'.repeat(33);
      // eslint-disable-next-line @typescript-eslint/no-misused-spread -- counting scalar values by design.
      expect([...name33]).toHaveLength(33);
      expect(name33.length).toBe(66); // UTF-16: 2 units per emoji.
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: name33, source: undefined }], 0),
      ).toBeUndefined();
      const name65 = '\u{1F600}'.repeat(65);
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: name65, source: undefined }], 0),
      ).toBe('name');
    });

    // UI-3 (review): the control check must reject the complete control
    // range Rust's `char::is_control()` rejects, including C1 (U+0080–
    // U+009F) — a naive `\u0000-\u001f` regex missed C1 entirely.
    it('rejects C1 control characters (U+0080–U+009F) like Rust char::is_control (UI-3 fix)', () => {
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: 'a\u0085b', source: undefined }], 0),
      ).toBe('name');
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: 'a\u009fb', source: undefined }], 0),
      ).toBe('name');
      // DEL (U+007F) and the already-covered C0 range still reject.
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: 'a\u007fb', source: undefined }], 0),
      ).toBe('name');
      // A plain ASCII name stays valid.
      expect(
        subtitleSlotError('hls', [{ format: 'webvtt', language: '', name: 'plain name', source: undefined }], 0),
      ).toBeUndefined();
    });
  });
});
