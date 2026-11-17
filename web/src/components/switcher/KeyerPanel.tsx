// A keyer panel (design note §10, K5): fill and key pickers limited to
// sources the caller may read (inputs `in:<n>` are never offered), type
// (luma|linear, and chroma with its `#RRGGBB` reference colour for a DSK, K7)
// with clip/gain/invert, mask, fly x/y/w/h with corner presets, and the loss
// policy. An upstream keyer (K3, `family = "usk"`) gets the same editor
// without chroma and sends `family: "usk"`; it is never offered chroma
// (`usk-chroma`) and never sends DSK-only fields (rate, color).
//
// Every value shown is the authoritative committed value from the snapshot
// row unless the operator edited that field; untouched fields follow each new
// snapshot. Only edited fields that differ from the committed value are sent
// (K4 `keyer_set` merge: an omitted field is preserved). Review (plan: class,
// cost, refusal) and Apply are separate steps; Apply sends exactly the
// reviewed draft and any later edit — or a snapshot change that alters the
// draft — invalidates the review. A command-time 409 `on-air-protected`
// re-plans and asks for the explicit `on_air: "allow"` confirmation.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../ui/button';
import { Input } from '../ui/input';
import { Label } from '../ui/label';
import type { FlyPreset, KeyerFields, KeyerKind, KeyerLossPolicy, KeyerPlan, KeyerRect, KeyerRow, KeyerSize } from '../../api/keyers';
import { FLY_PRESETS, flyPresetRect, keyerFillRedacted, keyerFillSource, keyerDomKey, keyerIsUsk, keyerKeyRedacted, keyerKeySource, keyerKind, normalizeHexColor } from '../../api/keyers';
import { OperationApiError } from '../../api/operations';

export interface KeyerPanelProps {
  readonly row: KeyerRow;
  readonly sources: readonly { readonly id: string; readonly name: string }[];
  /** Non-empty reason disables every control (engine unavailable, a request or outcome in flight, …). */
  readonly disabledReason: string | undefined;
  /** The output canvas (from `GET /switcher/{me}`), the space `fly` is in. */
  readonly canvas: KeyerSize;
  /** Classify a `keyer_set` draft without applying it; `undefined` when another request holds the submission gate. */
  readonly onPlan: (fields: KeyerFields) => Promise<KeyerPlan | undefined>;
  /** Submit a reviewed `keyer_set`; `allow` adds `on_air: "allow"`. Rejects with the RFC 9457 problem. */
  readonly onApply: (fields: KeyerFields, allow: boolean) => Promise<void>;
}

interface RectDraft { readonly x: string; readonly y: string; readonly w: string; readonly h: string }
interface Edits {
  readonly fill?: string;
  readonly key?: string;
  readonly type?: KeyerKind;
  readonly color?: string;
  readonly clip?: string;
  readonly gain?: string;
  readonly invert?: boolean;
  readonly loss?: KeyerLossPolicy;
  readonly fly?: RectDraft | null;
  readonly mask?: RectDraft | null;
}
interface Review { readonly json: string; readonly fields: KeyerFields; readonly plan: KeyerPlan }

const LOSS_POLICIES: readonly KeyerLossPolicy[] = ['drop_to_off_air', 'hold_last_good'];
const AXES = ['x', 'y', 'w', 'h'] as const;
/** The reference offered when a keyer first becomes chroma (a broadcast green). */
const DEFAULT_CHROMA = '#00b140';
type Invalid = 'clip' | 'gain' | 'fly' | 'mask' | 'color' | 'chroma-key';

function rectDraft(rect: KeyerRect): RectDraft {
  return { x: String(rect.x), y: String(rect.y), w: String(rect.w), h: String(rect.h) };
}
function parseRect(draft: RectDraft): KeyerRect | undefined {
  if (!AXES.every((axis) => /^\d+$/.test(draft[axis]))) return undefined;
  const rect = { x: Number(draft.x), y: Number(draft.y), w: Number(draft.w), h: Number(draft.h) };
  return rect.w >= 1 && rect.h >= 1 ? rect : undefined;
}
function sameRect(a: KeyerRect | null | undefined, b: KeyerRect | null | undefined): boolean {
  if (!a || !b) return !a && !b;
  return a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h;
}

