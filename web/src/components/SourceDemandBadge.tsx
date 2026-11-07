// Source demand badge: idle / starting / live / failed, with icon + text and
// the consumers in the accessible description (never colour alone).
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
      case 'event_journal':
        return t`event journal`;
      case 'always_on':
        return t`always on`;
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
      return t`No consumer: this source is not running and costs nothing.`;
    }
    const parts = demand.consumers.map((c) => `${label(c.kind)} ${c.label}`);
    return t`Consumed by: ${parts.join(', ')}`;
  };
}

/** A demand-state pill. An absent status reads as idle. */
export function SourceDemandBadge({ demand }: { readonly demand: SourceDemand | undefined }): JSX.Element {
  const summary = useConsumerSummary()(demand);
  const phase = demand?.phase ?? 'idle';
  const detail = demand?.detail;
  const title = detail !== undefined && detail !== null ? `${summary} ${detail}` : summary;
  switch (phase) {
    case 'live':
      return (
        <Badge variant="live" title={title}>
          <CircleDot className="size-3.5" aria-hidden="true" />
          <span><Trans>Live</Trans></span>
          <span className="sr-only">{title}</span>
        </Badge>
      );
    case 'starting':
      return (
        <Badge variant="reconnecting" title={title}>
          <Loader2 className="size-3.5 animate-spin motion-reduce:animate-none" aria-hidden="true" />
          <span><Trans>Starting</Trans></span>
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
