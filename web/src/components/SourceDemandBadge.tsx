// Source lifecycle badge: idle / starting / ready / failed, with icon + text and
// who uses the source in the accessible description (never colour alone). This
// is not the tile signal state (Live / Stale / No signal) and not tally.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { CircleDot, CirclePause, Loader2, TriangleAlert } from 'lucide-react';

import type { SourceDemand } from '../api/sourceStatus';
import { Badge } from './ui/badge';

/** Human label for a consumer kind token. */
function useConsumerLabel(): (kind: string) => string {
  const { t } = useLingui();
  return (kind: string): string => {
    switch (kind) {
      case 'layout_cell':
        return t`layout cell`;
      case 'audio_route':
        return t`audio route`;
      case 'subtitle_route':
        return t`subtitle route`;
      case 'preview':
        return t`preview`;
      case 'hls_session':
        return t`HLS session`;
      case 'event_automation':
        return t`event automation`;
      case 'always_on':
        return t`Always on setting`;
      case 'manual':
        return t`started by an operator`;
      default:
        return kind;
    }
  };
}

/** Who consumes a source, as one sentence. */
function useConsumerSummary(): (demand: SourceDemand | undefined) => string {
  const { t } = useLingui();
  const label = useConsumerLabel();
  return (demand: SourceDemand | undefined): string => {
    if (demand === undefined || demand.consumers.length === 0) {
      return t`Not in use: this source is not running and costs nothing.`;
    }
    const parts = demand.consumers.map((c) => (c.kind === 'always_on' ? label(c.kind) : `${label(c.kind)} ${c.label}`));
    return t`In use by: ${parts.join(', ')}`;
  };
}

/** Where a starting source is, as a short phrase. */
function useStartingPhase(): (demand: SourceDemand | undefined) => string | null {
  const { t } = useLingui();
  return (demand: SourceDemand | undefined): string | null => {
    switch (demand?.starting_phase) {
      case 'queued':
        return t`waiting for a start slot`;
      case 'opening':
        return t`opening`;
      case 'awaiting_media':
        return t`waiting for first media`;
      default:
        return null;
    }
  };
}

/** A lifecycle pill. An absent status reads as idle. */
export function SourceDemandBadge({ demand }: { readonly demand: SourceDemand | undefined }): JSX.Element {
  const summary = useConsumerSummary()(demand);
  const startingPhase = useStartingPhase()(demand);
  const phase = demand?.phase ?? 'idle';
  const detail = demand?.detail;
  const title = detail !== undefined && detail !== null ? `${summary} ${detail}` : summary;
  switch (phase) {
    case 'ready':
      return (
        <Badge variant="live" title={title}>
          <CircleDot className="size-3.5" aria-hidden="true" />
          <span><Trans>Ready</Trans></span>
          <span className="sr-only">{title}</span>
        </Badge>
      );
    case 'starting':
      return (
        <Badge variant="outline" title={title}>
          <Loader2 className="size-3.5 animate-spin motion-reduce:animate-none" aria-hidden="true" />
          <span>
            {startingPhase === null ? <Trans>Starting</Trans> : <Trans>Starting: {startingPhase}</Trans>}
          </span>
          <span className="sr-only">{title}</span>
        </Badge>
      );
    case 'failed':
      return (
        <Badge variant="nosignal" title={title}>
          <TriangleAlert className="size-3.5" aria-hidden="true" />
          <span><Trans>Failed, retrying</Trans></span>
          <span className="sr-only">{title}</span>
        </Badge>
      );
    case 'idle':
      return (
        <Badge variant="outline" title={title}>
          <CirclePause className="size-3.5" aria-hidden="true" />
          <span><Trans>Idle</Trans></span>
          <span className="sr-only">{title}</span>
        </Badge>
      );
  }
}
