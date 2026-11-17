// A keyer panel (design note §10, K5): fill and key pickers limited to
// sources the caller may read (inputs `in:<n>` are never offered), type
// (luma|linear) with clip/gain/invert, mask, fly x/y/w/h with corner presets,
// and the loss policy. Edits go through `keyer_set` with the plan shown
// first: cost/capacity refusal, and the on-air-protected 409 offers the
// explicit "allow" choice before a fill/key change of a keyer on air.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../ui/button';
import { Input } from '../ui/input';
import { Label } from '../ui/label';
import type { KeyerFields, KeyerKind, KeyerLossPolicy, KeyerPlan, KeyerRow, KeyerSize } from '../../api/keyers';
import { FLY_PRESETS, flyPresetRect, keyerFillRedacted, keyerFillSource, keyerKeyRedacted, keyerKeySource } from '../../api/keyers';
import type { FlyPreset } from '../../api/keyers';

export interface KeyerPanelProps {
  readonly row: KeyerRow;
  readonly sources: readonly { readonly id: string; readonly name: string }[];
  readonly disabledReason: string | undefined;
  /** The canvas size used to compute fly corner/centre presets (operator-entered; the API carries no canvas size). */
  readonly canvas: KeyerSize;
  /** Classify a `keyer_set` draft without applying it. */
  readonly onPlan: (fields: KeyerFields) => Promise<KeyerPlan>;
  /** Apply a `keyer_set` draft; `allow` is `on_air: "allow"` once the operator confirms the protected change. */
  readonly onApply: (fields: KeyerFields, allow: boolean) => Promise<void>;
}

const LOSS_POLICIES: readonly KeyerLossPolicy[] = ['drop_to_off_air', 'hold_last_good'];

/** The draft fields this panel edits, pre-filled from the current row. */
function draftFromRow(row: KeyerRow): {
  fill: string; key: string; type: KeyerKind; clip: string; gain: string; invert: boolean;
  loss: KeyerLossPolicy; fly: { x: string; y: string; w: string; h: string } | undefined;
} {
  return {
    fill: keyerFillSource(row) ?? '',
    key: keyerKeySource(row) ?? '',
    type: row.type === 'luma' ? 'luma' : 'linear',
    clip: '0',
    gain: '1',
    invert: false,
    loss: row.loss === 'hold_last_good' ? 'hold_last_good' : 'drop_to_off_air',
    fly: undefined,
  };
}

