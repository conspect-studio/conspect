import { describe, expect, it } from 'vitest';
import {
  affectsProgram,
  audioCell,
  audioRows,
  buildRows,
  crosspointOf,
  crosspointsAffectProgram,
  displayCell,
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
  selectionProblem,
  sourceOfRow,
  subtitleRows,
  takePathOf,
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

describe('E3-09 displayCell: armed selection + plan member', () => {
  const base = { state: 'available' } as const;
  it('is planned with the plan path glyph once the armed member is planned', () => {
    expect(displayCell(base, true, { path: 'P3', refusal: null })).toEqual({ state: 'planned', path: 'converted' });
  });
  it('is disabled with the typed refusal for a refused member', () => {
    const refusal = { type: 'capacity', detail: 'needs 1 encoder, 0 free' };
    expect(displayCell(base, true, { path: 'P2', refusal })).toEqual({
      state: 'disabled', disabledReason: 'refused', refusal, path: 'passthrough',
    });
  });
  it('is planned + needs confirmation when only the on-air guard refused it', () => {
    expect(displayCell(base, true, { path: 'mixer-input', refusal: { type: 'on-air-protected', detail: 'on Program' } }))
      .toEqual({ state: 'planned', needsConfirm: true });
  });
  it('keeps the readback state for an unarmed cell or an armed one with no plan yet', () => {
    expect(displayCell(base, false, { path: 'P1', refusal: null })).toEqual(base);
    expect(displayCell(base, true, undefined)).toEqual(base);
  });
});

describe('E3-10 audio plane rows and builders', () => {
  const rows = audioRows([{ id: 'cam-1', name: 'Camera 1' }]);
  it('lists follow, program mix, silence, then sources — not video buses/layouts', () => {
    expect(rows.map((r) => r.id)).toEqual(['audio:follow', 'audio:program', 'audio:none', 'source:cam-1']);
  });
  it('slot 0: follow is available, every other feed is disabled as fixed by configuration', () => {
    const slot = { slot: 0, source: 'program' };
    expect(audioCell(rows[0]!, slot)).toEqual({ state: 'available' });
    expect(audioCell(rows[1]!, slot)).toEqual({ state: 'on-air' });
    expect(audioCell(rows[3]!, slot)).toEqual({ state: 'disabled', disabledReason: 'audio-fixed' });
  });
  it('a track slot (> 0) is never patchable, even to follow', () => {
    expect(audioCell(rows[0]!, { slot: 1, track: 'fr', source: { track: 'fr' } }))
      .toEqual({ state: 'disabled', disabledReason: 'audio-fixed' });
  });
  it('an on-air follow shows on air', () => {
    expect(audioCell(rows[0]!, { slot: 0, source: 'follow' })).toEqual({ state: 'on-air' });
  });
  it('emits only output audio slot 0 `follow`; never a plane-video crosspoint for audio', () => {
    const col = { kind: 'output', output: 'hls-1', slot: 0, label: 'hls-1' } as const;
    expect(crosspointOf({ column: col, rowId: 'audio:follow', plane: 'audio' }))
      .toEqual({ plane: 'audio', destination: { output: 'hls-1', slot: 0 }, source: 'follow' });
    expect(crosspointOf({ column: col, rowId: 'source:cam-1', plane: 'audio' })).toBeUndefined();
    expect(crosspointOf({ column: { ...col, slot: 1 }, rowId: 'audio:follow', plane: 'audio' })).toBeUndefined();
  });
  it('a subtitle selection never emits a crosspoint (no plane video)', () => {
    const col = { kind: 'output', output: 'hls-1', slot: 0, label: 'English' } as const;
    expect(crosspointOf({ column: col, rowId: 'source:cam-1', plane: 'subtitle' })).toBeUndefined();
  });
  it('audio follow without its output video take is a selection problem', () => {
    const col = { kind: 'output', output: 'hls-1', slot: 0, label: 'hls-1' } as const;
    expect(selectionProblem([{ column: col, rowId: 'audio:follow', plane: 'audio' }])).toBe('audio-needs-video');
    expect(selectionProblem([
      { column: col, rowId: 'audio:follow', plane: 'audio' },
      { column: col, rowId: 'source:cam-1', plane: 'video' },
    ])).toBeUndefined();
  });
});

describe('E3-04 takePathOf: the event family a take settles on', () => {
  const out = (output: string) => ({ plane: 'video' as const, destination: { output, slot: 0 }, source: { source: 'x' } });
  const mix = { plane: 'video' as const, destination: { mixer: 'me1', input: 1 }, source: 'none' as const };
  it('one output (video + audio follow) settles on route.output', () => {
    expect(takePathOf([out('a'), { plane: 'audio', destination: { output: 'a', slot: 0 }, source: 'follow' }])).toBe('output');
  });
  it('mixer inputs alone settle on switcher.operation', () => {
    expect(takePathOf([mix, { ...mix, destination: { mixer: 'me1', input: 2 } }])).toBe('mixer');
  });
  it('two outputs, or mixer + output, are a set (patch.take)', () => {
    expect(takePathOf([out('a'), out('b')])).toBe('set');
    expect(takePathOf([mix, out('a')])).toBe('set');
  });
});

describe('E3-01 crosspointsAffectProgram (recalled salvo members)', () => {
  it('flags a member on the Program input or a Program-tallied output', () => {
    const mix = { plane: 'video' as const, destination: { mixer: 'me1', input: 1 }, source: 'none' as const };
    expect(crosspointsAffectProgram([mix], 1, new Set())).toBe(true);
    expect(crosspointsAffectProgram([mix], 2, new Set())).toBe(false);
    const out = { plane: 'video' as const, destination: { output: 'hls-1', slot: 0 }, source: { source: 'x' } };
    expect(crosspointsAffectProgram([out], undefined, new Set(['hls-1']))).toBe(true);
  });
});
