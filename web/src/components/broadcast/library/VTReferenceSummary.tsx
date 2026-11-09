// VTReferenceSummary — the compact, honest reference line for ONE VT slot
// in the media-library workspace. The prototype's verified UX carries
// forward; its fake store/jobs/success do not (folder README).
//
// Explicit states (never invented):
//   * `empty`     — nothing loaded, "no bytes at risk", honest.
//   * `temporary` — the slot holds UNSAVED fetched bytes: a TEMP warning
//                   chip rides the row and saving is the safe action.
//   * `library`   — the slot holds a library asset reference: duration is
//                   rendered through the exact per-asset grid
//                   (`AssetDurationLabel`), rights render through the
//                   never-cleared rights chip.
//   * `pendingOperation` — an accepted-but-not-applied ledger operation:
//                   "accepted — waiting for the applied event", NEVER
//                   "working"/"done" (the applied event is the only truth).
//
// Gates: `onAir` (derived desk truth, never a second hardcoded state)
// disables unload-style actions with the reason on the control — saving
// bytes alone stays available, because protecting bytes is always safe.
// A `supported: false` gate renders the action disabled with the caller's
// localized `unavailableReason` (the backend contract is absent — the UI
// never pretends it is working; audit §9.10–11).
// Every callback is an INTENT: firing it asserts nothing about acceptance
// or application; outcomes arrive back through props only.
import type { JSX, ReactNode } from 'react';
import { useLingui } from '@lingui/react/macro';

import { TouchAction } from '../TouchAction';
import { AssetDurationLabel } from '../AssetDurationLabel';
import { Badge } from '../../ui/badge';
import type { AssetDurationSource } from '../duration';
import { OnAirPill, RightsChip } from './Chips';
import type { ActionGate, LibraryRightsStatus } from './types';

/** What ONE VT slot currently holds — an explicit, discriminated state. */
export type VTReferenceState =
  | { readonly kind: 'empty' }
  | {
      /** The slot holds unsaved fetched bytes (TEMP). */
      readonly kind: 'temporary';
      /** The source label (already localized, e.g. `Match 4 — clip.mp4`). */
      readonly label: string;
      /** Exact byte size, already formatted; null = unknown. */
      readonly byteSizeLabel: string | null;
      /** Origin host label; null = unknown. */
      readonly originHostLabel: string | null;
      /** Already-localized fetch time; null = unknown. */
      readonly fetchedAtLabel: string | null;
    }
  | {
      /** The slot holds a reference to a library asset. */
      readonly kind: 'library';
      readonly assetName: string;
      /**
       * The slot's current position label (formatted by the caller from
       * the transport's exact grid position); null = not reported.
       */
      readonly positionLabel: string | null;
      /** The referenced asset's measured grid + length (honest nulls OK). */
      readonly duration: AssetDurationSource;
      readonly rights: LibraryRightsStatus;
      /** Already-localized expiry note, shown for `expired` rights. */
      readonly rightsExpiryLabel?: string | null;
    };

/** Props for {@link VTReferenceSummary}. */
export interface VTReferenceSummaryProps {
  /** The slot id (echoed in intents so one strip can host many rows). */
  readonly slotId: string;
  /** The visible slot name (e.g. `VT 1`). */
  readonly slotName: string;
  /** The explicit discriminated slot state. */
  readonly state: VTReferenceState;
  /**
   * Derived on-air truth (from the desk programme). Gates unload-style
   * actions with the reason on the control.
   */
  readonly onAir: boolean;
  /**
   * Localized sentence shown when `onAir` blocks an action; omit for the
   * component's default on-air reason.
   */
  readonly onAirReason?: string | undefined;
  /**
   * An accepted-but-not-applied ledger operation on this slot, if any.
   * Rendered as the honest pending line — never a success claim.
   */
  readonly pendingOperation?: { readonly label: string } | null | undefined;
  /** Capability gate for saving the fetched bytes to the library. */
  readonly saveGate: ActionGate;
  /** Capability gate for dropping the slot's reference (unload). */
  readonly unloadGate: ActionGate;
  /**
   * Intent: save the fetched bytes to the library (keep the slot
   * reference). Protecting bytes is always safe — stays enabled on air.
   */
  readonly onSaveIntent?: (() => void) | undefined;
  /** Intent: save the fetched bytes, then drop the slot reference (one applied event). */
  readonly onSaveAndUnloadIntent?: (() => void) | undefined;
  /**
   * Intent: drop the slot's library reference (bytes untouched — this is a
   * reference drop, never byte deletion).
   */
  readonly onUnloadIntent?: (() => void) | undefined;
  /** Intent: open the library browser (navigation only — no mutation). */
  readonly onOpenBrowserIntent?: (() => void) | undefined;
}

/**
 * The compact reference summary for one VT slot: explicit state, honest
 * chips, capability-gated save/unload intents, honest pending line.
 */
