/* eslint-disable react-refresh/only-export-components -- label hooks/helpers live beside the components that use them */
// Sources-page integration for linked sources: Link column cell, the
// read-only "Linked to IPTV-ORG" panel in the source dialog, and the link
// action dialogs (Review changes / Switch to alternative / Unlink / Probe).
// Every write goes through the ordinary If-Match source PUT (one write path).
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQueryClient } from '@tanstack/react-query';

import type { LinkAlternative, LinkInfo, LinkTarget, ProbeResult } from '../api/catalogue';
import { runProbe } from '../api/catalogueQueries';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { toast } from '../components/ui/use-toast';
import { getResource } from '../resources/api';
import { ORIGIN_FOLLOWS, parseSourceOrigin } from '../resources/forms';
import type { OriginFollow, SourceFormState } from '../resources/forms';
import { SelectField } from '../resources/FormControls';
import { resourceKeys, useSaveResource } from '../resources/queries';
import { LinkStatusBadge, VerdictBadge } from './Badges';
import { useFollowLabel } from './BulkAddDialog';
import { ProbeSummary } from './ProbeSummary';

/** The read-only provenance panel + follow selector in the source dialog. */
export function OriginPanel({
  form,
  setForm,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
}): JSX.Element | null {
  const { t } = useLingui();
  const followLabel = useFollowLabel();
  const origin = form.origin;
  if (origin === undefined) {
    return null;
  }
  return (
    <fieldset className="space-y-2 rounded-md border p-3" data-testid="origin-panel">
      <legend className="px-1 text-sm font-medium">
        <Trans>Linked to IPTV-ORG</Trans>
      </legend>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
        {origin.channel !== undefined ? (
          <>
            <dt className="text-muted-foreground"><Trans>Channel</Trans></dt>
            <dd>
              {origin.channel}
              {origin.feed !== undefined ? ` @ ${origin.feed}` : ''}
            </dd>
          </>
        ) : null}
        <dt className="text-muted-foreground"><Trans>Linked URL</Trans></dt>
        <dd className="break-all" dir="ltr">{origin.stream_url}</dd>
        {origin.labels.length > 0 ? (
          <>
            <dt className="text-muted-foreground"><Trans>Labels</Trans></dt>
            <dd>{origin.labels.join(', ')}</dd>
          </>
        ) : null}
        <dt className="text-muted-foreground"><Trans>Linked at</Trans></dt>
        <dd>{origin.linked_at}</dd>
      </dl>
      <SelectField<OriginFollow>
        label={t`Follow upstream changes`}
        value={origin.follow}
        options={ORIGIN_FOLLOWS}
        optionLabel={followLabel}
        onChange={(follow): void => {
          setForm({ ...form, origin: { ...origin, follow } });
        }}
      />
    </fieldset>
  );
}

/** Link column cell. */
export function LinkCell({ link }: { readonly link: LinkInfo | undefined }): JSX.Element {
  if (link === undefined) {
    return (
      <span className="text-sm text-muted-foreground">
        <span aria-hidden="true">—</span>
        <span className="sr-only">
          <Trans>Not linked to IPTV-ORG</Trans>
        </span>
      </span>
    );
  }
  return <LinkStatusBadge status={link.status} />;
}

function diffRows(
  current: LinkTarget,
  upstream: LinkTarget,
): { label: JSX.Element; before: string; after: string }[] {
  const rows: { label: JSX.Element; before: string; after: string }[] = [];
  const add = (label: JSX.Element, a: string, b: string): void => {
    if (a !== b) {
      rows.push({ label, before: a === '' ? '—' : a, after: b === '' ? '—' : b });
    }
  };
  add(<Trans>URL</Trans>, current.url, upstream.url);
  add(<Trans>User-Agent</Trans>, current.user_agent ?? '', upstream.user_agent ?? '');
  add(<Trans>Referer</Trans>, current.referrer ?? '', upstream.referrer ?? '');
  add(<Trans>Labels</Trans>, current.labels.join(', '), upstream.labels.join(', '));
  return rows;
}

type Action = 'review' | 'switch' | 'unlink' | 'probe';

/** Actions available for a link, by status. */
export function linkActions(link: LinkInfo): Action[] {
  const out: Action[] = [];
  if (link.status === 'updated') {
    out.push('review');
  }
  if (link.status === 'stream_removed' || link.status === 'channel_closed') {
    out.push('switch');
  }
  if (link.status !== 'unlinked') {
    out.push('unlink');
  }
  out.push('probe');
  return out;
}

