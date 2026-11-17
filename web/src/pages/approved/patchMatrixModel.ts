// The Routing page as the patch matrix (ADR-0110 §8, §12.9, #308): data model
// only — row/column assembly, cell state derivation, and the request bodies a
// multi-select "Take all" / salvo store compose. Presentation lives in
// `PatchMatrix.tsx`; this module has no JSX so its logic is unit-testable
// without a DOM.
import type {
  MixerPatchView,
  OutputAudioSlotView,
  OutputPatchView,
  OutputVideoSlotView,
  PatchCrosspoint,
  PatchSource,
} from '../../api/patch';
import { mixerCrosspoint, outputAudioCrosspoint, outputVideoCrosspoint } from '../../api/patch';

/** A matrix row: one patchable source, grouped per §12.9/§8. */
export interface MatrixRow {
  readonly id: string;
  readonly label: string;
  readonly group: 'feeds' | 'buses' | 'layouts' | 'players' | 'cg' | 'generators' | 'sources';
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

/**
 * A disabled cell's reason: §8's vocabulary, plus the E2 API limits the
 * matrix must state rather than send (audio slots fixed by configuration,
 * audio `follow` riding a video take, subtitle slots patched in the Subtitle patch panel).
 * The page translates each code at render.
 */
export type DisabledReason =
  | 'not-fresh'
  | 'would-cycle'
  | 'capacity'
  | 'not-authorized'
  | 'switcher-required'
  | 'unroutable-output'
  | 'audio-fixed'
  | 'subtitle-panel'
  | 'refused';

/** The path glyph suffix (§12.9): passthrough, shared or converted. */
export type CellPath = 'passthrough' | 'shared' | 'converted' | undefined;

/** One rendered cell: state, path and whether this row is the destination's current source. */
export interface MatrixCell {
  readonly state: CellState;
  readonly path?: CellPath;
  readonly disabledReason?: DisabledReason;
  /** The plan's typed refusal for this armed member (`disabledReason: 'refused'`), detail as the server worded it. */
  readonly refusal?: { readonly type: string; readonly detail: string };
  /** An armed, planned member whose only refusal is the on-air guard: a confirmed take may still apply it. */
  readonly needsConfirm?: boolean;
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

/** The Audio tab's feed rows: `follow` (the only patchable audio feed in this release), then the fixed feeds it can show on air. */
export function audioRows(
  sources: readonly { readonly id: string; readonly name: string }[],
): readonly MatrixRow[] {
  return [
    { id: 'audio:follow', label: 'Follow', group: 'feeds' },
    { id: 'audio:program', label: 'Program mix', group: 'feeds' },
    { id: 'audio:none', label: 'Silence', group: 'feeds' },
    ...sources.map((source): MatrixRow => ({ id: `source:${source.id}`, label: source.name, group: 'sources' })),
  ];
}

/** The row id an audio slot's accepted feed corresponds to (Audio tab). */
function audioRowIdOf(value: unknown): string | undefined {
  if (value === 'follow') return 'audio:follow';
  if (value === 'program') return 'audio:program';
  if (value === 'none') return 'audio:none';
  if (isRecord(value) && typeof value.source === 'string') return `source:${value.source}`;
  return undefined;
}

/**
 * One Audio-tab cell. E2 accepts only output audio slot 0 with `"follow"`
 * (riding that output's video take, `patch_output::shape`); every other
 * feed/slot is fixed by configuration, so it is shown (on air when it is the
 * accepted feed) but disabled with that reason.
 */
export function audioCell(row: MatrixRow, slot: OutputAudioSlotView): MatrixCell {
  if (isRecord(slot.source) && slot.source.redacted === true) return { state: 'unknown' };
  const onAir = audioRowIdOf(slot.source) === row.id;
  if (slot.slot !== 0 || row.id !== 'audio:follow') {
    return onAir ? { state: 'on-air' } : { state: 'disabled', disabledReason: 'audio-fixed' };
  }
  return onAir ? { state: 'on-air' } : { state: 'available' };
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

/** Build output subtitle-slot columns (Subtitle tab, display-only: patch-f's Subtitle patch panel takes them). */
export function outputSubtitleColumns(output: OutputPatchView): readonly MatrixColumn[] {
  return output.subtitle.map((slot): MatrixColumn => ({
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

/** Map an engine `path` (`P1`/`P2`/`P3`) to the §12.9 glyph suffix (readback or plan member). */
export function pathGlyph(path: string | null | undefined): CellPath {
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

const BUSES = ['program', 'preview', 'clean'] as const;

/** Build a bus/layout/source crosspoint source value from a matrix row id. */
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

/** A destination's identity within one plane (audio slot 0 and video slot 0 of one output are distinct cells). */
export function selectionKey(plane: ArmedSelection['plane'], column: MatrixColumn): string {
  return column.kind === 'mixer'
    ? `${plane}:mixer:${column.me}:${String(column.input)}`
    : `${plane}:output:${column.output}:${String(column.slot)}`;
}

/**
 * The crosspoint one selection sends, or `undefined` when the E2 API cannot
 * take it: audio only as output slot 0 `"follow"`, never subtitle (the Subtitle
 * patch panel owns subtitle takes, #345 F3), and
 * never a plane-video crosspoint for a non-video selection.
 */
export function crosspointOf(sel: ArmedSelection): PatchCrosspoint | undefined {
  if (sel.plane === 'subtitle') return undefined;
  if (sel.plane === 'audio') {
    if (sel.column.kind !== 'output' || sel.column.slot !== 0 || sel.rowId !== 'audio:follow') return undefined;
    return outputAudioCrosspoint(sel.column.output, 0, 'follow');
  }
  const source = sourceOfRow(sel.rowId);
  if (sel.column.kind === 'mixer') {
    return mixerCrosspoint(
      sel.column.me,
      sel.column.input,
      typeof source === 'object' && 'source' in source ? source.source : undefined,
    );
  }
  return outputVideoCrosspoint(sel.column.output, sel.column.slot, source);
}

/** The takeable members of a selection, in request order (plan `crosspoints[i]` answers `members[i]`). */
export function membersOf(
  selections: readonly ArmedSelection[],
): readonly { readonly selection: ArmedSelection; readonly crosspoint: PatchCrosspoint }[] {
  return selections.flatMap((selection) => {
    const crosspoint = crosspointOf(selection);
    return crosspoint === undefined ? [] : [{ selection, crosspoint }];
  });
}

/** Compose the crosspoints for an armed selection set (the salvo/plan/take body). */
export function crosspointsOf(selections: readonly ArmedSelection[]): readonly PatchCrosspoint[] {
  return membersOf(selections).map((member) => member.crosspoint);
}

/**
 * Which terminal event a take of these crosspoints settles on (OpenAPI
 * `POST /patch/take` 202): `patch.take` for a set (two or more distinct
 * outputs, or mixer inputs together with an output — `patch_set::is_set`),
 * `route.output` for one output, `switcher.operation` for mixer inputs alone.
 */
export type TakePath = 'set' | 'output' | 'mixer';

export function takePathOf(crosspoints: readonly PatchCrosspoint[]): TakePath {
  const outputs = new Set<string>();
  let mixer = false;
  for (const xp of crosspoints) {
    if ('mixer' in xp.destination) mixer = true;
    else outputs.add(xp.destination.output);
  }
  if (outputs.size >= 2 || (mixer && outputs.size > 0)) return 'set';
  return outputs.size === 1 ? 'output' : 'mixer';
}

/** True when 2+ distinct destinations are armed ("Take all"); the settle path is {@link takePathOf}'s. */
export function isSetSelection(selections: readonly ArmedSelection[]): boolean {
  const keys = new Set(
    selections.map((sel) =>
      sel.column.kind === 'mixer' ? `mixer:${sel.column.me}:${String(sel.column.input)}` : `output:${sel.column.output}:${String(sel.column.slot)}`,
    ),
  );
  return keys.size >= 2;
}

/** Why the armed selection cannot be sent as is, or `undefined` when it can. */
export type SelectionProblem = 'audio-needs-video';

export function selectionProblem(selections: readonly ArmedSelection[]): SelectionProblem | undefined {
  const videoOutputs = new Set(
    selections.flatMap((sel) => (sel.plane === 'video' && sel.column.kind === 'output' ? [sel.column.output] : [])),
  );
  const orphan = selections.some(
    (sel) => sel.plane === 'audio' && sel.column.kind === 'output' && !videoOutputs.has(sel.column.output),
  );
  return orphan ? 'audio-needs-video' : undefined;
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

/** Whether stored crosspoints (a recalled salvo's members) change Program, resolved against the current readback. */
export function crosspointsAffectProgram(
  crosspoints: readonly PatchCrosspoint[],
  programInput: number | undefined,
  programOutputTallies: ReadonlySet<string>,
): boolean {
  return crosspoints.some((xp) =>
    'mixer' in xp.destination
      ? xp.destination.input === programInput
      : programOutputTallies.has(xp.destination.output),
  );
}

/** One plan member's answer for an armed cell (`PlannedCrosspoint`: path + refusal). */
export interface PlannedMember {
  readonly path: string;
  readonly refusal: { readonly type: string; readonly detail: string } | null;
}

/** The refusal type the on-air guard answers; under the take lock it means "needs confirmation", not "cannot". */
export const ON_AIR_REFUSAL = 'on-air-protected';

/**
 * Readback states that outrank a plan overlay (E3-12): the engine's evidence
 * (fresh-gate `pending` with held ticks, `on-air`) and the absence of it
 * (`unknown`: stale or no readback) are never hidden behind `planned`.
 */
const EVIDENCE_STATES: ReadonlySet<CellState> = new Set<CellState>(['pending', 'on-air', 'unknown']);

/**
 * Merge an armed cell with its plan member (§8): `planned` (with the plan's
 * path glyph) once planned, `planned` + needs-confirmation when only the
 * on-air guard refused it, `disabled` with the typed refusal otherwise. An
 * armed cell with no plan yet keeps its readback state, and so does one whose
 * readback is pending, on air or unknown (E3-12).
 */
export function displayCell(base: MatrixCell, armed: boolean, member: PlannedMember | undefined): MatrixCell {
  if (!armed || member === undefined) return base;
  if (EVIDENCE_STATES.has(base.state)) return base;
  const path = pathGlyph(member.path);
  const withPath = path !== undefined ? { path } : {};
  if (member.refusal !== null && member.refusal.type !== ON_AIR_REFUSAL) {
    return { state: 'disabled', disabledReason: 'refused', refusal: member.refusal, ...withPath };
  }
  if (member.refusal !== null) return { state: 'planned', needsConfirm: true, ...withPath };
  return { state: 'planned', ...withPath };
}

/** Columns + rows for the empty-state check (Subtitle tab, patch-f note). */
export function hasSubtitleData(outputs: readonly OutputPatchView[]): boolean {
  return outputs.some((output) => output.subtitle.length > 0);
}

/** Sources with captions (Subtitle tab rows, patch-f note): filters by a caller-supplied predicate. */
export function subtitleRows(
  sources: readonly { readonly id: string; readonly name: string; readonly hasCaptions: boolean }[],
): readonly MatrixRow[] {
  return sources
    .filter((s) => s.hasCaptions)
    .map((s): MatrixRow => ({ id: `source:${s.id}`, label: s.name, group: 'sources' }));
}

