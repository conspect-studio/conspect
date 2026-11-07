// "Import from M3U" (epg-playlist): fetch or paste an extended-M3U catalogue,
// pick candidates, and create them as HLS sources through the ordinary source
// create API (each with upstream EPG + IPTV presentation metadata when the
// catalogue declares them).
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQueryClient } from '@tanstack/react-query';
import { ListPlus } from 'lucide-react';

import { candidateToSourceBody, importM3u, uniqueId } from '../api/iptv';
import type { M3uCatalogue } from '../api/iptv';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import { toast } from '../components/ui/use-toast';
import { writeResource } from '../resources/api';
import { resourceKeys, useSources } from '../resources/queries';

/** The Sources-page "Import from M3U" button + dialog. */
export function M3uImportButton(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const sources = useSources();
  const [open, setOpen] = useState(false);
  const [url, setUrl] = useState('');
  const [text, setText] = useState('');
  const [allowPrivate, setAllowPrivate] = useState(false);
  const [catalogue, setCatalogue] = useState<M3uCatalogue | undefined>(undefined);
  const [selected, setSelected] = useState<ReadonlySet<number>>(new Set());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | undefined>(undefined);

  const reset = (): void => {
    setCatalogue(undefined);
    setSelected(new Set());
    setError(undefined);
  };

  const load = async (): Promise<void> => {
    setBusy(true);
    setError(undefined);
    try {
      const request = url.trim() !== ''
        ? { url: url.trim(), allow_private_network: allowPrivate }
        : { text };
      const result = await importM3u(request);
      setCatalogue(result);
      setSelected(new Set());
    } catch (cause) {
      const err = cause instanceof Error ? cause : new Error(String(cause));
      const detail = 'detail' in err && typeof err.detail === 'string' ? err.detail : undefined;
      setError(detail ?? err.message);
      setCatalogue(undefined);
    } finally {
      setBusy(false);
    }
  };

  const create = async (): Promise<void> => {
    if (catalogue === undefined) return;
    setBusy(true);
    const taken = new Set((sources.data ?? []).map((source) => source.id));
    const urlTvg = catalogue.url_tvg[0];
    let created = 0;
    const failures: string[] = [];
    for (const index of [...selected].sort((a, b) => a - b)) {
      const entry = catalogue.entries[index];
      if (entry === undefined) continue;
      const id = uniqueId(entry.name, taken);
      try {
        await writeResource(
          'sources',
          id,
          { name: entry.name, body: candidateToSourceBody(entry, id, urlTvg) },
          true,
        );
        created += 1;
      } catch (cause) {
        failures.push(`${entry.name}: ${cause instanceof Error ? cause.message : String(cause)}`);
      }
    }
    setBusy(false);
    void queryClient.invalidateQueries({ queryKey: resourceKeys.list('sources') });
    if (failures.length === 0) {
      toast({ title: t`Imported ${String(created)} source(s)` });
      setOpen(false);
      reset();
    } else {
      setError(failures.join('\n'));
      toast({ title: t`Some sources could not be created`, variant: 'destructive' });
    }
  };

  const toggle = (index: number): void => {
    const next = new Set(selected);
    if (next.has(index)) next.delete(index);
    else next.add(index);
    setSelected(next);
  };

  const allSelected = catalogue !== undefined && catalogue.entries.length > 0 && selected.size === catalogue.entries.length;

  return (
    <>
      <Button
        variant="outline"
        size="sm"
        onClick={(): void => {
          setOpen(true);
        }}
      >
        <ListPlus aria-hidden="true" />
        <Trans>Import from M3U</Trans>
      </Button>
      <Dialog
        open={open}
        onOpenChange={(next): void => {
          setOpen(next);
          if (!next) reset();
        }}
      >
        <DialogContent className="max-h-[90vh] max-w-3xl overflow-y-auto">
          <DialogHeader>
            <DialogTitle>
              <Trans>Import sources from an M3U playlist</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Fetch an extended-M3U channel list by URL or paste its text, then pick the channels
                to add as HLS sources. A guide URL in the playlist header (url-tvg) is attached as
                each source's upstream EPG.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <div className="space-y-1">
              <Label htmlFor="m3u-url">
                <Trans>Playlist URL</Trans>
              </Label>
              <Input
                id="m3u-url"
                type="url"
                value={url}
                placeholder="https://example.com/channels.m3u8"
                onChange={(event): void => {
                  setUrl(event.target.value);
                }}
              />
              <label className="flex items-center gap-2 text-xs text-muted-foreground">
                <input
                  type="checkbox"
                  checked={allowPrivate}
                  onChange={(event): void => {
                    setAllowPrivate(event.target.checked);
                  }}
                />
                <Trans>Allow a private/LAN address</Trans>
              </label>
            </div>
            <div className="space-y-1">
              <Label htmlFor="m3u-text">
                <Trans>…or paste playlist text</Trans>
              </Label>
              <textarea
                id="m3u-text"
                className="h-28 w-full rounded-md border border-input bg-background p-2 font-mono text-xs"
                value={text}
                disabled={url.trim() !== ''}
                onChange={(event): void => {
                  setText(event.target.value);
                }}
              />
            </div>
            <Button
              size="sm"
              disabled={busy || (url.trim() === '' && text.trim() === '')}
              onClick={(): void => {
                void load();
              }}
            >
              <Trans>Load candidates</Trans>
            </Button>
            {error !== undefined ? (
              <p role="alert" className="whitespace-pre-wrap text-sm text-destructive">
                {error}
              </p>
            ) : null}
            {catalogue !== undefined ? (
              <div className="space-y-2">
                <p className="text-sm text-muted-foreground">
                  {catalogue.url_tvg.length > 0 ? (
                    <Trans>Guide (url-tvg): {catalogue.url_tvg.join(', ')}</Trans>
                  ) : (
                    <Trans>This playlist declares no guide URL.</Trans>
                  )}
                </p>
                <label className="flex items-center gap-2 text-sm">
                  <input
                    type="checkbox"
                    checked={allSelected}
                    onChange={(event): void => {
                      setSelected(event.target.checked ? new Set(catalogue.entries.map((_, i) => i)) : new Set());
                    }}
                  />
                  <Trans>Select all ({String(catalogue.entries.length)})</Trans>
                </label>
                <ul className="max-h-72 space-y-1 overflow-y-auto rounded-md border p-2" aria-label={t`Candidates`}>
                  {catalogue.entries.map((entry, index) => (
                    <li key={`${String(index)}-${entry.url}`}>
                      <label className="flex items-start gap-2 text-sm">
                        <input
                          type="checkbox"
                          checked={selected.has(index)}
                          onChange={(): void => {
                            toggle(index);
                          }}
                        />
                        <span>
                          <span className="font-medium">{entry.name}</span>
                          {entry.group ? <span className="text-muted-foreground"> · {entry.group}</span> : null}
                          {entry.tvg_id ? <span className="text-muted-foreground"> · {entry.tvg_id}</span> : null}
                          <br />
                          <code className="break-all text-xs text-muted-foreground">{entry.url}</code>
                        </span>
                      </label>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </div>
          <DialogFooter>
            <Button
              disabled={busy || selected.size === 0}
              onClick={(): void => {
                void create();
              }}
            >
              <Trans>Create {String(selected.size)} source(s)</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