/** Apply an edit to a stored source body via the ordinary If-Match PUT. */
async function mutateBody(
  id: string,
  edit: (body: Record<string, unknown>) => Record<string, unknown>,
  save: ReturnType<typeof useSaveResource>,
): Promise<void> {
  const current = await getResource('sources', id, {});
  const body = edit({ ...current.record.body });
  await save.mutateAsync({
    id,
    create: false,
    input: { name: current.record.name, body },
  });
}

/** Link action buttons + dialogs for one source row. */
export function LinkActions({
  sourceId,
  sourceName,
  locator,
  link,
}: {
  readonly sourceId: string;
  readonly sourceName: string;
  readonly locator: string | undefined;
  readonly link: LinkInfo | undefined;
}): JSX.Element | null {
  const { t } = useLingui();
  const save = useSaveResource('sources', {});
  const queryClient = useQueryClient();
  const [dialog, setDialog] = useState<Action | null>(null);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<ProbeResult | null>(null);
  const [probeError, setProbeError] = useState<string | null>(null);
  if (link === undefined) {
    return null;
  }
  const actions = linkActions(link);

  const done = (title: string): void => {
    toast({ title });
    void queryClient.invalidateQueries({ queryKey: ['catalogue', 'iptv-org', 'links'] });
    void queryClient.invalidateQueries({ queryKey: resourceKeys.list('sources') });
    setDialog(null);
  };
  const fail = (error: unknown): void => {
    toast({
      title: t`Could not update the source`,
      description: error instanceof Error ? error.message : String(error),
      variant: 'destructive',
    });
  };

  const applyTarget = async (target: LinkTarget, alt?: LinkAlternative): Promise<void> => {
    setBusy(true);
    try {
      await mutateBody(
        sourceId,
        (body) => {
          const origin = parseSourceOrigin(body.origin);
          const next: Record<string, unknown> = { ...body, url: target.url };
          if (target.user_agent !== null) {
            next.user_agent = target.user_agent;
          } else {
            delete next.user_agent;
          }
          if (target.referrer !== null) {
            next.referrer = target.referrer;
          } else {
            delete next.referrer;
          }
          if (origin !== undefined) {
            next.origin = {
              ...origin,
              ...(alt?.ch != null ? { channel: alt.ch } : {}),
              ...(alt?.feed != null ? { feed: alt.feed } : {}),
              stream_url: target.url,
              labels: [...target.labels],
            };
          }
          return next;
        },
        save,
      );
      done(alt !== undefined ? t`Switched to the alternative` : t`Upstream changes applied`);
    } catch (error) {
      fail(error);
    } finally {
      setBusy(false);
    }
  };

  const unlink = async (): Promise<void> => {
    setBusy(true);
    try {
      await mutateBody(
        sourceId,
        (body) => {
          const next = { ...body };
          delete next.origin;
          return next;
        },
        save,
      );
      done(t`Unlinked from IPTV-ORG`);
    } catch (error) {
      fail(error);
    } finally {
      setBusy(false);
    }
  };

  const probeNow = (): void => {
    setDialog('probe');
    setResult(null);
    setProbeError(null);
    runProbe({ source_ids: [sourceId] })
      .then((rs) => {
        setResult(rs[0] ?? null);
      })
      .catch((error: unknown) => {
        setProbeError(error instanceof Error ? error.message : String(error));
      });
  };

  const label = (a: Action): JSX.Element =>
    a === 'review' ? (
      <Trans>Review changes</Trans>
    ) : a === 'switch' ? (
      <Trans>Switch to alternative</Trans>
    ) : a === 'unlink' ? (
      <Trans>Unlink</Trans>
    ) : (
      <Trans>Probe</Trans>
    );
  const aria = (a: Action): string =>
    a === 'review'
      ? t`Review changes: ${sourceName}`
      : a === 'switch'
        ? t`Switch to alternative: ${sourceName}`
        : a === 'unlink'
          ? t`Unlink: ${sourceName}`
          : t`Probe: ${sourceName}`;

  const current: LinkTarget = {
    url: locator ?? '',
    user_agent: null,
    referrer: null,
    labels: [],
  };

  return (
    <>
      <div className="flex flex-wrap gap-1">
        {actions.map((a) => (
          <Button
            key={a}
            type="button"
            variant={a === 'review' || a === 'switch' ? 'default' : 'outline'}
            size="sm"
            aria-label={aria(a)}
            onClick={(): void => {
              if (a === 'probe') {
                probeNow();
              } else {
                setDialog(a);
              }
            }}
          >
            {label(a)}
          </Button>
        ))}
      </div>

      <Dialog
        open={dialog !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setDialog(null);
          }
        }}
      >
        <DialogContent>
          {dialog === 'review' && link.upstream !== null ? (
            <>
              <DialogHeader>
                <DialogTitle><Trans>Review upstream changes</Trans></DialogTitle>
                <DialogDescription>
                  <Trans>IPTV-ORG changed this channel’s stream. Apply to update the source.</Trans>
                </DialogDescription>
              </DialogHeader>
              <table className="w-full text-sm">
                <caption className="sr-only"><Trans>Changes</Trans></caption>
                <thead>
                  <tr className="text-start">
                    <th scope="col" className="text-start"><Trans>Field</Trans></th>
                    <th scope="col" className="text-start"><Trans>Current</Trans></th>
                    <th scope="col" className="text-start"><Trans>Upstream</Trans></th>
                  </tr>
                </thead>
                <tbody>
                  {diffRows(current, link.upstream).map((r, i) => (
                    <tr key={i} className="align-top">
                      <th scope="row" className="text-start font-medium">{r.label}</th>
                      <td className="break-all" dir="ltr">{r.before}</td>
                      <td className="break-all" dir="ltr">{r.after}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <DialogFooter>
                <Button type="button" variant="outline" onClick={(): void => { setDialog(null); }}>
                  <Trans>Cancel</Trans>
                </Button>
                <Button
                  type="button"
                  disabled={busy}
                  onClick={(): void => {
                    if (link.upstream !== null) {
                      void applyTarget(link.upstream);
                    }
                  }}
                >
                  <Trans>Apply</Trans>
                </Button>
              </DialogFooter>
            </>
          ) : null}
          {dialog === 'switch' ? (
            <>
              <DialogHeader>
                <DialogTitle><Trans>Switch to an alternative</Trans></DialogTitle>
                <DialogDescription>
                  <Trans>Ranked by probe verdict, quality and flags.</Trans>
                </DialogDescription>
              </DialogHeader>
              {link.alternatives.length === 0 ? (
                <p className="text-sm"><Trans>No alternatives are available.</Trans></p>
              ) : (
                <ul className="max-h-72 space-y-2 overflow-y-auto">
                  {link.alternatives.map((alt) => (
                    <li key={alt.k} className="flex items-start justify-between gap-2 border-b pb-2 text-sm">
                      <div className="min-w-0">
                        <p className="flex flex-wrap items-center gap-2">
                          <span>{alt.ch ?? alt.url}</span>
                          {alt.q !== null ? <span className="text-xs">{alt.q}</span> : null}
                          {alt.verdict !== null ? <VerdictBadge verdict={alt.verdict} /> : null}
                        </p>
                        <code className="block break-all text-xs text-muted-foreground" dir="ltr">{alt.url}</code>
                      </div>
                      <Button
                        type="button"
                        size="sm"
                        disabled={busy}
                        aria-label={t`Switch to ${alt.url}`}
                        onClick={(): void => {
                          void applyTarget(alt, alt);
                        }}
                      >
                        <Trans>Switch</Trans>
                      </Button>
                    </li>
                  ))}
                </ul>
              )}
              <DialogFooter>
                <Button type="button" variant="outline" onClick={(): void => { setDialog(null); }}>
                  <Trans>Close</Trans>
                </Button>
              </DialogFooter>
            </>
          ) : null}
          {dialog === 'unlink' ? (
            <>
              <DialogHeader>
                <DialogTitle><Trans>Unlink from IPTV-ORG?</Trans></DialogTitle>
                <DialogDescription>
                  <Trans>
                    The source keeps its URL and settings but stops following upstream changes.
                  </Trans>
                </DialogDescription>
              </DialogHeader>
              <DialogFooter>
                <Button type="button" variant="outline" onClick={(): void => { setDialog(null); }}>
                  <Trans>Cancel</Trans>
                </Button>
                <Button type="button" variant="destructive" disabled={busy} onClick={(): void => { void unlink(); }}>
                  <Trans>Unlink</Trans>
                </Button>
              </DialogFooter>
            </>
          ) : null}
          {dialog === 'probe' ? (
            <>
              <DialogHeader>
                <DialogTitle><Trans>Probe result</Trans></DialogTitle>
                <DialogDescription>
                  <Trans>Checked from this server.</Trans>
                </DialogDescription>
              </DialogHeader>
              {probeError !== null ? (
                <p role="alert" className="text-sm text-destructive">{probeError}</p>
              ) : result === null ? (
                <p role="status" className="text-sm"><Trans>Probing…</Trans></p>
              ) : (
                <ProbeSummary result={result} />
              )}
              <DialogFooter>
                <Button type="button" variant="outline" onClick={(): void => { setDialog(null); }}>
                  <Trans>Close</Trans>
                </Button>
              </DialogFooter>
            </>
          ) : null}
        </DialogContent>
      </Dialog>
    </>
  );
}
