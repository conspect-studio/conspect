/* eslint-disable react-refresh/only-export-components -- label hooks/helpers live beside the components that use them */
// Bulk add: one confirmed step that creates several linked sources and reports a
// result per item (partial failure never aborts the rest). Uses the SAME
// form→body mapping as the single "Add as source" path.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { CircleCheck, CircleX } from 'lucide-react';

import { useQueryClient } from '@tanstack/react-query';

import { bulkAddSources } from '../api/catalogue';
import type { BulkAddEntry, IndexStream } from '../api/catalogue';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { ORIGIN_FOLLOWS, validateSourceForm } from '../resources/forms';
import type { OriginFollow, SourceActivationChoice } from '../resources/forms';
import { SelectField } from '../resources/FormControls';
import { resourceKeys } from '../resources/queries';
import { ActivationChoice } from './ActivationChoice';
import { catalogueSourceBody, toSourceForm } from './toSource';

interface ItemResult {
  readonly key: string;
  readonly title: string;
  readonly id: string;
  readonly ok: boolean;
  readonly message: string;
}

/** Follow-mode label. */
export function useFollowLabel(): (f: OriginFollow) => string {
  const { t } = useLingui();
  return (f): string =>
    f === 'manual'
      ? t`Manual — never change automatically`
      : f === 'notify'
        ? t`Notify — tell me about upstream changes`
        : t`Auto — apply URL/header changes`;
}

/** The bulk-add confirmation dialog. */
export function BulkAddDialog({
  open,
  streams,
  takenIds,
  catNames,
  onClose,
  onDone,
}: {
  readonly open: boolean;
  readonly streams: readonly IndexStream[];
  readonly takenIds: ReadonlySet<string>;
  readonly catNames?: ReadonlyMap<string, string>;
  readonly onClose: () => void;
  readonly onDone: (okKeys: readonly string[]) => void;
}): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const followLabel = useFollowLabel();
  const [follow, setFollow] = useState<OriginFollow>('notify');
  // No preselection: the operator says how the whole batch starts, and the
  // server refuses a batch above its old cap that leaves the choice implicit.
  const [activation, setActivation] = useState<SourceActivationChoice | null>(null);
  const [results, setResults] = useState<readonly ItemResult[] | null>(null);
  const [running, setRunning] = useState(false);

  const run = async (): Promise<void> => {
    if (activation === null) {
      return;
    }
    setRunning(true);
    const taken = new Set(takenIds);
    const out: ItemResult[] = [];
    const entries: BulkAddEntry[] = [];
    const sent: { key: string; title: string; id: string }[] = [];
    for (const s of streams) {
      const form = toSourceForm(s, taken, follow, new Date(), catNames, activation);
      taken.add(form.id);
      const invalid = Object.keys(validateSourceForm(form, true)).length > 0;
      if (invalid) {
        out.push({ key: s.k, title: s.title, id: form.id, ok: false, message: t`Invalid source` });
        continue;
      }
      entries.push({ id: form.id, name: form.name.trim(), body: catalogueSourceBody(form) });
      sent.push({ key: s.k, title: s.title, id: form.id });
    }
    if (entries.length > 0) {
      // One server call: the server enforces its request cap and validates every
      // entry before it creates any (all-or-nothing on a bad request).
      try {
        const response = await bulkAddSources(entries);
        for (const item of sent) {
          const r = response.results.find((x) => x.id === item.id);
          out.push({
            ...item,
            ok: r?.status === 'created',
            message: r?.status === 'created' ? t`Created` : (r?.error ?? t`Not created`),
          });
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        for (const item of sent) {
          out.push({ ...item, ok: false, message });
        }
      }
      void queryClient.invalidateQueries({ queryKey: resourceKeys.list('sources') });
    }
    setResults(out);
    setRunning(false);
    onDone(out.filter((r) => r.ok).map((r) => r.key));
  };

  const close = (): void => {
    setResults(null);
    // Each open asks again: no choice carried over from a previous dialog.
    setActivation(null);
    onClose();
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next): void => {
        if (!next) {
          close();
        }
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            <Trans>Add selected streams as sources</Trans>
          </DialogTitle>
          <DialogDescription>
            <Trans>
              Each stream becomes an ordinary source linked to its IPTV-ORG entry. Nothing is
              probed or applied beyond creating the sources.
            </Trans>
          </DialogDescription>
        </DialogHeader>
        {results === null ? (
          <>
            <ul className="max-h-60 space-y-1 overflow-y-auto text-sm">
              {streams.map((s) => (
                <li key={s.k} lang="" dir="auto">
                  {s.title}
                  {s.q !== null ? ` · ${s.q}` : ''}
                </li>
              ))}
            </ul>
            <ActivationChoice
              value={activation}
              onChange={setActivation}
              count={streams.length}
              name="catalogue-bulk-activation"
            />
            <SelectField<OriginFollow>
              label={t`Follow upstream changes`}
              value={follow}
              options={ORIGIN_FOLLOWS}
              optionLabel={followLabel}
              onChange={setFollow}
            />
          </>
        ) : (
          <ul className="max-h-60 space-y-1 overflow-y-auto text-sm" aria-label={t`Results`}>
            {results.map((r) => (
              <li key={r.key} className="flex items-start gap-2" data-ok={r.ok ? 'true' : 'false'}>
                {r.ok ? (
                  <CircleCheck className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
                ) : (
                  <CircleX className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
                )}
                <span lang="" dir="auto">
                  <strong>{r.title}</strong> <code>{r.id}</code> —{' '}
                  {r.ok ? <Trans>Created</Trans> : <Trans>Failed: {r.message}</Trans>}
                </span>
              </li>
            ))}
          </ul>
        )}
        <DialogFooter>
          <Button type="button" variant="outline" onClick={close}>
            {results === null ? <Trans>Cancel</Trans> : <Trans>Close</Trans>}
          </Button>
          {results === null ? (
            <Button
              type="button"
              disabled={running || streams.length === 0 || activation === null}
              onClick={(): void => {
                void run();
              }}
            >
              <Trans>Add {streams.length} sources</Trans>
            </Button>
          ) : null}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
