// "Cast from this browser": this Chrome/Edge is the Cast sender; the TV
// fetches a token URL (`/cast/<token>/master.m3u8`) the engine serves from a
// cast output. Shows a feed picker + Cast, the running cast's card, and the
// states that block casting (unsupported browser, no cast output).
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import { useCastConfig } from '../api/castStreamsQueries';
import { feedFromKey } from '../api/outputRouting';
import { ActiveCastCard } from './ActiveCastCard';
import { CastButton } from './CastButton';
import { CastFeedPicker, useFeedOptions } from './CastFeedPicker';
import { useCastReceiver } from './CastProvider';
import type { CastTarget } from './castController';

const CONFIG_SNIPPET = `[[outputs]]
kind = "cast"
id = "cast-1"`;

export function BrowserCastSection(): JSX.Element {
  const { t } = useLingui();
  const receiver = useCastReceiver();
  const config = useCastConfig(5_000, receiver.supported);
  const options = useFeedOptions();
  const [selected, setSelected] = useState('bus:program');
  const option = options.find((o) => o.key === selected) ?? options[0];
  const feed = option === undefined ? undefined : feedFromKey(option.key);
  const target: CastTarget | null =
    option === undefined || feed === undefined ? null : { key: `feed:${option.key}`, name: option.label, feed };
  const insecure = window.location.protocol !== 'https:';

  return (
    <section aria-labelledby="cast-browser" className="mb-8 grid gap-3">
      <h2 id="cast-browser" className="text-sm font-semibold">
        <Trans>Cast from this browser</Trans>
      </h2>
      <p className="text-sm text-muted-foreground">
        <Trans>
          This browser is the Cast sender; the TV fetches the stream straight from this server over HTTPS. Works
          wherever the engine runs, as long as the TV can reach this address.
        </Trans>
      </p>
      {!receiver.supported ? (
        <p role="status" className="text-sm">
          <Trans>Cast needs Chrome or Edge on a computer or Android.</Trans>
        </p>
      ) : config.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading the cast settings…</Trans>
        </p>
      ) : config.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load the cast settings:</Trans> {config.error.message}
        </p>
      ) : config.data.outputs.length === 0 ? (
        <div className="grid gap-2 rounded-md border border-dashed p-4 text-sm">
          <p>
            <Trans>
              No cast output is configured. Browser casting plays from a cast output: add one to the engine
              configuration and restart the engine.
            </Trans>
          </p>
          <pre className="w-fit rounded bg-muted p-2 text-xs" aria-label={t`Configuration snippet`}>
            <code>{CONFIG_SNIPPET}</code>
          </pre>
        </div>
      ) : (
        <>
          {insecure ? (
            <p role="alert" className="text-sm text-destructive">
              <Trans>
                This page is not served over HTTPS. A TV only plays an HTTPS address with a valid certificate.
              </Trans>
            </p>
          ) : null}
          {receiver.appId === null ? (
            <p role="note" className="text-sm text-muted-foreground">
              <Trans>
                Using Google&apos;s default receiver. Register the Conspect receiver for diagnostics and the branded
                screen.
              </Trans>
            </p>
          ) : null}
          <div className="flex flex-wrap items-end gap-3">
            <div className="min-w-64">
              <CastFeedPicker
                options={options}
                value={option?.key ?? 'bus:program'}
                label={t`What to cast`}
                onChange={(key): void => {
                  setSelected(key);
                }}
              />
            </div>
            {target !== null ? <CastButton target={target} /> : null}
          </div>
          <ActiveCastCard options={options} />
        </>
      )}
      <p className="text-xs text-muted-foreground">
        <Trans>
          Google Cast and Chromecast are trademarks of Google LLC. This project is independent and not affiliated
          with or endorsed by Google.
        </Trans>
      </p>
    </section>
  );
}
