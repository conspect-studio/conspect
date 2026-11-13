// The browser cast running from this tab: device, what it shows, the real
// receiver state, resolution/latency, volume + mute, Stop, and "Show something
// else" (a take on the cast output: the TV keeps its URL).
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Square, Volume2, VolumeX } from 'lucide-react';

import { castRefusalSentence } from '../api/castStreams';
import { takeOutputFeed } from '../api/outputRouting';
import type { OutputVideoFeed } from '../api/outputRouting';
import { Button } from '../components/ui/button';
import { CastFeedPicker } from './CastFeedPicker';
import type { FeedOption } from './CastFeedPicker';
import { useCastController, useCastSnapshot } from './CastProvider';
import { castQualityLine, castStatusLabel } from './castLabels';

export function ActiveCastCard({ options }: { readonly options: readonly FeedOption[] }): JSX.Element | null {
  const { t } = useLingui();
  const controller = useCastController();
  const cast = useCastSnapshot();
  const [showing, setShowing] = useState<{ key: string; label: string } | null>(null);
  const [takeState, setTakeState] = useState<{ pending: boolean; error: string | null }>({ pending: false, error: null });
  if (cast.status === 'idle' || (cast.status === 'error' && cast.streamId === null)) return null;
  const quality = castQualityLine(cast);
  const volumePercent = Math.round((cast.volume ?? 0) * 100);
  const output = cast.output;
  const host = window.location.host;
  const shows = showing?.label ?? cast.targetName ?? t`Unknown feed`;

  const change = (key: string, feed: OutputVideoFeed | undefined, label: string): void => {
    if (output === null || feed === undefined) return;
    setTakeState({ pending: true, error: null });
    // takeOutputFeed sends a fresh Idempotency-Key per call.
    takeOutputFeed(output, feed)
      .then(() => {
        setShowing({ key, label });
        setTakeState({ pending: false, error: null });
      })
      .catch((error: unknown) => {
        setTakeState({ pending: false, error: castRefusalSentence(error) });
      });
  };

  return (
    <section aria-labelledby="cast-active" className="grid gap-3 rounded-md border p-4">
      <h3 id="cast-active" className="text-sm font-semibold">
        <Trans>Casting from this browser</Trans>
      </h3>
      <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-sm">
        <dt className="text-muted-foreground"><Trans>Device</Trans></dt>
        <dd lang="" dir="auto">{cast.deviceName ?? t`Choosing a device…`}</dd>
        <dt className="text-muted-foreground"><Trans>Shows</Trans></dt>
        <dd lang="" dir="auto">{shows}</dd>
        <dt className="text-muted-foreground"><Trans>State</Trans></dt>
        <dd role="status" aria-live="polite">
          {castStatusLabel(cast.status)}
          {quality !== null ? ` · ${quality}` : null}
        </dd>
      </dl>
      {cast.errorText !== null ? (
        <div role="alert" className="text-sm text-destructive">
          <p>{cast.errorText}</p>
          {cast.errorDetail !== null ? <p className="text-xs"><code>{cast.errorDetail}</code></p> : null}
        </div>
      ) : null}
      {cast.tvUnreachable ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>
            The TV has not reached this server. Check that the TV can open https://{host} (certificate, DNS,
            firewall).
          </Trans>
        </p>
      ) : null}
      <div className="flex flex-wrap items-center gap-3">
        <Button
          variant="outline"
          size="icon"
          aria-pressed={cast.muted === true}
          aria-label={cast.muted === true ? t`Unmute the TV` : t`Mute the TV`}
          title={cast.muted === true ? t`Unmute the TV` : t`Mute the TV`}
          onClick={(): void => {
            void controller.setMuted(cast.muted !== true);
          }}
        >
          {cast.muted === true ? <VolumeX aria-hidden="true" /> : <Volume2 aria-hidden="true" />}
        </Button>
        <label className="flex items-center gap-2 text-sm">
          <span><Trans>TV volume</Trans></span>
          <input
            type="range"
            min={0}
            max={100}
            step={1}
            value={volumePercent}
            aria-valuetext={`${String(volumePercent)}%`}
            className="h-11 w-40"
            onChange={(e): void => {
              void controller.setVolume(Number(e.target.value) / 100);
            }}
          />
          <span aria-hidden="true">{volumePercent}%</span>
        </label>
        <Button
          variant="destructive"
          disabled={cast.status === 'stopping'}
          onClick={(): void => {
            void controller.stop();
          }}
        >
          <Square aria-hidden="true" />
          <Trans>Stop casting</Trans>
        </Button>
      </div>
      {output !== null ? (
        <div className="grid max-w-md gap-1">
          <CastFeedPicker
            options={options}
            value={showing?.key ?? cast.targetKey?.replace(/^feed:/, '') ?? 'bus:program'}
            label={t`Show something else`}
            disabled={takeState.pending || cast.status === 'stopping'}
            onChange={change}
          />
          {takeState.error !== null ? <p role="alert" className="text-sm text-destructive">{takeState.error}</p> : null}
        </div>
      ) : null}
    </section>
  );
}
