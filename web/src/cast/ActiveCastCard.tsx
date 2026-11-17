// The browser cast running from this tab: device, what it shows, the real
// receiver state, resolution/latency, volume + mute, Stop, and the pickers
// for what the TV shows: Video (another cast output, decision 2), Audio and
// Subtitles (the loaded media's tracks, decision 7), plus the existing
// "Show something else" (a take on the cast output: the TV keeps its URL).
//
// `switchOutput`/`selectAudio`/`selectText` never throw — they report a
// failure on the snapshot (`errorText`/`trackError`) instead, so these
// handlers below are fire-and-forget.
import { useId, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Square, Volume2, VolumeX } from 'lucide-react';

import type { CastOutput } from '../api/castStreams';
import { castRefusalSentence } from '../api/castStreams';
import { feedOf, takeOutputFeed } from '../api/outputRouting';
import type { OutputVideoFeed } from '../api/outputRouting';
import { Button } from '../components/ui/button';
import { Label } from '../components/ui/label';
import { CastFeedPicker } from './CastFeedPicker';
import type { FeedOption } from './CastFeedPicker';
import { useCastController, useCastSnapshot } from './CastProvider';
import { castOutputLabel, castQualityLine, castStatusLabel } from './castLabels';

const OFF = '__off__';

function outputFeed(output: CastOutput): OutputVideoFeed | undefined {
  return output.feed === null || output.feed === undefined ? undefined : feedOf(output.feed);
}

export function ActiveCastCard({
  options,
  outputs = [],
}: {
  readonly options: readonly FeedOption[];
  /** Every cast output this principal may use (`GET /cast/config` `outputs`); the Video picker needs more than one. */
  readonly outputs?: readonly CastOutput[];
}): JSX.Element | null {
  const { t } = useLingui();
  const videoId = useId();
  const audioId = useId();
  const textId = useId();
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
  const busy = cast.status === 'stopping' || cast.status === 'preparing' || cast.status === 'connecting' || cast.status === 'loading';

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

  const changeVideo = (newOutput: string): void => {
    if (newOutput === output) return;
    const target = outputs.find((o) => o.id === newOutput);
    void controller.switchOutput(newOutput, castOutputLabel(newOutput, target === undefined ? undefined : outputFeed(target)));
  };

  const changeAudio = (value: string): void => {
    const trackId = Number.parseInt(value, 10);
    if (Number.isFinite(trackId)) void controller.selectAudio(trackId);
  };

  const changeText = (value: string): void => {
    const trackId = value === OFF ? null : Number.parseInt(value, 10);
    void controller.selectText(trackId === null || Number.isFinite(trackId) ? trackId : null);
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
      <div className="grid max-w-md gap-3 sm:grid-cols-3">
        {outputs.length > 1 && output !== null ? (
          <div className="grid gap-1.5">
            <Label htmlFor={videoId}>
              <Trans>Video</Trans>
            </Label>
            <select
              id={videoId}
              className="h-11 rounded-md border bg-background px-3 text-sm"
              value={output}
              disabled={busy}
              onChange={(e): void => {
                changeVideo(e.target.value);
              }}
            >
              {outputs.map((o) => (
                <option key={o.id} value={o.id}>
                  {castOutputLabel(o.id, outputFeed(o))}
                </option>
              ))}
            </select>
          </div>
        ) : null}
        {cast.audioTracks.length > 1 ? (
          <div className="grid gap-1.5">
            <Label htmlFor={audioId}>
              <Trans>Audio</Trans>
            </Label>
            <select
              id={audioId}
              className="h-11 rounded-md border bg-background px-3 text-sm"
              value={cast.activeAudioId ?? ''}
              disabled={busy}
              onChange={(e): void => {
                changeAudio(e.target.value);
              }}
            >
              {cast.audioTracks.map((track) => (
                <option key={track.id} value={track.id}>
                  {track.name}
                </option>
              ))}
            </select>
          </div>
        ) : null}
        {cast.textTracks.length >= 1 ? (
          <div className="grid gap-1.5">
            <Label htmlFor={textId}>
              <Trans>Subtitles</Trans>
            </Label>
            <select
              id={textId}
              className="h-11 rounded-md border bg-background px-3 text-sm"
              value={cast.activeTextId ?? OFF}
              disabled={busy}
              onChange={(e): void => {
                changeText(e.target.value);
              }}
            >
              <option value={OFF}>{t`Off`}</option>
              {cast.textTracks.map((track) => (
                <option key={track.id} value={track.id}>
                  {track.name}
                </option>
              ))}
            </select>
          </div>
        ) : null}
      </div>
      {cast.trackError !== null ? <p role="alert" className="text-sm text-destructive">{cast.trackError}</p> : null}
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
