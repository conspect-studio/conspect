// Operator wording of the browser-cast states.
import { t } from '@lingui/core/macro';

import type { CastSnapshot } from './castController';

/** A short label of a cast status. */
export function castStatusLabel(status: CastSnapshot['status']): string {
  switch (status) {
    case 'idle':
      return t`Not casting`;
    case 'preparing':
      return t`Preparing the stream…`;
    case 'connecting':
      return t`Connecting to the TV…`;
    case 'loading':
      return t`Loading on the TV…`;
    case 'buffering':
      return t`Buffering…`;
    case 'playing':
      return t`Playing`;
    case 'stopping':
      return t`Stopping…`;
    case 'error':
      return t`Cast failed`;
  }
}

/** `1920×1080 · 7.2 s behind live` from a receiver report, or `null`. */
export function castQualityLine(snapshot: CastSnapshot): string | null {
  const report = snapshot.report;
  if (report === null) return null;
  const parts: string[] = [];
  if (report.width !== null && report.height !== null && report.width > 0 && report.height > 0) {
    parts.push(`${String(report.width)}×${String(report.height)}`);
  }
  if (report.latencyS !== null) {
    const seconds = report.latencyS.toFixed(1);
    parts.push(t`${seconds} s behind live`);
  }
  return parts.length === 0 ? null : parts.join(' · ');
}
