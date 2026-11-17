// Per-DSK strip next to MIX/WIPE/DIP (design note §10, K5): TIE toggle, CUT
// and AUTO, rate in frames (1..300), and an ON AIR lamp with ramp progress
// computed from the snapshot's `ramp` descriptor. Every disabled control
// states its reason as `title` and `aria-describedby` (design note §10).
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../ui/button';
import { Input } from '../ui/input';
import type { KeyerRow } from '../../api/keyers';
import { keyerRampWeight, keyerRamping } from '../../api/keyers';

export interface KeyerStripProps {
  readonly row: KeyerRow;
  /** The output tick now: the snapshot's authoritative tick, extrapolated at the exact cadence between snapshots. */
  readonly tick: number;
  /** Non-empty reason disables every control on the strip (engine unavailable, stale, in-flight command, …). */
  readonly disabledReason: string | undefined;
  readonly onTie: (tie: boolean) => void;
  readonly onCut: () => void;
  readonly onAuto: (rateFrames: number) => void;
}

const MIN_RATE = 1;
const MAX_RATE = 300;

/** One DSK's TIE/CUT/AUTO/rate/lamp row. */
export function KeyerStrip({ row, tick, disabledReason, onTie, onCut, onAuto }: KeyerStripProps): JSX.Element {
  const { t } = useLingui();
  // Follows the committed rate until the operator types one.
  const [rateEdit, setRateEdit] = useState<string>();
  const rateInput = rateEdit ?? String(row.rate);
  const parsedRate = Number(rateInput);
  const rateValid = Number.isInteger(parsedRate) && parsedRate >= MIN_RATE && parsedRate <= MAX_RATE;
  const rateReason = rateValid ? undefined : t`Rate must be a whole number of frames, 1–300`;
  const ramping = keyerRamping(row);
  const weight = ramping ? keyerRampWeight(row, tick) : undefined;
  const describedById = `keyer-strip-reason-${row.id}`;
  const reason = disabledReason;
  const cutReason = reason;
  const autoReason = reason ?? rateReason;
  const tieReason = reason;
  return (
    <div className="flex flex-wrap items-center gap-2 rounded border p-2" aria-label={t`Keyer ${row.id} strip`} data-testid={`keyer-strip-${row.id}`}>
      <span className="font-semibold uppercase">{row.id}</span>
      <span
        role="status"
        aria-label={row.on_air ? t`On air` : t`Off air`}
        data-testid={`keyer-lamp-${row.id}`}
        data-on-air={row.on_air}
        data-ramping={ramping}
        className={`rounded px-1 text-xs text-white ${row.on_air ? 'bg-red-600' : 'bg-muted-foreground'}`}
      >
        {row.on_air ? t`ON AIR` : t`OFF AIR`}
        {ramping && weight !== undefined && <span data-testid={`keyer-ramp-${row.id}`} data-weight={weight.toFixed(3)}> · {Math.round(weight * 100)}%</span>}
      </span>
      {row.stale && <span role="status" className="rounded bg-amber-600 px-1 text-xs text-white" data-testid={`keyer-stale-${row.id}`}><Trans>Stale</Trans></span>}
      {row.pending && <span role="status" className="rounded bg-amber-700 px-1 text-xs text-white" data-testid={`keyer-pending-${row.id}`}><Trans>Pending</Trans></span>}
      <label className="flex items-center gap-1 text-sm">
        <input
          type="checkbox"
          checked={row.tie}
          disabled={tieReason !== undefined}
          title={tieReason}
          aria-describedby={tieReason !== undefined ? describedById : undefined}
          onChange={(event) => { onTie(event.target.checked); }}
        />
        <Trans>Tie</Trans>
      </label>
      <Button
        size="sm"
        disabled={cutReason !== undefined}
        title={cutReason}
        aria-describedby={cutReason !== undefined ? describedById : undefined}
        onClick={onCut}
      >
        <Trans>Cut</Trans>
      </Button>
      <label className="sr-only" htmlFor={`keyer-rate-${row.id}`}><Trans>Rate in frames for {row.id}</Trans></label>
      <Input
        id={`keyer-rate-${row.id}`}
        type="number"
        min={MIN_RATE}
        max={MAX_RATE}
        value={rateInput}
        disabled={reason !== undefined}
        title={reason}
        aria-describedby={reason !== undefined ? describedById : undefined}
        aria-invalid={!rateValid}
        className="w-20"
        onChange={(event) => { setRateEdit(event.target.value); }}
      />
      <Button
        size="sm"
        disabled={autoReason !== undefined}
        title={autoReason}
        aria-describedby={autoReason !== undefined ? describedById : undefined}
        onClick={() => { onAuto(parsedRate); }}
      >
        <Trans>Auto</Trans>
      </Button>
      <span className="sr-only" id={describedById}>{reason ?? rateReason}</span>
    </div>
  );
}
