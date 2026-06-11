// Audit — the read-only change log.
//
// Reads `GET /api/v1/audit` (newest first) and shows who did what to which
// object, and when. The `when` is a media-timeline nanosecond stamp (NOT a
// wall-clock time), so it is rendered as a media-time value, not a calendar date.
// An optional object-id filter scopes the listing to a single object's history.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import { useAudit } from '../api/auditQueries';
import type { AuditEntry } from '../api/auditQueries';
import { PageHeader } from '../components/PageHeader';
import { Button } from '../components/ui/button';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../components/ui/table';
import { useActiveLocale } from '../i18n/I18nProvider';
import { formatNumber } from '../i18n/format';

/** Render a media-time nanosecond stamp as locale-formatted seconds. */
function mediaSeconds(locale: string, nanos: number): string {
  // The audit `at_nanos` is on the engine media timeline, not wall-clock; show
  // it as a number of seconds on that timeline so the column is honest.
  const seconds = nanos / 1_000_000_000;
  return `${formatNumber(locale, seconds, { maximumFractionDigits: 3 })} s`;
}

/** The audit-log page. */
export function AuditPage(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const [objectFilter, setObjectFilter] = useState('');
  const [applied, setApplied] = useState<string | undefined>(undefined);

  const audit = useAudit(applied);
  const entries = useMemo<AuditEntry[]>(() => audit.data ?? [], [audit.data]);

  const apply = (): void => {
    const trimmed = objectFilter.trim();
    setApplied(trimmed === '' ? undefined : trimmed);
  };

  return (
    <>
      <PageHeader
        title={<Trans>Audit</Trans>}
        description={
          <Trans>
            The immutable change log: who changed what, and when, on the engine
            media timeline. Read-only.
          </Trans>
        }
      />

      <form
        className="mb-4 flex flex-wrap items-end gap-3"
        onSubmit={(e): void => {
          e.preventDefault();
          apply();
        }}
      >
        <div className="grid gap-1.5">
          <Label htmlFor="audit-object">
            <Trans>Filter by object id</Trans>
          </Label>
          <Input
            id="audit-object"
            className="w-64"
            value={objectFilter}
            placeholder={t`Leave empty for all objects`}
            onChange={(e): void => {
              setObjectFilter(e.target.value);
            }}
          />
        </div>
        <Button type="submit" variant="outline">
          <Trans>Apply filter</Trans>
        </Button>
      </form>

      {audit.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading audit log…</Trans>
        </p>
      ) : audit.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load the audit log:</Trans> {audit.error.message}
        </p>
      ) : entries.length === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">
            <Trans>No audit entries match.</Trans>
          </p>
        </div>
      ) : (
        <Table>
          <TableCaption>{t`Change log, newest first.`}</TableCaption>
          <TableHeader>
            <TableRow>
              <TableHead>{t`Action`}</TableHead>
              <TableHead>{t`Object`}</TableHead>
              <TableHead>{t`Actor`}</TableHead>
              <TableHead>{t`Media time`}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {entries.map((entry, i) => (
              <TableRow key={`${entry.object_kind}/${entry.object_id}/${String(entry.at_nanos)}/${String(i)}`}>
                <TableCell className="font-medium">{entry.action}</TableCell>
                <TableCell>
                  <code className="text-xs text-muted-foreground">
                    {entry.object_kind}/{entry.object_id}
                  </code>
                </TableCell>
                <TableCell>
                  <span lang="" dir="auto">
                    {entry.actor}
                  </span>
                </TableCell>
                <TableCell className="tabular-nums">
                  {mediaSeconds(locale, entry.at_nanos)}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </>
  );
}
