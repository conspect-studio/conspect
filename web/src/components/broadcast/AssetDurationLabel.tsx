// AssetDurationLabel — the display half of the per-asset duration contract
// (correction-4 comfort §N8): browser cards and inspectors once printed
// INVALID for a valid 48 kHz asset, and `undefined` for a clip's missing
// metadata. This label renders {@link formatAssetDuration}'s honest states:
// a real label when measured, `Duration not measured` on a known cadence,
// `Duration unknown` when no cadence is known — and an inherited grid is
// marked as such, so a derivative never masquerades as self-measured.
import type { JSX } from 'react';
import { useLingui } from '@lingui/react/macro';

import { describeDurationPrecision } from './duration';
import type { AssetDurationSource } from './duration';
import { formatAssetDuration } from './duration';

/** Props for {@link AssetDurationLabel}. */
export interface AssetDurationLabelProps {
  /** The asset whose duration is displayed. */
  readonly asset: AssetDurationSource;
  /** The parent asset whose grid a derivative may fall back to. */
  readonly parent?: AssetDurationSource | undefined;
}

/**
 * One asset's duration with its precision: `00:00:02.000 · 48 kHz · SMP
 * 96,000` for audio, `00:00:10:00 · 29.97 (30000/1001) fps NDF` for
 * fractional video — or a meaningful unavailable state, never INVALID or
 * NaN.
 */
export function AssetDurationLabel({
  asset,
  parent,
}: AssetDurationLabelProps): JSX.Element {
  const { t } = useLingui();
  const format = formatAssetDuration(asset, parent);
  const precision = describeDurationPrecision(format);
  const durationText =
    format.state === 'formatted' && format.duration !== null
      ? format.duration
      : format.state === 'unmeasured'
        ? t`Duration not measured`
        : t`Duration unknown`;
  const inheritedSuffix =
    format.gridOrigin === 'inherited' ? ` · ${t`inherited from source`}` : '';
  return (
    <span
      data-state={format.state}
      className="inline-flex min-w-0 flex-wrap items-baseline gap-x-2 break-words"
    >
      <span className="font-mono text-sm">{durationText}</span>
      <span className="text-xs text-muted-foreground">
        {precision ?? t`cadence unknown`}
        {inheritedSuffix}
      </span>
    </span>
  );
}
