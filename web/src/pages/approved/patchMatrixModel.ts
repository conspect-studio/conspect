// The Routing page as the patch matrix (ADR-0110 §8, §12.9, #308): data model
// only — row/column assembly, cell state derivation, and the request bodies a
// multi-select "Take all" / salvo store compose. Presentation lives in
// `PatchMatrix.tsx`; this module has no JSX so its logic is unit-testable
// without a DOM.
import type {
  MixerPatchView,
  OutputAudioSlotView,
  OutputPatchView,
  OutputSubtitleSlotView,
  OutputVideoSlotView,
  PatchCrosspoint,
  PatchSource,
} from '../../api/patch';
import { mixerCrosspoint, outputAudioCrosspoint, outputVideoCrosspoint } from '../../api/patch';

/** A matrix row: one patchable source, grouped per §12.9/§8. */
export interface MatrixRow {
  readonly id: string;
  readonly label: string;
  readonly group: 'buses' | 'layouts' | 'players' | 'cg' | 'generators' | 'sources';
}

/** A matrix column: a mixer input, or one output's one slot. */
export type MatrixColumn =
  | { readonly kind: 'mixer'; readonly me: string; readonly input: number; readonly label: string }
  | { readonly kind: 'output'; readonly output: string; readonly slot: number; readonly label: string };

/** One crosspoint cell's derived display state (§8's glyph table). */
export type CellState =
  | 'on-air'
  | 'planned'
  | 'pending'
  | 'available'
  | 'disabled'
  | 'na'
  | 'unknown';

/** A disabled cell's reason, exactly §8's vocabulary. */
export type DisabledReason =
  | 'not-fresh'
  | 'would-cycle'
  | 'capacity'
  | 'not-authorized'
  | 'switcher-required'
  | 'unroutable-output';

/** The path glyph suffix (§12.9): passthrough, shared or converted. */
export type CellPath = 'passthrough' | 'shared' | 'converted' | undefined;

/** One rendered cell: state, path and whether this row is the destination's current source. */
export interface MatrixCell {
  readonly state: CellState;
  readonly path?: CellPath;
  readonly disabledReason?: DisabledReason;
  readonly heldTicks?: number;
}

const GENERATOR_IDS = new Set(['bars', 'test', 'solid', 'clock', 'timer']);

/** Group a source id for the row header, given raw transport kinds + player/layout membership. */
export function groupOf(
  id: string,
  rawKind: string | undefined,
  playerIds: ReadonlySet<string>,
): MatrixRow['group'] {
  if (playerIds.has(id) || rawKind === 'media_player') return 'players';
  if (rawKind !== undefined && GENERATOR_IDS.has(rawKind)) return 'generators';
  return 'sources';
}

/** Build the row list: Buses, Layouts, Players, Generators, Sources, in that order. */
export function buildRows(
  sources: readonly { readonly id: string; readonly name: string; readonly rawKind: string }[],
  layouts: readonly { readonly id: string; readonly name: string }[],
  players: readonly { readonly id: string }[],
  buses: readonly ('program' | 'preview' | 'clean')[] = ['program', 'preview', 'clean'],
): readonly MatrixRow[] {
  const playerIds = new Set(players.map((p) => p.id));
  const busRows: MatrixRow[] = buses.map((bus) => ({
    id: `bus:${bus}`,
    label: bus === 'program' ? 'Program' : bus === 'preview' ? 'Preview' : 'Clean',
    group: 'buses',
  }));
  const layoutRows: MatrixRow[] = layouts.map((layout) => ({
    id: `layout:${layout.id}`,
    label: layout.name,
    group: 'layouts',
  }));
  const sourceRows: MatrixRow[] = sources.map((source) => ({
    id: `source:${source.id}`,
    label: source.name,
    group: groupOf(source.id, source.rawKind, playerIds),
  }));
  return [...busRows, ...layoutRows, ...sourceRows];
}

/** Build mixer input columns from one M/E's live bank. */
export function mixerColumns(mixer: MixerPatchView): readonly MatrixColumn[] {
  return mixer.inputs.map((input): MatrixColumn => ({
    kind: 'mixer',
    me: mixer.me,
    input: input.number,
    label: input.label ?? `in${String(input.number)}`,
  }));
}

/** Build output video-slot columns from one output's readback. */
export function outputVideoColumns(output: OutputPatchView): readonly MatrixColumn[] {
  return output.video.map((slot): MatrixColumn => ({
    kind: 'output',
    output: output.output,
    slot: slot.slot,
    label: output.video.length > 1 ? `${output.output}[${String(slot.slot)}]` : output.output,
  }));
}

/** Build output audio-slot columns (Audio tab) from one output's readback. */
export function outputAudioColumns(output: OutputPatchView): readonly MatrixColumn[] {
  return output.audio.map((slot): MatrixColumn => ({
    kind: 'output',
    output: output.output,
    slot: slot.slot,
    label: slot.track !== undefined ? `${output.output}:${slot.track}` : output.output,
  }));
}

