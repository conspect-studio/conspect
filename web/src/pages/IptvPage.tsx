// IPTV playlist (epg-playlist): enable the server's own managed M3U playlist,
// choose and order what appears in it (HLS outputs — program, preview, aux —
// and directly-published sources), copy its URL, and watch the upstream
// XMLTV guide feeds passed through at /iptv/guide.xml.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { ArrowDown, ArrowUp, Copy, Plus, Trash2 } from 'lucide-react';

import {
  GUIDE_REFRESH_DEFAULT,
  GUIDE_REFRESH_MAX,
  GUIDE_REFRESH_MIN,
  moveEntry,
  normalizeIptv as normalize,
  useIptv,
  useSaveIptv,
} from '../api/iptv';
import type { IptvConfig, IptvEntry, IptvWithEtag, NormalizedConfig } from '../api/iptv';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '../components/ui/card';
import { Input } from '../components/ui/input';
import { toast } from '../components/ui/use-toast';
import { useOutputs, useSources } from '../resources/queries';

function formatTime(unix: number | null | undefined): string {
  return unix === undefined || unix === null ? '—' : new Date(unix * 1000).toLocaleString();
}

/** The IPTV playlist page. */
export function IptvPage(): JSX.Element {
  const iptv = useIptv();
  if (iptv.isPending) {
    return <p><Trans>Loading IPTV publication…</Trans></p>;
  }
  if (iptv.isError) {
    return <p role="alert"><Trans>Could not load the IPTV publication: {iptv.error.message}</Trans></p>;
  }
  // Re-key on the document ETag so a saved/remote change resets the draft.
  return <IptvEditor key={iptv.data.etag ?? 'none'} data={iptv.data} />;
}

