import { describe, expect, it } from 'vitest';
import {
  affectsProgram,
  buildRows,
  crosspointsOf,
  groupOf,
  hasSubtitleData,
  isSetSelection,
  mixerCell,
  mixerColumns,
  outputAudioColumns,
  outputSlotCell,
  outputSubtitleColumns,
  outputVideoColumns,
  sourceOfRow,
  subtitleRows,
} from './patchMatrixModel';
import type { ArmedSelection } from './patchMatrixModel';
import type { MixerPatchView, OutputPatchView } from '../../api/patch';

describe('groupOf', () => {
  it('classifies a media player by id membership', () => {
    expect(groupOf('vt1', 'rtmp', new Set(['vt1']))).toBe('players');
  });
  it('classifies a media_player raw kind directly', () => {
    expect(groupOf('vt2', 'media_player', new Set())).toBe('players');
  });
  it('classifies a generator by raw kind', () => {
    expect(groupOf('bars', 'bars', new Set())).toBe('generators');
    expect(groupOf('clock-1', 'clock', new Set())).toBe('generators');
  });
  it('falls back to sources', () => {
    expect(groupOf('cam-1', 'rtsp', new Set())).toBe('sources');
  });
});

describe('buildRows', () => {
  it('orders Buses, Layouts, Players, Generators, Sources', () => {
    const rows = buildRows(
      [
        { id: 'cam-1', name: 'Camera 1', rawKind: 'rtsp' },
        { id: 'vt1', name: 'VT 1', rawKind: 'rtmp' },
        { id: 'bars', name: 'Bars', rawKind: 'bars' },
      ],
      [{ id: 'news-one', name: 'NewsOne' }],
      [{ id: 'vt1' }],
    );
    expect(rows.map((r) => r.group)).toEqual([
      'buses', 'buses', 'buses',
      'layouts',
      'sources', 'players', 'generators',
    ]);
    expect(rows.map((r) => r.id)).toEqual([
      'bus:program', 'bus:preview', 'bus:clean',
      'layout:news-one',
      'source:cam-1', 'source:vt1', 'source:bars',
    ]);
  });
});

describe('mixerColumns / outputVideoColumns / outputAudioColumns / outputSubtitleColumns', () => {
  const mixer: MixerPatchView = {
    me: 'me1', inputs_version: 1, tick: 10,
    inputs: [
      { number: 1, label: 'Source 1', video: { source: 'cam-1' }, on_program: true, on_preview: false },
      { number: 2, label: null, video: null, on_program: false, on_preview: false },
    ],
  };
  it('labels mixer inputs by their declared label, falling back to inN', () => {
    const cols = mixerColumns(mixer);
    expect(cols).toEqual([
      { kind: 'mixer', me: 'me1', input: 1, label: 'Source 1' },
      { kind: 'mixer', me: 'me1', input: 2, label: 'in2' },
    ]);
  });

  const output: OutputPatchView = {
    output: 'hls-1',
    format: { width: 1920, height: 1080 },
    video: [{ slot: 0, live: true, source: { source: 'cam-1' }, path: 'P1', pending: null, degraded: false }],
    audio: [{ slot: 0, source: 'follow' }],
    subtitle: [{ slot: 0, format: 'webvtt', language: 'en', name: 'English', source: 'none' }],
  };
  it('builds one video column per slot', () => {
    expect(outputVideoColumns(output)).toEqual([{ kind: 'output', output: 'hls-1', slot: 0, label: 'hls-1' }]);
  });
  it('builds one audio column per slot', () => {
    expect(outputAudioColumns(output)).toEqual([{ kind: 'output', output: 'hls-1', slot: 0, label: 'hls-1' }]);
  });
  it('builds subtitle columns from outputs[].subtitle (patch-f data)', () => {
    expect(outputSubtitleColumns(output)).toEqual([{ kind: 'output', output: 'hls-1', slot: 0, label: 'English' }]);
  });
  it('builds no subtitle columns when the output carries none (empty-state)', () => {
    expect(outputSubtitleColumns({ ...output, subtitle: [] })).toEqual([]);
    const withoutSubtitle: typeof output = { output: output.output, format: output.format, video: output.video, audio: output.audio };
    expect(outputSubtitleColumns(withoutSubtitle)).toEqual([]);
  });
});

describe('hasSubtitleData', () => {
  it('is false when no output carries subtitle slots', () => {
    expect(hasSubtitleData([{ output: 'hls-1', format: null, video: [], audio: [] }])).toBe(false);
  });
  it('is true once one output reports a subtitle slot', () => {
    expect(hasSubtitleData([
      { output: 'hls-1', format: null, video: [], audio: [], subtitle: [{ slot: 0, format: 'webvtt', source: 'none' }] },
    ])).toBe(true);
  });
});

describe('subtitleRows', () => {
  it('filters to sources with captions', () => {
    expect(subtitleRows([
      { id: 'cam-1', name: 'Camera 1', hasCaptions: true },
      { id: 'cam-2', name: 'Camera 2', hasCaptions: false },
    ])).toEqual([{ id: 'source:cam-1', label: 'Camera 1', group: 'sources' }]);
  });
});