/** Build output subtitle-slot columns (Subtitle tab, patch-f #345). Empty while that tab is unfilled. */
export function outputSubtitleColumns(output: OutputPatchView): readonly MatrixColumn[] {
  return (output.subtitle ?? []).map((slot): MatrixColumn => ({
    kind: 'output',
    output: output.output,
    slot: slot.slot,
    label: slot.name ?? `${output.output}[${String(slot.slot)}]`,
  }));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** The row id a readback source value corresponds to (`{source}`/`{layout}`/`{bus}`), if known. */
function rowIdOfSource(value: unknown): string | undefined {
  if (!isRecord(value)) return undefined;
  if (typeof value.source === 'string') return `source:${value.source}`;
  if (typeof value.layout === 'string') return `layout:${value.layout}`;
  if (typeof value.bus === 'string') return `bus:${value.bus}`;
  return undefined;
}

/** Map an engine `path` (`P1`/`P2`/`P3`) to the §12.9 glyph suffix. */
function pathGlyph(path: string | null): CellPath {
  if (path === 'P1') return 'shared';
  if (path === 'P2') return 'passthrough';
  if (path === 'P3') return 'converted';
  return undefined;
}

/** Derive one mixer-input cell's state from the live bank + the row it represents. */
export function mixerCell(
  row: MatrixRow,
  input: MixerPatchView['inputs'][number],
  tally: { readonly program?: string; readonly preview?: string },
): MatrixCell {
  const current = input.video !== null && 'source' in input.video ? `source:${input.video.source}` : undefined;
  if (input.video !== null && 'redacted' in input.video) return { state: 'unknown' };
  if (row.group !== 'sources' && row.group !== 'players' && row.group !== 'generators') {
    // A bus/layout cannot patch onto a mixer input (§12.8's unknown-source refusal).
    return { state: 'na' };
  }
  if (current === row.id) {
    if (tally.program === row.id.replace('source:', '')) return { state: 'on-air', path: 'shared' };
    return { state: 'on-air' };
  }
  return { state: 'available' };
}

/** Derive one output video/audio-slot cell's state from its readback + the row it represents. */
export function outputSlotCell(
  row: MatrixRow,
  slot: OutputVideoSlotView | OutputAudioSlotView,
): MatrixCell {
  // Video slots report readback via `live`; audio slot 0 is always the
  // accepted (configured) feed, so `live` is simply absent on it (§12.8).
  if ('live' in slot && !slot.live) return { state: 'unknown' };
  if (isRecord(slot.source) && slot.source.redacted === true) return { state: 'unknown' };
  const current = rowIdOfSource(slot.source);
  const path = 'path' in slot ? pathGlyph(slot.path) : undefined;
  const pending = 'pending' in slot ? slot.pending : null;
  if (pending !== null && rowIdOfSource(pending.source) === row.id) {
    return { state: 'pending', heldTicks: pending.held_ticks };
  }
  if (current === row.id) {
    return { state: 'on-air', ...(path !== undefined ? { path } : {}) };
  }
  if (row.group !== 'sources' && row.group !== 'players' && row.group !== 'generators' && row.group !== 'layouts' && row.group !== 'buses') {
    return { state: 'na' };
  }
  return { state: 'available' };
}

/** Build a bus/layout/source crosspoint source value from a matrix row id. */
const BUSES = ['program', 'preview', 'clean'] as const;

export function sourceOfRow(rowId: string): PatchSource {
  if (rowId.startsWith('bus:')) {
    const name = rowId.slice('bus:'.length);
    const bus = BUSES.find((candidate) => candidate === name) ?? 'program';
    return { bus };
  }
  if (rowId.startsWith('layout:')) return { layout: rowId.slice('layout:'.length) };
  return { source: rowId.slice('source:'.length) };
}

/** One armed multi-select cell (a mixer input or output slot, with its chosen row). */
export interface ArmedSelection {
  readonly column: MatrixColumn;
  readonly rowId: string;
  readonly plane: 'video' | 'audio' | 'subtitle';
}

/** Compose the crosspoints for an armed selection set (the salvo/plan/take body). */
export function crosspointsOf(selections: readonly ArmedSelection[]): readonly PatchCrosspoint[] {
  return selections.map((sel) => {
    const source = sourceOfRow(sel.rowId);
    if (sel.column.kind === 'mixer') {
      return mixerCrosspoint(
        sel.column.me,
        sel.column.input,
        typeof source === 'object' && 'source' in source ? source.source : undefined,
      );
    }
    return sel.plane === 'audio'
      ? outputAudioCrosspoint(sel.column.output, sel.column.slot, source)
      : outputVideoCrosspoint(sel.column.output, sel.column.slot, source);
  });
}

/** True when 2+ distinct destinations are armed (an atomic set, not a single take). */
export function isSetSelection(selections: readonly ArmedSelection[]): boolean {
  const keys = new Set(
    selections.map((sel) =>
      sel.column.kind === 'mixer' ? `mixer:${sel.column.me}:${String(sel.column.input)}` : `output:${sel.column.output}:${String(sel.column.slot)}`,
    ),
  );
  return keys.size >= 2;
}

/** Whether an armed selection changes Program (take-lock confirm gate, §8). */
export function affectsProgram(
  selections: readonly ArmedSelection[],
  programInput: number | undefined,
  programOutputTallies: ReadonlySet<string>,
): boolean {
  return selections.some((sel) => {
    if (sel.column.kind === 'mixer') return sel.column.input === programInput;
    return programOutputTallies.has(sel.column.output);
  });
}

/** Columns + rows for the empty-state check (Subtitle tab, patch-f note). */
export function hasSubtitleData(outputs: readonly OutputPatchView[]): boolean {
  return outputs.some((output) => (output.subtitle ?? []).length > 0);
}

/** Sources with captions (Subtitle tab rows, patch-f note): filters by a caller-supplied predicate. */
export function subtitleRows(
  sources: readonly { readonly id: string; readonly name: string; readonly hasCaptions: boolean }[],
): readonly MatrixRow[] {
  return sources
    .filter((s) => s.hasCaptions)
    .map((s): MatrixRow => ({ id: `source:${s.id}`, label: s.name, group: 'sources' }));
}

export type { OutputSubtitleSlotView };
