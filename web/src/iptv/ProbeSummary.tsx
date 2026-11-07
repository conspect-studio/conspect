// Renders one probe result: verdict, provenance, media facts and timed events.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import type { ProbeResult } from '../api/catalogue';
import { VerdictBadge } from './Badges';
import { NetworkButton } from './NetworkDialog';

/** The full probe result (verdict + facts + events). */
export function ProbeSummary({ result }: { readonly result: ProbeResult }): JSX.Element {
  const { t } = useLingui();
  const f = result.facts;
  return (
    <div className="space-y-2 text-sm" data-testid="probe-summary">
      <p className="flex flex-wrap items-center gap-2">
        <VerdictBadge verdict={result.verdict} />
        <span className="text-xs text-muted-foreground">
          <Trans>
            probed from {result.probed_from} at{' '}
            <time dateTime={result.probed_at}>{result.probed_at}</time>
          </Trans>
        </span>
        {result.network != null || result.source_id !== null ? (
          <NetworkButton
            label={t`Network path of this probe`}
            report={result.network}
            sourceId={result.source_id}
          />
        ) : null}
      </p>
      {result.detail !== null ? <p className="text-xs">{result.detail}</p> : null}
      {f !== null ? (
        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
          {f.width !== null && f.height !== null ? (
            <>
              <dt className="text-muted-foreground"><Trans>Resolution</Trans></dt>
              <dd>
                {f.width}×{f.height}
                {f.fps !== null ? ` @ ${String(f.fps)} fps` : ''}
              </dd>
            </>
          ) : null}
          {f.video_codec !== null || f.audio_codec !== null ? (
            <>
              <dt className="text-muted-foreground"><Trans>Codecs</Trans></dt>
              <dd>{[f.video_codec, f.audio_codec].filter((x) => x !== null).join(' / ')}</dd>
            </>
          ) : null}
          {f.bitrate_kbps !== null ? (
            <>
              <dt className="text-muted-foreground"><Trans>Bitrate</Trans></dt>
              <dd>
                {f.bitrate_kbps} kb/s
                {f.advertised_kbps !== null ? ` (${String(f.advertised_kbps)} advertised)` : ''}
              </dd>
            </>
          ) : null}
          {f.audio_languages.length > 0 ? (
            <>
              <dt className="text-muted-foreground"><Trans>Audio languages</Trans></dt>
              <dd>{f.audio_languages.join(', ')}</dd>
            </>
          ) : null}
          {f.captions.length > 0 ? (
            <>
              <dt className="text-muted-foreground"><Trans>Captions</Trans></dt>
              <dd>{f.captions.join(', ')}</dd>
            </>
          ) : null}
        </dl>
      ) : null}
      {f?.thumbnail != null ? (
        <img src={f.thumbnail} alt="" width={160} className="rounded border" />
      ) : null}
      {result.events.length > 0 ? (
        <div>
          <h4 className="text-xs font-medium"><Trans>Events seen</Trans></h4>
          <ul className="list-disc ps-5 text-xs">
            {result.events.map((e, i) => (
              <li key={`${e.kind}-${String(i)}`}>
                <code>{e.kind}</code>
                {e.at !== null ? ` · ${e.at}` : ''}
                {e.detail !== null ? ` — ${e.detail}` : ''}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}