/**
 * The `keyer_set` fields of `edits` against the committed `row`: only edited
 * fields whose value differs, or the reason the draft is invalid.
 */
function keyerSetFields(row: KeyerRow, edits: Edits): { readonly fields: KeyerFields; readonly changed: boolean; readonly invalid?: Invalid } {
  const usk = keyerIsUsk(row);
  // `family` scopes the object id; it is not a change by itself.
  const fields: { -readonly [K in keyof KeyerFields]: KeyerFields[K] } = usk ? { keyer: row.id, family: 'usk' } : { keyer: row.id };
  const baseCount = Object.keys(fields).length;
  let invalid: Invalid | undefined;
  if (edits.fill !== undefined && edits.fill !== '' && edits.fill !== keyerFillSource(row)) fields.fill = edits.fill;
  const rowType = keyerKind(row);
  const type = edits.type ?? rowType;
  if (type !== rowType) fields.type = type;
  if (type === 'chroma') {
    // K7: a chroma key keys the fill itself (`chroma-key-source`) and needs its reference colour.
    if (keyerKeyRedacted(row)) invalid = 'chroma-key';
    else if (keyerKeySource(row) !== undefined) fields.key = '';
    const color = normalizeHexColor(edits.color ?? normalizeHexColor(row.color) ?? DEFAULT_CHROMA);
    if (color === undefined) invalid = 'color';
    else if (color !== normalizeHexColor(row.color)) fields.color = color;
  } else {
    if (edits.key !== undefined && !keyerKeyRedacted(row) && edits.key !== (keyerKeySource(row) ?? '')) fields.key = edits.key;
    // A colour on another type is refused (`chroma-color-without-chroma`): clear it.
    if (!usk && row.color !== null && row.color !== undefined && row.color !== '') fields.color = '';
  }
  if (edits.clip !== undefined) {
    const clip = Number(edits.clip);
    if (edits.clip.trim() === '' || !Number.isFinite(clip) || clip < 0 || clip > 1) invalid = 'clip';
    else if (clip !== row.clip) fields.clip = clip;
  }
  if (edits.gain !== undefined) {
    const gain = Number(edits.gain);
    if (edits.gain.trim() === '' || !Number.isFinite(gain) || gain < 0) invalid = 'gain';
    else if (gain !== row.gain) fields.gain = gain;
  }
  if (edits.invert !== undefined && edits.invert !== row.invert) fields.invert = edits.invert;
  const rowLoss: KeyerLossPolicy = row.loss === 'hold_last_good' ? 'hold_last_good' : 'drop_to_off_air';
  if (edits.loss !== undefined && edits.loss !== rowLoss) fields.loss = edits.loss;
  for (const name of ['fly', 'mask'] as const) {
    const edit = edits[name];
    if (edit === undefined) continue;
    const rect = edit === null ? null : parseRect(edit);
    if (rect === undefined) invalid = name;
    else if (!sameRect(rect, row[name])) fields[name] = rect;
  }
  const changed = Object.keys(fields).length > baseCount;
  return invalid === undefined ? { fields, changed } : { fields, changed, invalid };
}

