// The BKGD / KEY arm strip (design note §10, K3 #388): which layers the next
// CUT or AUTO changes. Each toggle is lit while the engine's `armed` view
// lists it (never optimistically); toggling hands the next full set
// `{background, keys}` to `onArm`, which plans and then commands `arm` behind
// the page's shared submission gate. Each KEY carries its ON AIR lamp.
//
// Redaction: `armed.keys` and the USK rows list only keyers the caller may
// read, so an unreadable key is never shown. A readable key whose fill or key
// source is hidden is shown (its lamp too) but is not toggleable: `arm`
// needs Read on an armed key's sources. Because `arm` replaces the whole set,
// an armed key with hidden sources blocks every toggle (the server would
// refuse the set with 403). An armed key the caller may not read at all is
// absent from `armed.keys`; the server then sends `armed.complete = false`
// (a boolean only) and refuses any `arm` (409 `arm-hidden-keys`, review
// K5-R2-01), so the whole strip is read-only with that reason, never a
// toggle that would silently disarm the hidden key. The set may never become empty (422), so the last
// lit toggle cannot be switched off. Every disabled toggle states its reason
// as `title` and `aria-describedby`.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../ui/button';
import type { ArmedView, ArmSet, ArmTarget, KeyerRow } from '../../api/keyers';
import { keyerFillRedacted, keyerKeyRedacted, toggleArmed } from '../../api/keyers';

/** At most four upstream keyers per M/E (K3). */
export const MAX_ARM_KEYS = 4;

export interface ArmStripProps {
  /** The engine's armed set as the caller may see it (`complete` may be absent from an older backend). */
  readonly armed: Omit<ArmedView, 'complete'> & { readonly complete?: boolean };
  /** The M/E's upstream keyer rows the caller may read, in declared order. */
  readonly keys: readonly KeyerRow[];
  /** Non-empty reason disables every toggle (engine unavailable, a request or outcome in flight, a transition running, …). */
  readonly disabledReason: string | undefined;
  /** Plan and command `arm` with the next full set. */
  readonly onArm: (next: ArmSet) => void;
}

function sourcesHidden(row: KeyerRow): boolean {
  return keyerFillRedacted(row) || keyerKeyRedacted(row);
}

/** BKGD and KEY 1–4 toggles with per-key ON AIR lamps. */
export function ArmStrip({ armed, keys, disabledReason, onArm }: ArmStripProps): JSX.Element {
  const { t } = useLingui();
  const shown = keys.slice(0, MAX_ARM_KEYS);
  const litCount = (armed.background ? 1 : 0) + armed.keys.length;
  const blockedByHidden = shown.some((row) => armed.keys.includes(row.id) && sourcesHidden(row));
  // Older backends send no flag: their view is the whole set.
  const incomplete = !(armed.complete ?? true);
  const reasonFor = (lit: boolean, row?: KeyerRow): string | undefined => {
    if (disabledReason !== undefined) return disabledReason;
    if (incomplete) return t`A key you cannot see is armed, so the arm set cannot be changed here`;
    if (row !== undefined && sourcesHidden(row)) return t`This key's sources are hidden from you, so you cannot arm it`;
    if (blockedByHidden) return t`An armed key's sources are hidden from you, so the arm set cannot be changed here`;
    if (lit && litCount <= 1) return t`At least one of BKGD or a key must stay armed`;
    return undefined;
  };
  const toggle = (target: ArmTarget, reason: string | undefined, label: string, lit: boolean, testId: string) => {
    const describedBy = `${testId}-reason`;
    return (
      <>
        <Button
          type="button"
          size="sm"
          variant={lit ? 'default' : 'outline'}
          className={lit ? 'bg-amber-500 text-black hover:bg-amber-400' : undefined}
          aria-pressed={lit}
          data-testid={testId}
          data-armed={lit}
          disabled={reason !== undefined}
          title={reason}
          aria-describedby={reason !== undefined ? describedBy : undefined}
          onClick={() => { onArm(toggleArmed(armed, target)); }}
        >
          {label}
        </Button>
        <span className="sr-only" id={describedBy}>{reason}</span>
      </>
    );
  };
  return (
    <section className="flex flex-wrap items-center gap-3 rounded border p-2" aria-label={t`Next transition`} data-testid="arm-strip">
      <span className="text-sm font-semibold"><Trans>Next transition</Trans></span>
      {toggle({ kind: 'background' }, reasonFor(armed.background), t`BKGD`, armed.background, 'keyer-arm-background')}
      {shown.map((row, index) => {
        const lit = armed.keys.includes(row.id);
        return (
          <span key={row.id} className="flex items-center gap-1" data-testid={`keyer-arm-slot-${row.id}`}>
            {toggle({ kind: 'key', id: row.id }, reasonFor(lit, row), t`KEY ${index + 1} (${row.id})`, lit, `keyer-arm-${row.id}`)}
            <span
              role="status"
              aria-label={row.on_air ? t`KEY ${index + 1} on air` : t`KEY ${index + 1} off air`}
              data-testid={`keyer-arm-lamp-${row.id}`}
              data-on-air={row.on_air}
              className={`rounded px-1 text-xs text-white ${row.on_air ? 'bg-red-600' : 'bg-muted-foreground'}`}
            >
              {row.on_air ? t`ON AIR` : t`OFF AIR`}
            </span>
          </span>
        );
      })}
    </section>
  );
}
