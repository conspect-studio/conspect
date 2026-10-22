// System Actions (/system/actions) — the pending remote-actions strip + the
// append-only account audit log (§10).
//
// PENDING STRIP: queued remote actions (restart/reboot/salvo) the operator can
// cancel locally before they run (POST /api/v1/actions/{id}/cancel) — local
// always wins before execution. AUDIT LOG: the immutable, append-only account
// audit trail (GET /api/v1/account/audit), cursor-paginated and filterable by
// kind, with the four spec fields (kind/actor/at/detail). There is no mutating
// verb on the audit log by construction.
import { useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { useAccountAudit, useCancelAction, usePendingActions } from "../api/conspectQueries";
import type { AccountAuditKind } from "../api/conspectQueries";
import { PageHeader } from "../components/PageHeader";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
import { Label } from "../components/ui/label";
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "../components/ui/table";
import { toast } from "../components/ui/use-toast";
import { formatNumber } from "../i18n/format";

/** The account-audit kinds, for the filter dropdown (the schema enum, kebab). */
const AUDIT_KINDS: readonly AccountAuditKind[] = [
  "claim",
  "transfer",
  "lease-grant",
  "lease-install",
  "enforcement-change",
  "consent-change",
  "relay-toggle",
  "context-pack-export",
  "ticket",
  "bundle-compose",
  "data-request-approve",
  "data-request-deny",
  "action-requested",
  "action-cancelled",
  "action-executed",
];

/** Narrow a raw select value to a known audit kind, or `undefined` for "all". */
function toAuditKind(raw: string): AccountAuditKind | undefined {
  return AUDIT_KINDS.find((k) => k === raw);
}

/** Render a media-time nanosecond stamp as locale-formatted seconds. */
function mediaSeconds(locale: string, nanos: number): string {
  const seconds = nanos / 1_000_000_000;
  return `${formatNumber(locale, seconds, { maximumFractionDigits: 3 })} s`;
}

/** A short, structured rendering of an audit `detail` (never secrets). */
function renderDetail(detail: unknown): string {
  if (detail === null || detail === undefined) {
    return "—";
  }
  if (typeof detail === "string") {
    return detail;
  }
  return JSON.stringify(detail);
}

/** The pending remote-actions strip. */
function PendingStrip(): JSX.Element {
  const { t } = useLingui();
  const pending = usePendingActions();
  const cancel = useCancelAction();
  const rows = (pending.data ?? []).filter((a) => a.state === "pending");

  const doCancel = (id: string): void => {
    cancel.mutate(id, {
      onError: (error) => {
        toast({
          title: t`Could not cancel the action`,
          description: error.message,
          variant: "destructive",
        });
      },
    });
  };

  return (
    <Card data-testid="pending-strip">
      <CardHeader>
        <CardTitle>
          <Trans>Pending actions</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Remote actions queued for this machine. You can cancel one locally
            before it runs — a local cancel always wins before execution.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent>
        {pending.isPending ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Loading pending actions…</Trans>
          </p>
        ) : pending.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load pending actions:</Trans> {pending.error.message}
          </p>
        ) : rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            <Trans>No pending actions.</Trans>
          </p>
        ) : (
          <ul className="space-y-2">
            {rows.map((action) => (
              <li
                key={action.action_id}
                className="flex flex-wrap items-center justify-between gap-3 rounded-md border px-3 py-2"
              >
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <Badge variant="stale">{action.kind}</Badge>
                    <code className="font-mono text-xs">{action.action_id}</code>
                  </div>
                  <p className="mt-1 text-xs text-muted-foreground">
                    <Trans>requested by</Trans>{" "}
                    <span className="font-mono">{action.requested_by}</span>
                  </p>
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={cancel.isPending}
                  onClick={(): void => {
                    doCancel(action.action_id);
                  }}
                >
                  <Trans>Cancel</Trans>
                </Button>
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

/** The append-only account audit log (cursor-paginated, filterable). */
function AccountAuditLog(): JSX.Element {
  const { t, i18n } = useLingui();
  const locale = i18n.locale;
  const [cursor, setCursor] = useState<number | undefined>(undefined);
  const [filter, setFilter] = useState<AccountAuditKind | undefined>(undefined);

  const audit = useAccountAudit({
    ...(cursor !== undefined ? { cursor } : {}),
    ...(filter !== undefined ? { filter } : {}),
  });
  const entries = audit.data?.entries ?? [];
  const nextCursor = audit.data?.next_cursor ?? null;

  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Account audit log</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Every account-side action — claim, transfer, lease, consent,
            enforcement change, and more. Immutable and append-only.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form className="mb-4 flex flex-wrap items-end gap-3">
          <div className="grid gap-1.5">
            <Label htmlFor="audit-filter">
              <Trans>Filter by kind</Trans>
            </Label>
            <select
              id="audit-filter"
              className="h-9 min-h-[44px] rounded-md border border-input bg-background px-3 text-sm"
              value={filter ?? ""}
              onChange={(e): void => {
                setCursor(undefined);
                setFilter(toAuditKind(e.target.value));
              }}
            >
              <option value="">{t`All kinds`}</option>
              {AUDIT_KINDS.map((kind) => (
                <option key={kind} value={kind}>
                  {kind}
                </option>
              ))}
            </select>
          </div>
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
          <>
            <Table data-testid="account-audit-table">
              <TableCaption>{t`Account audit log, oldest first within the page.`}</TableCaption>
              <TableHeader>
                <TableRow>
                  <TableHead>{t`Kind`}</TableHead>
                  <TableHead>{t`Actor`}</TableHead>
                  <TableHead>{t`Media time`}</TableHead>
                  <TableHead>{t`Detail`}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {entries.map((entry) => (
                  <TableRow key={entry.seq}>
                    <TableCell className="font-medium">{entry.kind}</TableCell>
                    <TableCell>
                      <span className="font-mono text-xs">{entry.actor}</span>
                    </TableCell>
                    <TableCell className="tabular-nums">
                      {mediaSeconds(locale, entry.at_nanos)}
                    </TableCell>
                    <TableCell>
                      <code className="font-mono text-xs text-muted-foreground">
                        {renderDetail(entry.detail)}
                      </code>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>

            <div className="mt-4 flex items-center gap-2">
              <Button
                type="button"
                variant="outline"
                disabled={nextCursor === null}
                onClick={(): void => {
                  if (nextCursor !== null) {
                    setCursor(nextCursor);
                  }
                }}
              >
                <Trans>Next page</Trans>
              </Button>
              {cursor !== undefined ? (
                <Button
                  type="button"
                  variant="ghost"
                  onClick={(): void => {
                    setCursor(undefined);
                  }}
                >
                  <Trans>Back to start</Trans>
                </Button>
              ) : null}
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}

/** The System Actions screen. */
export function SystemActionsPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>System actions</Trans>}
        description={
          <Trans>
            Queued remote actions you can cancel locally, and the immutable account
            audit trail.
          </Trans>
        }
      />

      <div className="space-y-4">
        <PendingStrip />
        <AccountAuditLog />
      </div>
    </>
  );
}
