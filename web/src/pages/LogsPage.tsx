// Logs — the read-only buffered structured log tail.
//
// Reads `GET /api/v1/logs` (oldest first), polled, and shows the recent ring of
// engine + libav log records: level, wall-clock time, the tracing target, the
// message, and any resource attribution. The level + resource-kind + resource-id
// filters narrow the tail server-side. Read-only: there is no mutation here.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import { useLogs, LOG_LEVELS, LOG_RESOURCE_KINDS } from '../api/logsQueries';
import type { LogLevel, LogQuery, LogRecord, LogResourceKind } from '../api/logsQueries';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import type { BadgeProps } from '../components/ui/badge';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../components/ui/select';
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
import { formatDateTime } from '../i18n/format';

/** The "any" sentinel for the level + kind selects (Radix forbids an empty value). */
const ANY = '__any__';

/** Map a log level to a badge hue (text always carries the meaning). */
function levelVariant(level: LogLevel): BadgeProps['variant'] {
  switch (level) {
    case 'error':
      return 'destructive';
    case 'warn':
      return 'reconnecting';
    case 'info':
      return 'secondary';
    case 'debug':
    case 'trace':
      return 'outline';
  }
}

/** Narrow a select value to a known level, or undefined for "any". */
function asLevel(value: string): LogLevel | undefined {
  return LOG_LEVELS.find((level) => level === value);
}

/** Narrow a select value to a known resource kind, or undefined for "any". */
function asResourceKind(value: string): LogResourceKind | undefined {
  return LOG_RESOURCE_KINDS.find((kind) => kind === value);
}

/** The buffered-log browser page. */
export function LogsPage(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const [level, setLevel] = useState<string>(ANY);
  const [kind, setKind] = useState<string>(ANY);
  const [resourceId, setResourceId] = useState<string>('');

  const query = useMemo<LogQuery>(() => {
    const chosenLevel = asLevel(level);
    const chosenKind = asResourceKind(kind);
    const trimmed = resourceId.trim();
    return {
      ...(chosenLevel !== undefined ? { level: chosenLevel } : {}),
      ...(chosenKind !== undefined ? { kind: chosenKind } : {}),
      ...(trimmed !== '' ? { resourceId: trimmed } : {}),
    };
  }, [level, kind, resourceId]);

  const logs = useLogs(query);
  const records = useMemo<LogRecord[]>(() => logs.data ?? [], [logs.data]);

  return (
    <>
      <PageHeader
        title={<Trans>Logs</Trans>}
        description={
          <Trans>
            The recent buffered structured log tail — engine and libav records
            with their resource attribution. Read-only; refreshes on its own.
          </Trans>
        }
      />

      <div className="mb-4 flex flex-wrap items-end gap-3">
        <div className="grid gap-1.5">
          <Label htmlFor="logs-level">
            <Trans>Minimum level</Trans>
          </Label>
          <Select value={level} onValueChange={setLevel}>
            <SelectTrigger id="logs-level" className="w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ANY}>{t`Any level`}</SelectItem>
              {LOG_LEVELS.map((option) => (
                <SelectItem key={option} value={option}>
                  {option}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="grid gap-1.5">
          <Label htmlFor="logs-kind">
            <Trans>Resource kind</Trans>
          </Label>
          <Select value={kind} onValueChange={setKind}>
            <SelectTrigger id="logs-kind" className="w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ANY}>{t`Any kind`}</SelectItem>
              {LOG_RESOURCE_KINDS.map((option) => (
                <SelectItem key={option} value={option}>
                  {option}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="grid gap-1.5">
          <Label htmlFor="logs-resource">
            <Trans>Resource id</Trans>
          </Label>
          <Input
            id="logs-resource"
            className="w-56"
            value={resourceId}
            placeholder={t`Leave empty for all resources`}
            onChange={(e): void => {
              setResourceId(e.target.value);
            }}
          />
        </div>
      </div>

      {logs.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading logs…</Trans>
        </p>
      ) : logs.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load the logs:</Trans> {logs.error.message}
        </p>
      ) : records.length === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">
            <Trans>No log records match the filter.</Trans>
          </p>
        </div>
      ) : (
        <Table>
          <TableCaption>{t`Buffered log records, oldest first.`}</TableCaption>
          <TableHeader>
            <TableRow>
              <TableHead>{t`Level`}</TableHead>
              <TableHead>{t`Time`}</TableHead>
              <TableHead>{t`Target`}</TableHead>
              <TableHead>{t`Resource`}</TableHead>
              <TableHead>{t`Message`}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {records.map((record) => (
              <TableRow key={record.seq}>
                <TableCell>
                  <Badge variant={levelVariant(record.level)}>{record.level}</Badge>
                </TableCell>
                <TableCell className="whitespace-nowrap tabular-nums text-xs text-muted-foreground">
                  {formatDateTime(locale, new Date(record.timestamp_ms))}
                </TableCell>
                <TableCell>
                  <code className="text-xs text-muted-foreground">{record.target}</code>
                </TableCell>
                <TableCell>
                  {record.resource_id !== undefined && record.resource_id !== null ? (
                    <code className="text-xs">
                      {record.resource_kind !== undefined && record.resource_kind !== null
                        ? `${record.resource_kind}/${record.resource_id}`
                        : record.resource_id}
                    </code>
                  ) : (
                    <span className="text-xs text-muted-foreground" aria-hidden="true">
                      —
                    </span>
                  )}
                </TableCell>
                <TableCell>
                  <span dir="auto">{record.message}</span>
                  {record.repeated !== undefined &&
                  record.repeated !== null &&
                  record.repeated > 1 ? (
                    <span className="ml-2 text-xs text-muted-foreground">
                      {t`×${record.repeated}`}
                    </span>
                  ) : null}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </>
  );
}