describe('mixerCell', () => {
  const input = { number: 1, label: null, video: { source: 'cam-1' }, on_program: true, on_preview: false };
  it('is on-air for the row currently patched', () => {
    expect(mixerCell({ id: 'source:cam-1', label: 'Camera 1', group: 'sources' }, input, {})).toEqual({ state: 'on-air' });
  });
  it('is available for another source row', () => {
    expect(mixerCell({ id: 'source:cam-2', label: 'Camera 2', group: 'sources' }, input, {})).toEqual({ state: 'available' });
  });
  it('is n/a for a bus or layout row (unknown-source on a mixer input, §12.8)', () => {
    expect(mixerCell({ id: 'bus:program', label: 'Program', group: 'buses' }, input, {})).toEqual({ state: 'na' });
    expect(mixerCell({ id: 'layout:news-one', label: 'NewsOne', group: 'layouts' }, input, {})).toEqual({ state: 'na' });
  });
  it('is unknown for a redacted input', () => {
    const redacted = { number: 2, label: null, video: { redacted: true } as const, on_program: false, on_preview: false };
    expect(mixerCell({ id: 'source:cam-1', label: 'Camera 1', group: 'sources' }, redacted, {})).toEqual({ state: 'unknown' });
  });
});

describe('outputSlotCell', () => {
  const videoSlot = { slot: 0, live: true, source: { source: 'cam-1' }, path: 'P1', pending: null, degraded: false };
  it('is on-air with the shared path glyph for the current source', () => {
    expect(outputSlotCell({ id: 'source:cam-1', label: 'Camera 1', group: 'sources' }, videoSlot))
      .toEqual({ state: 'on-air', path: 'shared' });
  });
  it('is available for another source with no path', () => {
    expect(outputSlotCell({ id: 'source:cam-2', label: 'Camera 2', group: 'sources' }, videoSlot))
      .toEqual({ state: 'available' });
  });
  it('is unknown with no readback (live: false)', () => {
    const noReadback = { slot: 0, live: false, source: null, path: null, pending: null, degraded: false };
    expect(outputSlotCell({ id: 'source:cam-1', label: 'Camera 1', group: 'sources' }, noReadback))
      .toEqual({ state: 'unknown' });
  });
  it('is unknown for a redacted feed', () => {
    const redacted = { slot: 0, live: true, source: { redacted: true }, path: null, pending: null, degraded: false };
    expect(outputSlotCell({ id: 'source:cam-1', label: 'Camera 1', group: 'sources' }, redacted))
      .toEqual({ state: 'unknown' });
  });
  it('is pending with held ticks when a take holds this row at the fresh gate', () => {
    const pendingSlot = { slot: 0, live: true, source: { source: 'cam-2' }, path: 'P1', pending: { source: { source: 'cam-1' }, held_ticks: 12 }, degraded: false };
    expect(outputSlotCell({ id: 'source:cam-1', label: 'Camera 1', group: 'sources' }, pendingSlot))
      .toEqual({ state: 'pending', heldTicks: 12 });
  });
  it('treats an always-live audio slot 0 (no `live` field) as readback, not unknown', () => {
    const audioSlot = { slot: 0, source: 'follow' as const };
    expect(outputSlotCell({ id: 'bus:program', label: 'Program', group: 'buses' }, audioSlot))
      .toEqual({ state: 'available' });
  });
});

describe('sourceOfRow', () => {
  it('builds a bus source', () => { expect(sourceOfRow('bus:program')).toEqual({ bus: 'program' }); });
  it('builds a layout source', () => { expect(sourceOfRow('layout:news-one')).toEqual({ layout: 'news-one' }); });
  it('builds a plain source', () => { expect(sourceOfRow('source:cam-1')).toEqual({ source: 'cam-1' }); });
});

describe('crosspointsOf / isSetSelection / affectsProgram', () => {
  const selections: readonly ArmedSelection[] = [
    { column: { kind: 'output', output: 'hls-1', slot: 0, label: 'hls-1' }, rowId: 'layout:news-one', plane: 'video' },
    { column: { kind: 'output', output: 'srt-1', slot: 0, label: 'srt-1' }, rowId: 'source:abc-news', plane: 'video' },
  ];
  it('composes one crosspoint per selection', () => {
    expect(crosspointsOf(selections)).toEqual([
      { plane: 'video', destination: { output: 'hls-1', slot: 0 }, source: { layout: 'news-one' } },
      { plane: 'video', destination: { output: 'srt-1', slot: 0 }, source: { source: 'abc-news' } },
    ]);
  });
  it('composes a mixer crosspoint with `none` when the row is unselectable', () => {
    const mixerSel: ArmedSelection = { column: { kind: 'mixer', me: 'me1', input: 1, label: 'in1' }, rowId: 'bus:program', plane: 'video' };
    expect(crosspointsOf([mixerSel])).toEqual([
      { plane: 'video', destination: { mixer: 'me1', input: 1 }, source: 'none' },
    ]);
  });
  it('is a set once two distinct destinations are armed', () => {
    expect(isSetSelection(selections)).toBe(true);
    expect(isSetSelection([selections[0]!])).toBe(false);
  });
  it('affects Program when a mixer selection targets the Program input', () => {
    const sel: ArmedSelection = { column: { kind: 'mixer', me: 'me1', input: 1, label: 'in1' }, rowId: 'source:x', plane: 'video' };
    expect(affectsProgram([sel], 1, new Set())).toBe(true);
    expect(affectsProgram([sel], 2, new Set())).toBe(false);
  });
  it('affects Program when an output selection targets a Program-tallied output', () => {
    const sel: ArmedSelection = { column: { kind: 'output', output: 'hls-1', slot: 0, label: 'hls-1' }, rowId: 'source:x', plane: 'video' };
    expect(affectsProgram([sel], undefined, new Set(['hls-1']))).toBe(true);
    expect(affectsProgram([sel], undefined, new Set(['srt-1']))).toBe(false);
  });
});
