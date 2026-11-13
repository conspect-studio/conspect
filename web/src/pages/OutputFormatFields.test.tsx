// OutputFormatFields (#342 B3): presets, width/height/fit/anchor into the
// stored `format`, the video-slot bitrate into `slots.video[0]`, read-only
// audio slots, validation, and the live-reset vs restart warning per kind.
import { useState } from 'react';
import type { JSX } from 'react';
import { describe, expect, it } from 'vitest';
import { fireEvent, screen } from '@testing-library/react';
import { renderWithProviders } from '../test/render';
import { emptyOutputForm, outputFormToBody } from '../resources/forms';
import type { OutputFormState } from '../resources/forms';
import { OutputFormatFields } from './OutputFormatFields';
import { formatError, resetsLive, withBitrate, withFormat } from '../resources/outputFormat';

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
}: {
  readonly initial: OutputFormState;
  readonly editing: boolean;
  readonly onChange: (next: OutputFormState) => void;
}): JSX.Element {
  const [form, setForm] = useState(initial);
  return (
    <OutputFormatFields
      form={form}
      editing={editing}
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
});
