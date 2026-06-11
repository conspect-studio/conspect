// Alarms — the active/historical fault list with operator acknowledgement.
//
// Reads `GET /api/v1/alarms` (filterable by severity, active state, and scope
// kind) and acknowledges one via `POST /api/v1/alarms/{id}/ack` under `If-Match`.
// Severity and acknowledgement are conveyed with text + a glyph, never colour
// alone (WCAG 1.4.1). The engine is isolated (invariant #10): the list is a
// best-effort read and degrades to loading / error states; an ack refetches the
// list so the table reflects authoritative server state.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import {
  BellOff,
  CheckCircle2,
  CircleAlert,
  CircleDot,
  ShieldAlert,
  TriangleAlert,
} from 'lucide-react';

import { useAckAlarm, useAlarms } from '../api/alarmsQueries';
import type { AlarmRecord, Severity } from '../api/alarmsQueries';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import type { BadgeProps } from '../components/ui/badge';
import { Button } from '../components/ui/button';
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
import { toast } from '../components/ui/use-toast';

/** The X.733 severities, lowest to highest, plus an "any" sentinel. */
const SEVERITIES: readonly Severity[] = [
  'Cleared',
  'Indeterminate',
  'Warning',
  'Minor',
  'Major',
  'Critical',
];

const ANY = '__any__';
const ACTIVE_ANY = '__any__';

/** Narrow a select value to a known {@link Severity}, or `undefined`. */
function asSeverity(value: string): Severity | undefined {
  return SEVERITIES.find((s) => s === value);
}

interface SeverityPresentation {
  readonly variant: NonNullable<BadgeProps['variant']>;
  readonly icon: JSX.Element;
}

function severityPresentation(severity: Severity): SeverityPresentation {
  switch (severity) {
    case 'Critical':
    case 'Major':
      return {
        variant: 'destructive',
        icon: <ShieldAlert className="size-3.5" aria-hidden="true" />,
      };
    case 'Minor':
    case 'Warning':
      return {
        variant: 'stale',
        icon: <TriangleAlert className="size-3.5" aria-hidden="true" />,
      };
    case 'Indeterminate':
      return {
        variant: 'reconnecting',
        icon: <CircleAlert className="size-3.5" aria-hidden="true" />,
      };
    case 'Cleared':
      return {
        variant: 'live',
        icon: <CircleDot className="size-3.5" aria-hidden="true" />,
      };
  }
}

/** A salvo-scope label as plain text (text carries the meaning). */
function scopeLabel(scope: AlarmRecord['scope']): string {
  switch (scope.kind) {
    case 'tile':
      return `tile #${String(scope.index)}`;
    case 'probe':
      return `probe ${scope.id}`;
    case 'group':
      return `group ${scope.name}`;
    case 'system':
      return 'system';
  }
}

/** The alarms management page. */
export function AlarmsPage(): JSX.Element {
  const { t } = useLingui();
  const [severity, setSeverity] = useState<string>(ANY);
  const [active, setActive] = useState<string>(ACTIVE_ANY);

  const filter = useMemo(() => {
    const next: { severity?: Severity; active?: boolean } = {};
    const chosen = asSeverity(severity);
    if (chosen !== undefined) {
      next.severity = chosen;
    }
    if (active === 'true') {
      next.active = true;
    } else if (active === 'false') {
      next.active = false;
    }
    return next;
  }, [severity, active]);

  const alarms = useAlarms(filter);
  const ack = useAckAlarm();
  const data = alarms.data ?? [];

  const onAck = (alarm: AlarmRecord): void => {
    ack.mutate(alarm.id, {
      onSuccess: (): void => {
        toast({ title: t`Alarm acknowledged` });
      },
      onError: (error): void => {
        toast({
          title: t`Could not acknowledge alarm`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
  };

  return (
    <>
      <PageHeader
        title={<Trans>Alarms</Trans>}
        description={
          <Trans>
            Active and historical faults the engine raised. Acknowledge an alarm
            to record that an operator has seen it.
          </Trans>
        }
      />

      <div className="mb-4 flex flex-wrap items-end gap-4">
        <div className="grid gap-1.5">
          <Label htmlFor="alarm-severity">
            <Trans>Minimum severity</Trans>
          </Label>
          <Select value={severity} onValueChange={setSeverity}>
            <SelectTrigger id="alarm-severity" className="w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ANY}>{t`Any severity`}</SelectItem>
              {SEVERITIES.map((s) => (
                <SelectItem key={s} value={s}>
                  {s}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        <div className="grid gap-1.5">
          <Label htmlFor="alarm-active">
            <Trans>State</Trans>
          </Label>
          <Select value={active} onValueChange={setActive}>
            <SelectTrigger id="alarm-active" className="w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ACTIVE_ANY}>{t`Active and cleared`}</SelectItem>
              <SelectItem value="true">{t`Active only`}</SelectItem>
              <SelectItem value="false">{t`Cleared only`}</SelectItem>
            </SelectContent>
          </Select>
        </div>
      </div>

      {alarms.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading alarms…</Trans>
        </p>
      ) : alarms.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load alarms:</Trans> {alarms.error.message}
        </p>
      ) : data.length === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">
            <Trans>No alarms match the current filter.</Trans>
          </p>
        </div>
      ) : (
        <Table>
          <TableCaption>{t`Alarms matching the current filter.`}</TableCaption>
          <TableHeader>
            <TableRow>
              <TableHead>{t`Severity`}</TableHead>
              <TableHead>{t`Kind`}</TableHead>
              <TableHead>{t`Scope`}</TableHead>
              <TableHead>{t`Acknowledged`}</TableHead>
              <TableHead>{t`Actions`}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {data.map((alarm) => {
              const present = severityPresentation(alarm.severity);
              const acked = alarm.ack.state === 'Acked';
              return (
                <TableRow key={alarm.id}>
                  <TableCell>
                    <Badge variant={present.variant}>
                      {present.icon}
                      <span>{alarm.severity}</span>
                    </Badge>
                  </TableCell>
                  <TableCell className="font-medium">{alarm.kind}</TableCell>
                  <TableCell>
                    <code className="text-xs text-muted-foreground">
                      {scopeLabel(alarm.scope)}
                    </code>
                  </TableCell>
                  <TableCell>
                    {acked ? (
                      <span className="inline-flex items-center gap-1 text-sm">
                        <CheckCircle2 className="size-3.5" aria-hidden="true" />
                        <Trans>Acknowledged</Trans>
                      </span>
                    ) : (
                      <span className="inline-flex items-center gap-1 text-sm text-muted-foreground">
                        <BellOff className="size-3.5" aria-hidden="true" />
                        <Trans>Unacknowledged</Trans>
                      </span>
                    )}
                  </TableCell>
                  <TableCell>
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={acked || ack.isPending}
                      aria-label={`${t`Acknowledge alarm`}: ${alarm.kind} (${scopeLabel(alarm.scope)})`}
                      onClick={(): void => {
                        onAck(alarm);
                      }}
                    >
                      <CheckCircle2 aria-hidden="true" />
                      <Trans>Acknowledge</Trans>
                    </Button>
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      )}
    </>
  );
}