function IptvEditor({ data }: { readonly data: IptvWithEtag }): JSX.Element {
  const { t } = useLingui();
  const iptv = useIptv();
  const save = useSaveIptv();
  const outputs = useOutputs();
  const sources = useSources();
  const [draft, setDraft] = useState<NormalizedConfig>(() => normalize(data.state.config));
  const [pick, setPick] = useState('');
  const state = (iptv.data ?? data).state;
  const origin = typeof window === 'undefined' ? '' : window.location.origin;
  const playlistUrl = `${draft.public_base_url ?? origin}${state.playlist_path}`;
  const guideUrl = `${draft.public_base_url ?? origin}${state.guide_path}`;
  const dirty = JSON.stringify(draft) !== JSON.stringify(normalize(data.state.config));

  const hlsOutputs = (outputs.data ?? []).filter((o) => o.kind === 'hls' || o.kind === 'll-hls');
  const candidates = [
    ...hlsOutputs.map((o) => ({ key: `output:${o.id}`, label: t`Output: ${o.name} (${o.id})` })),
    ...(sources.data ?? []).map((s) => ({ key: `source:${s.id}`, label: t`Source: ${s.name} (${s.id})` })),
  ].filter((c) => !draft.entries.some((e) => `${e.kind}:${e.ref}` === c.key));

  const setEntries = (entries: readonly IptvEntry[]): void => {
    setDraft({ ...draft, entries });
  };
  const updateEntry = (index: number, patch: Partial<IptvEntry>): void => {
    setEntries(draft.entries.map((e, i) => (i === index ? { ...e, ...patch } : e)));
  };
  const add = (): void => {
    const [kind, ...rest] = pick.split(':');
    const ref = rest.join(':');
    if ((kind !== 'output' && kind !== 'source') || ref === '') return;
    setEntries([...draft.entries, { kind, ref, allow_direct_source_url: kind === 'source' }]);
    setPick('');
  };
  const refresh = draft.guide_refresh_minutes;
  const refreshInvalid =
    refresh !== null &&
    (!Number.isInteger(refresh) || refresh < GUIDE_REFRESH_MIN || refresh > GUIDE_REFRESH_MAX);
  const submit = (): void => {
    if (refreshInvalid) return;
    const { guide_refresh_minutes: refreshMinutes, ...rest } = draft;
    const clean: IptvConfig = {
      ...rest,
      ...(refreshMinutes === null ? {} : { guide_refresh_minutes: refreshMinutes }),
      entries: draft.entries.map((e) => {
        const entry: IptvEntry = { kind: e.kind, ref: e.ref, allow_direct_source_url: e.allow_direct_source_url ?? false };
        for (const key of ['tvg_id', 'name', 'logo', 'group'] as const) {
          const value = e[key]?.trim();
          if (value) entry[key] = value;
        }
        return entry;
      }),
    };
    save.mutate(
      { config: clean, etag: data.etag },
      {
        onSuccess: () => {
          toast({ title: t`IPTV publication saved` });
        },
        onError: (error) => {
          toast({ title: t`Could not save`, description: error.detail ?? error.message, variant: 'destructive' });
        },
      },
    );
  };
  const copy = (url: string): void => {
    void navigator.clipboard.writeText(url).then(() => toast({ title: t`Copied ${url}` }));
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title={<Trans>IPTV playlist</Trans>}
        description={
          <Trans>
            Publish this server's own M3U playlist with an XMLTV guide for IPTV players. The playlist
            and guide are public files (like HLS output files) — publish only what viewers may see.
          </Trans>
        }
        actions={
          <Button disabled={!dirty || save.isPending || refreshInvalid} onClick={submit}>
            <Trans>Save</Trans>
          </Button>
        }
      />
      <Card>
        <CardHeader>
          <CardTitle><Trans>Publication</Trans></CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={draft.enabled}
              onChange={(event): void => {
                setDraft({ ...draft, enabled: event.target.checked });
              }}
            />
            <Trans>Serve the playlist and guide</Trans>
          </label>
          <div className="space-y-1">
            <label htmlFor="iptv-base" className="text-sm font-medium"><Trans>Public base URL (optional)</Trans></label>
            <Input
              id="iptv-base"
              value={draft.public_base_url ?? ''}
              placeholder={origin}
              onChange={(event): void => {
                const value = event.target.value.trim();
                setDraft({ ...draft, public_base_url: value === '' ? null : value });
              }}
            />
          </div>
          <div className="space-y-1">
            <label htmlFor="iptv-guide-refresh" className="text-sm font-medium">
              <Trans>Guide refresh (minutes)</Trans>
            </label>
            <Input
              id="iptv-guide-refresh"
              type="number"
              inputMode="numeric"
              min={GUIDE_REFRESH_MIN}
              max={GUIDE_REFRESH_MAX}
              value={refresh ?? ''}
              placeholder={String(GUIDE_REFRESH_DEFAULT)}
              aria-invalid={refreshInvalid}
              aria-describedby="iptv-guide-refresh-hint"
              onChange={(event): void => {
                const value = event.target.value.trim();
                setDraft({ ...draft, guide_refresh_minutes: value === '' ? null : Number(value) });
              }}
            />
            <p
              id="iptv-guide-refresh-hint"
              className={refreshInvalid ? 'text-xs text-destructive' : 'text-xs text-muted-foreground'}
            >
              <Trans>
                How often guide feeds are fetched in the background while publication is enabled.
                Blank means {GUIDE_REFRESH_DEFAULT}. Allowed: {GUIDE_REFRESH_MIN} to {GUIDE_REFRESH_MAX}.
              </Trans>
            </p>
          </div>
          {[{ label: t`Playlist URL`, url: playlistUrl }, { label: t`Guide URL`, url: guideUrl }].map((row) => (
            <div key={row.label} className="flex flex-wrap items-center gap-2 text-sm">
              <span className="font-medium">{row.label}:</span>
              <code className="break-all">{row.url}</code>
              <Button variant="outline" size="sm" aria-label={t`Copy ${row.label}`} onClick={(): void => { copy(row.url); }}>
                <Copy aria-hidden="true" />
              </Button>
            </div>
          ))}
          {!(state.config.enabled ?? false) ? (
            <p className="text-xs text-muted-foreground"><Trans>Disabled: both URLs answer 404 and no guide is fetched.</Trans></p>
          ) : null}
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle><Trans>Entries</Trans></CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <select
              aria-label={t`Add an output or source`}
              className="h-9 rounded-md border border-input bg-background px-2 text-sm"
              value={pick}
              onChange={(event): void => {
                setPick(event.target.value);
              }}
            >
              <option value="">{t`Choose an output or source…`}</option>
              {candidates.map((c) => (
                <option key={c.key} value={c.key}>{c.label}</option>
              ))}
            </select>
            <Button size="sm" variant="outline" disabled={pick === ''} onClick={add}>
              <Plus aria-hidden="true" />
              <Trans>Add</Trans>
            </Button>
          </div>
          {draft.entries.length === 0 ? (
            <p className="text-sm text-muted-foreground"><Trans>Nothing is published yet.</Trans></p>
          ) : null}
          <ol className="space-y-3">
            {draft.entries.map((entry, index) => {
              const resolved = state.entries.find((r) => r.kind === entry.kind && r.ref === entry.ref);
              return (
                <li key={`${entry.kind}:${entry.ref}`} className="space-y-2 rounded-md border p-3">
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge variant="secondary">{entry.kind === 'output' ? t`Output` : t`Source`}</Badge>
                    <span className="font-medium">{entry.ref}</span>
                    {resolved?.unavailable ? <Badge variant="destructive">{resolved.unavailable}</Badge> : null}
                    {resolved?.epg_feed ? <Badge variant="outline"><Trans>EPG: {resolved.epg_feed}</Trans></Badge> : null}
                    <span className="ml-auto flex gap-1">
                      <Button size="icon" variant="ghost" aria-label={t`Move up`} disabled={index === 0} onClick={(): void => { setEntries(moveEntry(draft.entries, index, -1)); }}>
                        <ArrowUp aria-hidden="true" />
                      </Button>
                      <Button size="icon" variant="ghost" aria-label={t`Move down`} disabled={index === draft.entries.length - 1} onClick={(): void => { setEntries(moveEntry(draft.entries, index, 1)); }}>
                        <ArrowDown aria-hidden="true" />
                      </Button>
                      <Button size="icon" variant="ghost" aria-label={t`Remove ${entry.ref}`} onClick={(): void => { setEntries(draft.entries.filter((_, i) => i !== index)); }}>
                        <Trash2 aria-hidden="true" />
                      </Button>
                    </span>
                  </div>
                  <div className="grid gap-2 sm:grid-cols-4">
                    {(['name', 'tvg_id', 'group', 'logo'] as const).map((field) => (
                      <Input
                        key={field}
                        aria-label={`${entry.ref} ${field}`}
                        placeholder={field === 'name' ? t`Name` : field === 'tvg_id' ? t`tvg-id` : field === 'group' ? t`Group` : t`Logo URL`}
                        value={entry[field] ?? ''}
                        onChange={(event): void => {
                          updateEntry(index, { [field]: event.target.value });
                        }}
                      />
                    ))}
                  </div>
                  {entry.kind === 'source' ? (
                    <label className="flex items-center gap-2 text-xs">
                      <input
                        type="checkbox"
                        checked={entry.allow_direct_source_url ?? false}
                        onChange={(event): void => {
                          updateEntry(index, { allow_direct_source_url: event.target.checked });
                        }}
                      />
                      <Trans>Publish this source's upstream URL directly (credentials are always removed)</Trans>
                    </label>
                  ) : null}
                </li>
              );
            })}
          </ol>
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle><Trans>Guide feeds</Trans></CardTitle>
        </CardHeader>
        <CardContent>
          <p className="mb-2 text-xs text-muted-foreground">
            <Trans>
              Upstream XMLTV feeds of published sources with an EPG set, refreshed in the background
              every {state.config.guide_refresh_minutes ?? GUIDE_REFRESH_DEFAULT} minutes while
              publication is enabled. Guide last assembled: {formatTime(state.guide_generated_at)}
            </Trans>
          </p>
          {state.feeds.length === 0 ? (
            <p className="text-sm text-muted-foreground"><Trans>No published source has an upstream EPG.</Trans></p>
          ) : (
            <ul className="space-y-2">
              {state.feeds.map((feed) => (
                <li key={feed.url} className="text-sm">
                  <code className="break-all">{feed.url}</code>
                  <div className="text-xs text-muted-foreground">
                    <Trans>
                      Last success {formatTime(feed.last_success)} · next {formatTime(feed.next_attempt)} ·{' '}
                      {String(feed.channels)} channel(s), {String(feed.programmes)} programme(s)
                    </Trans>
                  </div>
                  {feed.error ? <p role="status" className="text-xs text-destructive">{feed.error}</p> : null}
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
