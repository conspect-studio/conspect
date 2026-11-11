// Shared honest-state chips for the library surfaces. Every chip pairs its
// hue with REQUIRED text content (never color alone — WCAG 1.4.1) and
// renders an explicit fallback for every unknown/absent value. Rights are
// the load-bearing case: an UNKNOWN or EXPIRED status is always visible as
// such and is never silently presented as cleared (audit §9.3, §9.11).
import { TriangleAlert } from 'lucide-react';
import type { JSX } from 'react';
import { useLingui } from '@lingui/react/macro';

import { Badge } from '../../ui/badge';
import type {
  LibraryRightsStatus,
  LibraryTranscriptState,
} from './types';

/** The on-air pill — derived desk truth, always red-tinted with its glyph. */
export function OnAirPill(): JSX.Element {
  const { t } = useLingui();
  return (
    <Badge variant="destructive" data-testid="chip-on-air">
      <span aria-hidden="true">●</span>
      {t`ON AIR`}
    </Badge>
  );
}

/**
 * One asset's rights chip. `compact` hides a `cleared` status (silence is
 * not a claim); `unknown` and `expired` are ALWAYS visible — unknown is
 * never rendered as cleared.
 */
export function RightsChip({
  rights,
  compact = false,
  expiryLabel,
}: {
  readonly rights: LibraryRightsStatus;
  /** Hide the chip when rights are `cleared` (silence is not a claim). */
  readonly compact?: boolean | undefined;
  /** Already-localized expiry note, shown for `expired`. */
  readonly expiryLabel?: string | null | undefined;
}): JSX.Element | null {
  const { t } = useLingui();
  if (rights === 'cleared') {
    if (compact) {
      return null;
    }
    return (
      <Badge variant="live" data-testid="chip-rights">
        {t`Rights cleared`}
      </Badge>
    );
  }
  if (rights === 'expired') {
    return (
      <Badge
        variant="destructive"
        data-testid="chip-rights"
        title={expiryLabel ?? undefined}
      >
        <TriangleAlert aria-hidden="true" size={12} />
        {expiryLabel ? t`Rights expired — ${expiryLabel}` : t`Rights expired`}
      </Badge>
    );
  }
  return (
    <Badge variant="offline" data-testid="chip-rights">
      <TriangleAlert aria-hidden="true" size={12} />
      {t`Rights unknown`}
    </Badge>
  );
}

/**
 * One asset's transcript state chip (the STT contract is CONCEPT today —
 * the label states the state, never a measured-looking value).
 */
export function TranscriptStateChip({
  state,
}: {
  readonly state: LibraryTranscriptState;
}): JSX.Element {
  const { t } = useLingui();
  if (state === 'done') {
    return (
      <Badge variant="live" data-testid="chip-transcript">
        {t`Transcript done`}
      </Badge>
    );
  }
  if (state === 'processing') {
    return (
      <Badge variant="stale" data-testid="chip-transcript">
        {t`Transcript processing`}
      </Badge>
    );
  }
  if (state === 'failed') {
    return (
      <Badge variant="destructive" data-testid="chip-transcript">
        <TriangleAlert aria-hidden="true" size={12} />
        {t`Transcript failed`}
      </Badge>
    );
  }
  return (
    <Badge variant="secondary" data-testid="chip-transcript">
      {t`No transcript`}
    </Badge>
  );
}

/** A usage chip: the asset is held by a slot; on-air usage turns red. */
export function InUseChip({
  slotName,
  onAir,
}: {
  readonly slotName: string;
  readonly onAir: boolean;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <Badge
      variant={onAir ? 'destructive' : 'secondary'}
      data-testid="chip-in-use"
    >
      {onAir ? t`In ${slotName} — ON AIR` : t`In ${slotName}`}
    </Badge>
  );
}

/** One asset's kind chip (video / audio / derivative clip). */
export function AssetKindChip({
  kind,
}: {
  readonly kind: 'video' | 'audio' | 'clip';
}): JSX.Element {
  const { t } = useLingui();
  const label = kind === 'video' ? t`Video` : kind === 'audio' ? t`Audio` : t`Clip`;
  return (
    <Badge variant="outline" data-testid="chip-kind">
      {label}
    </Badge>
  );
}