/** One keyer's fill/key/type/geometry/loss editor: Review (plan) then Apply. */
export function KeyerPanel({ row, sources, disabledReason, canvas, onPlan, onApply }: KeyerPanelProps): JSX.Element {
  const { t } = useLingui();
  const [edits, setEdits] = useState<Edits>({});
  const [review, setReview] = useState<Review>();
  const [confirmAllow, setConfirmAllow] = useState(false);
  const [error, setError] = useState<string>();
  const fillRedacted = keyerFillRedacted(row);
  const keyRedacted = keyerKeyRedacted(row);
  const edit = (next: Partial<Edits>): void => {
    setEdits({ ...edits, ...next });
    // Every edit invalidates the review (review K5-R2-01).
    setReview(undefined);
    setConfirmAllow(false);
    setError(undefined);
  };

  // Displayed values: the edit, else the authoritative committed value.
  const fill = edits.fill ?? keyerFillSource(row) ?? '';
  const key = edits.key ?? keyerKeySource(row) ?? '';
  const usk = keyerIsUsk(row);
  const type: KeyerKind = edits.type ?? keyerKind(row);
  const color = edits.color ?? normalizeHexColor(row.color) ?? DEFAULT_CHROMA;
  const clip = edits.clip ?? String(row.clip);
  const gain = edits.gain ?? String(row.gain);
  const invert = edits.invert ?? row.invert;
  const loss: KeyerLossPolicy = edits.loss ?? (row.loss === 'hold_last_good' ? 'hold_last_good' : 'drop_to_off_air');
  const fly: RectDraft | null = edits.fly !== undefined ? edits.fly : row.fly ? rectDraft(row.fly) : null;
  const mask: RectDraft | null = edits.mask !== undefined ? edits.mask : row.mask ? rectDraft(row.mask) : null;
  const maskDefault: KeyerRect = { x: 0, y: 0, w: row.max_source?.w ?? canvas.w, h: row.max_source?.h ?? canvas.h };

  const draft = keyerSetFields(row, edits);
  const json = JSON.stringify(draft.fields);
  // A review is actionable only while the draft is valid AND unchanged: an
  // invalid edit is omitted from `fields`, so JSON equality alone would keep
  // an earlier review live (review K5-R2-01).
  const reviewed = draft.invalid === undefined && review?.json === json ? review : undefined;
  const changesSources = (reviewed?.fields ?? draft.fields).fill !== undefined || (reviewed?.fields ?? draft.fields).key !== undefined;

  const invalidReason = draft.invalid === 'clip' ? t`Clip must be a number from 0 to 1`
    : draft.invalid === 'gain' ? t`Gain must be a number of 0 or more`
    : draft.invalid === 'fly' ? t`Fly x/y must be whole numbers of 0 or more and width/height at least 1`
    : draft.invalid === 'mask' ? t`Mask x/y must be whole numbers of 0 or more and width/height at least 1`
    : draft.invalid === 'color' ? t`The chroma colour must be #RRGGBB`
    : draft.invalid === 'chroma-key' ? t`A chroma key keys the fill itself, but this keyer's key source is hidden from you, so it cannot be cleared`
    : undefined;
  const reviewReason = disabledReason ?? invalidReason ?? (draft.changed ? undefined : t`No changes to review`);
  const applyReason = disabledReason ?? (reviewed === undefined ? t`Review this change first` : !reviewed.plan.can_take ? t`The reviewed change was refused` : undefined);
  const domId = keyerDomKey(row);
  const reasonId = `keyer-panel-reason-${domId}`;
  /** Disabled state plus its reason as `title` and `aria-describedby` (design note §10). */
  const gate = (reason: string | undefined, describedBy = reasonId): { disabled: boolean; title?: string; 'aria-describedby'?: string } =>
    reason === undefined ? { disabled: false } : { disabled: true, title: reason, 'aria-describedby': describedBy };
  const base = gate(disabledReason);

  const runReview = async (): Promise<Review | undefined> => {
    setError(undefined);
    setConfirmAllow(false);
    const fields = draft.fields;
    try {
      const plan = await onPlan(fields);
      if (plan === undefined) return undefined;
      const next = { json: JSON.stringify(fields), fields, plan };
      setReview(next);
      if (plan.refusal?.type === 'on-air-protected' && changesSources) setConfirmAllow(true);
      return next;
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      return undefined;
    }
  };

  const apply = async (allow: boolean): Promise<void> => {
    if (reviewed === undefined || draft.invalid !== undefined) return;
    setError(undefined);
    try {
      await onApply(reviewed.fields, allow);
      setEdits({});
      setReview(undefined);
      setConfirmAllow(false);
    } catch (err) {
      if (err instanceof OperationApiError && err.problemType?.endsWith('on-air-protected') === true) {
        // The keyer went on air after the review: re-plan, then ask.
        setConfirmAllow(false);
        const fresh = await runReview();
        if (fresh !== undefined && fresh.plan.refusal?.type !== 'on-air-protected') {
          setError(t`The keyer state changed; review the new plan before applying`);
        }
        return;
      }
      setError(err instanceof OperationApiError && err.detail !== undefined ? `${err.message}: ${err.detail}` : err instanceof Error ? err.message : String(err));
    }
  };

  const presetLabel = (preset: FlyPreset): string => preset === 'top_left' ? t`TL` : preset === 'top_right' ? t`TR`
    : preset === 'bottom_left' ? t`BL` : preset === 'bottom_right' ? t`BR` : preset === 'centre' ? t`Centre` : t`Full`;
  const presetName = (preset: FlyPreset): string => preset === 'top_left' ? t`Fly to the top left corner`
    : preset === 'top_right' ? t`Fly to the top right corner` : preset === 'bottom_left' ? t`Fly to the bottom left corner`
    : preset === 'bottom_right' ? t`Fly to the bottom right corner` : preset === 'centre' ? t`Fly to the centre` : t`Full canvas (no fly)`;
  const axisLabel = (axis: (typeof AXES)[number]): string => axis === 'x' ? t`X` : axis === 'y' ? t`Y` : axis === 'w' ? t`Width` : t`Height`;
  const flyBox = (): KeyerSize => {
    const current = fly ? parseRect(fly) : undefined;
    return current ? { w: current.w, h: current.h } : { w: Math.round(canvas.w / 2), h: Math.round(canvas.h / 2) };
  };

  return (
    <section className="space-y-2 rounded border p-3" aria-label={usk ? t`Upstream keyer ${row.id} panel` : t`Keyer ${row.id} panel`} data-testid={`keyer-panel-${row.id}`} data-family={usk ? 'usk' : 'dsk'}>
      <h3 className="font-semibold uppercase">{row.id}</h3>
      <span className="sr-only" id={reasonId}>{disabledReason}</span>
      <div className="flex flex-wrap items-end gap-3">
        <div className="space-y-1">
          <Label htmlFor={`keyer-fill-${domId}`}><Trans>Fill</Trans></Label>
          {fillRedacted
            ? <p id={`keyer-fill-${domId}`} role="status" data-testid={`keyer-fill-redacted-${row.id}`}><Trans>redacted</Trans></p>
            : <select id={`keyer-fill-${domId}`} className="rounded border bg-background p-2" value={fill} {...base}
                onChange={(event) => { edit({ fill: event.target.value }); }}>
                <option value="">{t`Choose a source`}</option>
                {sources.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
              </select>}
        </div>
        {type !== 'chroma' && <div className="space-y-1">
          <Label htmlFor={`keyer-key-${domId}`}><Trans>Key</Trans></Label>
          {keyRedacted
            ? <p id={`keyer-key-${domId}`} role="status" data-testid={`keyer-key-redacted-${row.id}`}><Trans>redacted</Trans></p>
            : <select id={`keyer-key-${domId}`} className="rounded border bg-background p-2" value={key} {...base}
                onChange={(event) => { edit({ key: event.target.value }); }}>
                <option value="">{t`Self key (fill alpha)`}</option>
                {sources.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
              </select>}
        </div>}
        <div className="space-y-1">
          <Label htmlFor={`keyer-type-${domId}`}><Trans>Type</Trans></Label>
          <select id={`keyer-type-${domId}`} className="rounded border bg-background p-2" value={type} {...base}
            onChange={(event) => { const v = event.target.value; edit({ type: v === 'luma' ? 'luma' : v === 'chroma' && !usk ? 'chroma' : 'linear' }); }}>
            <option value="linear">{t`Linear`}</option>
            <option value="luma">{t`Luma`}</option>
            {!usk && <option value="chroma">{t`Chroma`}</option>}
          </select>
        </div>
      </div>
      {(type === 'luma' || type === 'chroma') && (
        <div className="flex flex-wrap items-end gap-3">
          {type === 'chroma' && <div className="space-y-1"><Label htmlFor={`keyer-color-${domId}`}><Trans>Chroma colour</Trans></Label>
            <input id={`keyer-color-${domId}`} type="color" value={normalizeHexColor(color) ?? DEFAULT_CHROMA} {...base}
              aria-invalid={draft.invalid === 'color'} className="h-9 w-16 rounded border bg-background"
              onChange={(event) => { edit({ color: event.target.value }); }} />
            <span className="ml-2 font-mono text-xs" data-testid={`keyer-color-value-${row.id}`}>{color}</span></div>}
          <div className="space-y-1"><Label htmlFor={`keyer-clip-${domId}`}><Trans>Clip</Trans></Label>
            <Input id={`keyer-clip-${domId}`} type="number" min="0" max="1" step="0.01" value={clip} {...base} aria-invalid={draft.invalid === 'clip'} className="w-24" onChange={(event) => { edit({ clip: event.target.value }); }} /></div>
          <div className="space-y-1"><Label htmlFor={`keyer-gain-${domId}`}><Trans>Gain</Trans></Label>
            <Input id={`keyer-gain-${domId}`} type="number" min="0" step="0.01" value={gain} {...base} aria-invalid={draft.invalid === 'gain'} className="w-24" onChange={(event) => { edit({ gain: event.target.value }); }} /></div>
        </div>
      )}
      <label className="flex items-center gap-1 text-sm">
        <input type="checkbox" checked={invert} {...base} onChange={(event) => { edit({ invert: event.target.checked }); }} />
        <Trans>Invert matte</Trans>
      </label>
      <fieldset className="space-y-1" data-testid={`keyer-fly-fieldset-${row.id}`}>
        <legend className="text-sm font-medium"><Trans>Fly (canvas {canvas.w}×{canvas.h})</Trans></legend>
        <div className="flex flex-wrap items-end gap-2" role="group" aria-label={t`Fly preset`}>
          {FLY_PRESETS.map((preset) => {
            const rect = flyPresetRect(preset, canvas, flyBox());
            const active = preset === 'full' ? fly === null : rect !== null && fly !== null && sameRect(parseRect(fly), rect);
            return (
              <Button key={preset} type="button" size="sm" variant={active ? 'default' : 'outline'} aria-label={presetName(preset)} aria-pressed={active} {...base}
                onClick={() => { edit({ fly: rect === null ? null : rectDraft(rect) }); }}>
                {presetLabel(preset)}
              </Button>
            );
          })}
        </div>
        {fly !== null && (
          <div className="flex flex-wrap items-end gap-2">
            {AXES.map((axis) => (
              <div className="space-y-1" key={axis}>
                <Label htmlFor={`keyer-fly-${axis}-${domId}`}><Trans>Fly {axisLabel(axis)}</Trans></Label>
                <Input id={`keyer-fly-${axis}-${domId}`} type="number" min={axis === 'w' || axis === 'h' ? '1' : '0'} step="1" value={fly[axis]} {...base}
                  aria-invalid={draft.invalid === 'fly'} className="w-24" onChange={(event) => { edit({ fly: { ...fly, [axis]: event.target.value } }); }} />
              </div>
            ))}
          </div>
        )}
      </fieldset>
      <fieldset className="space-y-1" data-testid={`keyer-mask-fieldset-${row.id}`}>
        <legend className="text-sm font-medium"><Trans>Mask</Trans></legend>
        <label className="flex items-center gap-1 text-sm">
          <input type="checkbox" checked={mask !== null} {...base}
            onChange={(event) => { edit({ mask: event.target.checked ? (row.mask ? rectDraft(row.mask) : rectDraft(maskDefault)) : null }); }} />
          <Trans>Enable mask</Trans>
        </label>
        {mask !== null && (
          <div className="flex flex-wrap items-end gap-2">
            {AXES.map((axis) => (
              <div className="space-y-1" key={axis}>
                <Label htmlFor={`keyer-mask-${axis}-${domId}`}><Trans>Mask {axisLabel(axis)}</Trans></Label>
                <Input id={`keyer-mask-${axis}-${domId}`} type="number" min={axis === 'w' || axis === 'h' ? '1' : '0'} step="1" value={mask[axis]} {...base}
                  aria-invalid={draft.invalid === 'mask'} className="w-24" onChange={(event) => { edit({ mask: { ...mask, [axis]: event.target.value } }); }} />
              </div>
            ))}
            <Button type="button" size="sm" variant="outline" {...base} onClick={() => { edit({ mask: null }); }}>
              <Trans>Full (clear mask)</Trans>
            </Button>
          </div>
        )}
        {invalidReason !== undefined && <p role="alert" data-testid={`keyer-invalid-${row.id}`}>{invalidReason}</p>}
      </fieldset>
      <div className="space-y-1">
        <Label htmlFor={`keyer-loss-${domId}`}><Trans>Loss policy</Trans></Label>
        <select id={`keyer-loss-${domId}`} className="rounded border bg-background p-2" value={loss} {...base}
          onChange={(event) => { edit({ loss: event.target.value === 'hold_last_good' ? 'hold_last_good' : 'drop_to_off_air' }); }}>
          {LOSS_POLICIES.map((policy) => <option key={policy} value={policy}>{policy === 'hold_last_good' ? t`Hold last good` : t`Drop to off air`}</option>)}
        </select>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Button {...gate(reviewReason, `keyer-review-reason-${domId}`)} onClick={() => { void runReview(); }}><Trans>Review</Trans></Button>
        <span className="sr-only" id={`keyer-review-reason-${domId}`}>{reviewReason}</span>
        <Button {...gate(applyReason, `keyer-apply-reason-${domId}`)} onClick={() => { void apply(false); }}><Trans>Apply</Trans></Button>
        <span className="sr-only" id={`keyer-apply-reason-${domId}`}>{applyReason}</span>
        {reviewed !== undefined && (
          <span role="status" data-testid={`keyer-plan-${row.id}`}>
            {reviewed.plan.class === 'class1' ? t`Class 1 (seamless, one tick)` : t`Class 2 (controlled reset)`}
            {reviewed.plan.cost && <> · {reviewed.plan.cost.fits
              ? t`cost delta ${reviewed.plan.cost.delta.toFixed(2)} canvas folds, fits the budget`
              : t`cost delta ${reviewed.plan.cost.delta.toFixed(2)} canvas folds, exceeds the budget`}</>}
          </span>
        )}
        {reviewed?.plan.refusal && reviewed.plan.refusal.type !== 'on-air-protected' && (
          <span role="alert" data-testid={`keyer-refusal-${row.id}`}>{reviewed.plan.refusal.detail}</span>
        )}
      </div>
      {confirmAllow && reviewed !== undefined && (
        <div role="alertdialog" aria-label={t`Confirm on-air keyer change`} className="flex items-center gap-2 rounded border border-red-600 p-2">
          <span><Trans>{row.id} is on air: this fill/key change needs explicit confirmation to allow it.</Trans></span>
          <Button size="sm" variant="destructive" {...base} onClick={() => { void apply(true); }}><Trans>Allow on-air change</Trans></Button>
          <Button size="sm" variant="outline" onClick={() => { setConfirmAllow(false); }}><Trans>Cancel</Trans></Button>
        </div>
      )}
      {error !== undefined && <p role="alert" className="text-destructive">{error}</p>}
    </section>
  );
}
