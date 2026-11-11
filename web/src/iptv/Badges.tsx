/* eslint-disable react-refresh/only-export-components -- label hooks/helpers live beside the components that use them */
// Flag + verdict + link-status badges. Every badge is icon AND text — status is
// never carried by colour alone (docs/web/accessibility.md §1.4.1).
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import {
  Ban,
  CircleAlert,
  CircleCheck,
  CircleX,
  Clock,
  FileQuestion,
  Globe,
  KeyRound,
  Link2,
  Lock,
  LockOpen,
  ShieldAlert,
  TriangleAlert,
  VideoOff,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';

import type { LinkStatus, ProbeVerdict, StreamFlag } from '../api/catalogue';
import { Badge } from '../components/ui/badge';

const FLAG_ICON: Record<StreamFlag, LucideIcon> = {
  geo_blocked: Globe,
  not_24_7: Clock,
  needs_headers: KeyRound,
  http_only: LockOpen,
  closed: CircleX,
  blocklisted: ShieldAlert,
  unsupported: Ban,
  no_metadata: FileQuestion,
  already_added: CircleCheck,
};

/** The localized text of a flag. */
export function useFlagLabel(): (flag: StreamFlag) => string {
  const { t } = useLingui();
  return (flag): string => {
    switch (flag) {
      case 'geo_blocked':
        return t`Geo-blocked`;
      case 'not_24_7':
        return t`Not 24/7`;
      case 'needs_headers':
        return t`Needs headers`;
      case 'http_only':
        return t`HTTP only`;
      case 'closed':
        return t`Closed`;
      case 'blocklisted':
        return t`Blocklisted`;
      case 'unsupported':
        return t`Unsupported`;
      case 'no_metadata':
        return t`No metadata`;
      case 'already_added':
        return t`Already added`;
    }
  };
}

/** One flag badge (icon + text). */
export function FlagBadge({ flag }: { readonly flag: StreamFlag }): JSX.Element {
  const label = useFlagLabel();
  const Icon = FLAG_ICON[flag];
  return (
    <Badge
      variant={flag === 'already_added' ? 'live' : flag === 'blocklisted' ? 'destructive' : 'outline'}
      data-flag={flag}
    >
      <Icon className="size-3" aria-hidden="true" />
      {label(flag)}
    </Badge>
  );
}

const VERDICT_ICON: Record<ProbeVerdict, LucideIcon> = {
  live: CircleCheck,
  degraded: TriangleAlert,
  geo_blocked_here: Globe,
  dead: CircleX,
  no_media: VideoOff,
  blocked: Ban,
};

/** The localized text of a probe verdict. */
export function useVerdictLabel(): (v: ProbeVerdict) => string {
  const { t } = useLingui();
  return (v): string => {
    switch (v) {
      case 'live':
        return t`Live`;
      case 'degraded':
        return t`Degraded`;
      case 'geo_blocked_here':
        return t`Geo-blocked from here`;
      case 'dead':
        return t`Dead`;
      case 'no_media':
        return t`No media`;
      case 'blocked':
        return t`Blocked by guard`;
    }
  };
}

/** A probe verdict badge (icon + text). */
export function VerdictBadge({ verdict }: { readonly verdict: ProbeVerdict }): JSX.Element {
  const label = useVerdictLabel();
  const Icon = VERDICT_ICON[verdict];
  return (
    <Badge
      variant={
        verdict === 'live'
          ? 'live'
          : verdict === 'degraded' || verdict === 'geo_blocked_here'
            ? 'stale'
            : 'nosignal'
      }
      data-verdict={verdict}
    >
      <Icon className="size-3" aria-hidden="true" />
      {label(verdict)}
    </Badge>
  );
}

/** The localized text of a link status. */
export function useLinkStatusLabel(): (s: LinkStatus) => string {
  const { t } = useLingui();
  return (s): string => {
    switch (s) {
      case 'in_sync':
        return t`In sync`;
      case 'updated':
        return t`Updated upstream`;
      case 'stream_removed':
        return t`Stream removed`;
      case 'channel_closed':
        return t`Channel closed`;
      case 'blocklisted':
        return t`Blocklisted`;
      case 'unlinked':
        return t`Unlinked`;
      case 'edited':
        return t`Edited locally`;
    }
  };
}

/** A link-status badge (icon + text). */
export function LinkStatusBadge({ status }: { readonly status: LinkStatus }): JSX.Element {
  const label = useLinkStatusLabel();
  const Icon =
    status === 'in_sync'
      ? Link2
      : status === 'updated' || status === 'edited'
        ? CircleAlert
        : status === 'unlinked'
          ? Lock
          : TriangleAlert;
  return (
    <Badge
      variant={status === 'in_sync' ? 'live' : status === 'unlinked' ? 'outline' : 'stale'}
      data-link-status={status}
    >
      <Icon className="size-3" aria-hidden="true" />
      {label(status)}
    </Badge>
  );
}

/** The data-source attributions (also listed in docs/io/iptv-org.md). */
export function DataSourcesFooter(): JSX.Element {
  return (
    <footer className="mt-8 border-t pt-4 text-xs text-muted-foreground" aria-labelledby="iptv-data-sources">
      <h2 id="iptv-data-sources" className="mb-1 font-medium text-foreground">
        <Trans>Data sources</Trans>
      </h2>
      <ul className="list-disc space-y-0.5 ps-5">
        <li>
          <Trans>
            <a className="underline" href="https://github.com/iptv-org/iptv" target="_blank" rel="noreferrer">
              iptv-org
            </a>{' '}
            channel and stream database — Unlicense. The streams belong to their broadcasters.
          </Trans>
        </li>
        <li>
          <Trans>
            <a className="underline" href="https://www.naturalearthdata.com/" target="_blank" rel="noreferrer">
              Natural Earth
            </a>{' '}
            country and state boundaries — public domain.
          </Trans>
        </li>
        <li>
          <Trans>
            <a className="underline" href="https://www.wikidata.org/" target="_blank" rel="noreferrer">
              Wikidata
            </a>{' '}
            city coordinates — CC0.
          </Trans>
        </li>
        <li>
          <Trans>
            Map rendering by{' '}
            <a className="underline" href="https://maplibre.org/" target="_blank" rel="noreferrer">
              MapLibre GL JS
            </a>{' '}
            (BSD-3-Clause). No map tiles are fetched from the internet.
          </Trans>
        </li>
      </ul>
    </footer>
  );
}