/** One keyer's fill/key/type/geometry/loss editor, gated by a plan-first apply. */
export function KeyerPanel({ row, sources, disabledReason, canvas, onPlan, onApply }: KeyerPanelProps): JSX.Element {
  const { t } = useLingui();
  const [draft, setDraft] = useState(() => draftFromRow(row));
  const [flyPreset, setFlyPreset] = useState<FlyPreset>('full');
  const [flyBox, setFlyBox] = useState({ w: String(Math.round(canvas.w / 2)), h: String(Math.round(canvas.h / 2)) });
  const [plan, setPlan] = useState<KeyerPlan>();
  const [planning, setPlanning] = useState(false);
  const [applying, setApplying] = useState(false);
  const [confirmAllow, setConfirmAllow] = useState(false);
  const [error, setError] = useState<string>();
  const fillRedacted = keyerFillRedacted(row);
  const keyRedacted = keyerKeyRedacted(row);
  const sourceChoices = sources; // `useSources()` already excludes unreadable/`in:<n>` entries.

  const buildFields = (): KeyerFields => {
    const fly = flyPreset === 'full' ? undefined : flyPresetRect(flyPreset, canvas, { w: Number(flyBox.w) || 0, h: Number(flyBox.h) || 0 });
    return {
      keyer: row.id,
      ...(draft.fill === '' ? {} : { fill: draft.fill }),
      key: draft.key === '' ? null : draft.key,
      type: draft.type,
      ...(draft.type === 'luma' ? { clip: Number(draft.clip), gain: Number(draft.gain) } : {}),
      invert: draft.invert,
      loss: draft.loss,
      ...(fly === undefined ? {} : { fly }),
    };
  };

  const changesSources = draft.fill !== (keyerFillSource(row) ?? '') || draft.key !== (keyerKeySource(row) ?? '');
  const disabled = disabledReason !== undefined;

  const runPlan = async (): Promise<KeyerPlan | undefined> => {
    setPlanning(true);
    setError(undefined);
    try {
      const result = await onPlan(buildFields());
      setPlan(result);
      return result;
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      return undefined;
    } finally {
      setPlanning(false);
    }
  };

  const apply = async (allow: boolean): Promise<void> => {
    setApplying(true);
    setError(undefined);
    try {
      await onApply(buildFields(), allow);
      setConfirmAllow(false);
      setPlan(undefined);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setApplying(false);
    }
  };

  const onReview = async (): Promise<void> => {
    const result = await runPlan();
    if (!result) return;
    if (result.refusal?.type === 'on-air-protected' && changesSources) {
      setConfirmAllow(true);
      return;
    }
    if (result.can_take) await apply(false);
  };

  return (
    <section className="space-y-2 rounded border p-3" aria-label={t`Keyer ${row.id} panel`} data-testid={`keyer-panel-${row.id}`}>
      <h3 className="font-semibold uppercase">{row.id}</h3>
      <div className="flex flex-wrap items-end gap-3">
        <div className="space-y-1">
          <Label htmlFor={`keyer-fill-${row.id}`}><Trans>Fill</Trans></Label>
          {fillRedacted
            ? <p id={`keyer-fill-${row.id}`} role="status" data-testid={`keyer-fill-redacted-${row.id}`}><Trans>redacted</Trans></p>
            : <select
                id={`keyer-fill-${row.id}`}
                className="rounded border bg-background p-2"
                value={draft.fill}
                disabled={disabled}
                onChange={(event) => { setDraft({ ...draft, fill: event.target.value }); }}
              >
                <option value="">{t`Choose a source`}</option>
                {sourceChoices.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
              </select>}
        </div>
        <div className="space-y-1">
          <Label htmlFor={`keyer-key-${row.id}`}><Trans>Key</Trans></Label>
          {keyRedacted
            ? <p id={`keyer-key-${row.id}`} role="status" data-testid={`keyer-key-redacted-${row.id}`}><Trans>redacted</Trans></p>
            : <select
                id={`keyer-key-${row.id}`}
                className="rounded border bg-background p-2"
                value={draft.key}
                disabled={disabled}
                onChange={(event) => { setDraft({ ...draft, key: event.target.value }); }}
              >
                <option value=""><Trans>Self key (fill alpha)</Trans></option>
                {sourceChoices.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
              </select>}
        </div>
        <div className="space-y-1">
          <Label htmlFor={`keyer-type-${row.id}`}><Trans>Type</Trans></Label>
          <select
            id={`keyer-type-${row.id}`}
            className="rounded border bg-background p-2"
            value={draft.type}
            disabled={disabled}
            onChange={(event) => { setDraft({ ...draft, type: event.target.value === 'luma' ? 'luma' : 'linear' }); }}
          >
            <option value="linear">{t`Linear`}</option>
            <option value="luma">{t`Luma`}</option>
          </select>
        </div>
      </div>
      {draft.type === 'luma' && (
        <div className="flex flex-wrap items-end gap-3">
          <div className="space-y-1"><Label htmlFor={`keyer-clip-${row.id}`}><Trans>Clip</Trans></Label>
            <Input id={`keyer-clip-${row.id}`} type="number" min="0" max="1" step="0.01" value={draft.clip} disabled={disabled} className="w-24" onChange={(event) => { setDraft({ ...draft, clip: event.target.value }); }} /></div>
          <div className="space-y-1"><Label htmlFor={`keyer-gain-${row.id}`}><Trans>Gain</Trans></Label>
            <Input id={`keyer-gain-${row.id}`} type="number" min="0" step="0.01" value={draft.gain} disabled={disabled} className="w-24" onChange={(event) => { setDraft({ ...draft, gain: event.target.value }); }} /></div>
        </div>
      )}
      <label className="flex items-center gap-1 text-sm">
        <input type="checkbox" checked={draft.invert} disabled={disabled} onChange={(event) => { setDraft({ ...draft, invert: event.target.checked }); }} />
        <Trans>Invert matte</Trans>
      </label>
      <fieldset className="space-y-1" disabled={disabled}>
        <legend className="text-sm font-medium"><Trans>Fly</Trans></legend>
        <div className="flex flex-wrap items-end gap-2" role="group" aria-label={t`Fly preset`}>
          {FLY_PRESETS.map((preset) => (
            <Button key={preset} type="button" size="sm" variant={flyPreset === preset ? 'default' : 'outline'} disabled={disabled} onClick={() => { setFlyPreset(preset); }}>
              {preset === 'top_left' ? t`TL` : preset === 'top_right' ? t`TR` : preset === 'bottom_left' ? t`BL` : preset === 'bottom_right' ? t`BR` : preset === 'centre' ? t`Centre` : t`Full`}
            </Button>
          ))}
        </div>
        {flyPreset !== 'full' && (
          <div className="flex flex-wrap items-end gap-2">
            <div className="space-y-1"><Label htmlFor={`keyer-fly-w-${row.id}`}><Trans>Width</Trans></Label>
              <Input id={`keyer-fly-w-${row.id}`} type="number" min="1" value={flyBox.w} disabled={disabled} className="w-24" onChange={(event) => { setFlyBox({ ...flyBox, w: event.target.value }); }} /></div>
            <div className="space-y-1"><Label htmlFor={`keyer-fly-h-${row.id}`}><Trans>Height</Trans></Label>
              <Input id={`keyer-fly-h-${row.id}`} type="number" min="1" value={flyBox.h} disabled={disabled} className="w-24" onChange={(event) => { setFlyBox({ ...flyBox, h: event.target.value }); }} /></div>
          </div>
        )}
      </fieldset>
      <div className="space-y-1">
        <Label htmlFor={`keyer-loss-${row.id}`}><Trans>Loss policy</Trans></Label>
        <select
          id={`keyer-loss-${row.id}`}
          className="rounded border bg-background p-2"
          value={draft.loss}
          disabled={disabled}
          onChange={(event) => { setDraft({ ...draft, loss: event.target.value === 'hold_last_good' ? 'hold_last_good' : 'drop_to_off_air' }); }}
        >
          {LOSS_POLICIES.map((policy) => <option key={policy} value={policy}>{policy === 'hold_last_good' ? t`Hold last good` : t`Drop to off air`}</option>)}
        </select>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Button disabled={disabled || planning} title={disabledReason} aria-describedby={disabled ? `keyer-panel-reason-${row.id}` : undefined} onClick={() => { void onReview(); }}>
          {planning ? t`Reviewing…` : t`Review & apply`}
        </Button>
        {disabled && <span className="sr-only" id={`keyer-panel-reason-${row.id}`}>{disabledReason}</span>}
        {plan?.cost && (
          <span role="status" data-testid={`keyer-cost-${row.id}`}>
            <Trans>Cost delta {plan.cost.delta.toFixed(2)} — {plan.cost.fits ? t`fits the budget` : t`exceeds the budget`}</Trans>
          </span>
        )}
        {plan?.refusal && plan.refusal.type !== 'on-air-protected' && (
          <span role="alert" data-testid={`keyer-refusal-${row.id}`}>{plan.refusal.detail}</span>
        )}
      </div>
      {confirmAllow && (
        <div role="alertdialog" aria-label={t`Confirm on-air keyer change`} className="flex items-center gap-2 rounded border border-red-600 p-2">
          <span><Trans>{row.id} is on air: this fill/key change needs explicit confirmation to allow it.</Trans></span>
          <Button size="sm" variant="destructive" disabled={applying} onClick={() => { void apply(true); }}><Trans>Allow on-air change</Trans></Button>
          <Button size="sm" variant="outline" disabled={applying} onClick={() => { setConfirmAllow(false); }}><Trans>Cancel</Trans></Button>
        </div>
      )}
      {error !== undefined && <p role="alert" className="text-destructive">{error}</p>}
    </section>
  );
}