export function VTReferenceSummary({
  slotId,
  slotName,
  state,
  onAir,
  onAirReason,
  pendingOperation = null,
  saveGate,
  unloadGate,
  onSaveIntent,
  onSaveAndUnloadIntent,
  onUnloadIntent,
  onOpenBrowserIntent,
}: VTReferenceSummaryProps): JSX.Element {
  const { t } = useLingui();
  const onAirBlock = onAir
    ? onAirReason ?? t`This slot is on air — take another source or stop first.`
    : null;

  // Save-alone gate: only the capability statement can block it — saving
  // bytes is safe on air. The reason rides the control either way.
  const saveBlocked = !saveGate.supported;
  const saveReason = saveBlocked ? saveGate.unavailableReason : null;

  // Unload-style gates stack: capability first, then the on-air guard.
  const unloadBlocked = !unloadGate.supported || onAir;
  const unloadReason = !unloadGate.supported
    ? unloadGate.unavailableReason
    : onAirBlock;

  return (
    <article
      data-testid="vt-reference-summary"
      data-slot-id={slotId}
      data-state={state.kind}
      data-on-air={onAir ? 'true' : 'false'}
      aria-label={slotName}
      className="flex min-w-0 flex-col gap-2 rounded-lg border p-3"
    >
      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <span className="text-sm font-medium">{slotName}</span>
        {onAir ? <OnAirPill /> : null}
        <span className="min-w-0 flex-1 basis-full break-words text-sm sm:basis-auto">
          {state.kind === 'temporary'
            ? state.label
            : state.kind === 'library'
              ? state.assetName
              : t`Nothing loaded — no bytes at risk`}
        </span>
      </div>

      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        {state.kind === 'temporary' ? (
          <>
            <BadgeTemp byteSizeLabel={state.byteSizeLabel} />
            {state.originHostLabel !== null ? (
              <Badge variant="outline" data-testid="chip-origin">
                {state.originHostLabel}
              </Badge>
            ) : null}
          </>
        ) : null}
        {state.kind === 'library' ? (
          <RightsChip
            rights={state.rights}
            compact
            expiryLabel={state.rightsExpiryLabel}
          />
        ) : null}
      </div>

      {state.kind === 'temporary' && state.fetchedAtLabel !== null ? (
        <p className="min-w-0 break-words text-xs text-muted-foreground">
          {t`Fetched ${state.fetchedAtLabel}`}
        </p>
      ) : null}

      {state.kind === 'library' ? (
        <p className="min-w-0 break-words text-xs text-muted-foreground">
          {state.positionLabel !== null
            ? t`Position ${state.positionLabel}`
            : t`Position not reported`}
          {' · '}
          <AssetDurationLabel asset={state.duration} />
        </p>
      ) : null}

      {pendingOperation !== null ? (
        <p
          data-testid="vt-reference-pending"
          className="min-w-0 break-words text-xs text-muted-foreground"
        >
          {t`${pendingOperation.label} accepted — waiting for the applied event.`}
        </p>
      ) : null}

      <div className="flex min-w-0 flex-wrap items-start gap-2">
        {state.kind === 'temporary' ? (
          <>
            <ActionCell>
              <TouchAction
                variant="default"
                disabled={saveBlocked}
                disabledReason={saveReason ?? undefined}
                onPress={(): void => {
                  onSaveIntent?.();
                }}
              >
                {t`Save`}
              </TouchAction>
            </ActionCell>
            <ActionCell>
              <TouchAction
                variant="outline"
                disabled={unloadBlocked}
                disabledReason={unloadReason ?? undefined}
                onPress={(): void => {
                  onSaveAndUnloadIntent?.();
                }}
              >
                {t`Save & unload`}
              </TouchAction>
            </ActionCell>
          </>
        ) : null}
        {state.kind === 'library' ? (
          <ActionCell>
            <TouchAction
              variant="outline"
              disabled={unloadBlocked}
              disabledReason={unloadReason ?? undefined}
              onPress={(): void => {
                onUnloadIntent?.();
              }}
            >
              {t`Unload`}
            </TouchAction>
          </ActionCell>
        ) : null}
        {state.kind === 'empty' && onOpenBrowserIntent !== undefined ? (
          <ActionCell>
            <TouchAction
              variant="outline"
              onPress={onOpenBrowserIntent}
            >
              {t`Load from library…`}
            </TouchAction>
          </ActionCell>
        ) : null}
      </div>
    </article>
  );
}

/** One action + its visible disabled reason, grouped for flex layout. */
function ActionCell({ children }: { readonly children: ReactNode }): JSX.Element {
  return <div className="flex min-w-0 flex-col gap-1">{children}</div>;
}

/** The TEMP warning chip: unsaved fetched bytes are at risk in this slot. */
function BadgeTemp({
  byteSizeLabel,
}: {
  readonly byteSizeLabel: string | null;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <Badge variant="offline" data-testid="chip-temp">
      <span aria-hidden="true">⚠</span>
      {byteSizeLabel !== null
        ? t`TEMP — unsaved bytes (${byteSizeLabel})`
        : t`TEMP — unsaved bytes`}
    </Badge>
  );
}
