// Data (/settings/data) — the TWO pipes, never co-mingled (§4, ADR-0052).
//
// LEFT panel: the LICENSING HEARTBEAT — a locked row (no toggle: it is the
// licensing keep-alive, not analytics), "always on", the last/next-due contact,
// the exhaustive payload-field list, and the honest source-build caveat.
// RIGHT panel: the PRODUCT TELEMETRY — an opt-in consent switch wired to
// GET/PUT, the schema summary (sent + never-sent) linked from the schema
// endpoint, what consent enables / what staying off costs, and the incentive
// line. The two are SEPARATE panels with separate copy and separate API
// surfaces — they are never presented as one switch.
//
// A diagnostics-snapshot button assembles a redacted local bundle (202 → read
// back → download) — built from the consent-independent local buffer, so it
// works regardless of the telemetry consent above.
import { useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { getSnapshot, requestSnapshot } from "../api/conspect";
import type { DiagnosticsSnapshot } from "../api/conspectQueries";
import {
  useConsent,
  useHeartbeatStatus,
  useSetConsent,
  useTelemetrySchema,
} from "../api/conspectQueries";
import { Switch } from "../components/account/Switch";
import { PageHeader } from "../components/PageHeader";
import { Button } from "../components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
import { toast } from "../components/ui/use-toast";
import { formatDateTime } from "../i18n/format";

/** Render an RFC 3339 instant for the active locale, or an em dash when absent. */
function instant(locale: string, rfc3339: string | null | undefined): string {
  if (rfc3339 === null || rfc3339 === undefined || rfc3339 === "") {
    return "—";
  }
  const date = new Date(rfc3339);
  if (Number.isNaN(date.getTime())) {
    return rfc3339;
  }
  return formatDateTime(locale, date);
}

/** Trigger a browser download of `text` under `filename`. */
function downloadText(text: string, filename: string, mime: string): void {
  const blob = new Blob([text], { type: mime });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}

/** The licensing-heartbeat panel: a LOCKED row, no toggle, always on. */
function HeartbeatPanel(): JSX.Element {
  const { i18n } = useLingui();
  const locale = i18n.locale;
  const heartbeat = useHeartbeatStatus();
  return (
    <Card data-testid="heartbeat-panel">
      <CardHeader>
        <CardTitle>
          <Trans>Licensing heartbeat</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            The minimum monthly contact that keeps your entitlement live. This is
            the licensing keep-alive — not analytics — so it carries only what
            licensing needs.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* The LOCKED row — a status, NOT a toggle. */}
        <div className="flex items-center justify-between rounded-md border bg-muted/40 px-3 py-2">
          <span className="text-sm font-medium">
            <Trans>Status</Trans>
          </span>
          <span className="text-sm font-semibold">
            <Trans>Always on</Trans>
          </span>
        </div>

        {heartbeat.isPending ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Loading heartbeat status…</Trans>
          </p>
        ) : heartbeat.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load the heartbeat status:</Trans>{" "}
            {heartbeat.error.message}
          </p>
        ) : (
          <>
            <dl className="divide-y text-sm">
              <div className="flex items-baseline justify-between gap-2 py-1.5">
                <dt className="text-muted-foreground">
                  <Trans>Transport</Trans>
                </dt>
                <dd className="font-mono font-medium">{heartbeat.data.transport}</dd>
              </div>
              <div className="flex items-baseline justify-between gap-2 py-1.5">
                <dt className="text-muted-foreground">
                  <Trans>Last contact</Trans>
                </dt>
                <dd className="font-medium">{instant(locale, heartbeat.data.last_at)}</dd>
              </div>
              <div className="flex items-baseline justify-between gap-2 py-1.5">
                <dt className="text-muted-foreground">
                  <Trans>Next due</Trans>
                </dt>
                <dd className="font-medium">{instant(locale, heartbeat.data.next_due)}</dd>
              </div>
            </dl>

            <div>
              <p className="mb-1 text-sm font-medium">
                <Trans>What the heartbeat carries</Trans>
              </p>
              <p className="mb-2 text-sm text-muted-foreground">
                <Trans>
                  Salted digests and a signed lease request — never raw serials,
                  MAC addresses, or media. The exhaustive list:
                </Trans>
              </p>
              <ul className="flex flex-wrap gap-1.5">
                {heartbeat.data.payload_fields.map((field) => (
                  <li key={field}>
                    <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                      {field}
                    </code>
                  </li>
                ))}
              </ul>
            </div>
          </>
        )}

        <p className="rounded-md border bg-muted/40 p-3 text-sm text-muted-foreground">
          <Trans>
            Multiview is source-available, so a source build can compile the
            heartbeat client out. We state that plainly: removing it does not grant
            a commercial right — the licence terms bind regardless of the binary.
            The official builds carry the heartbeat for the smooth claim and lease
            experience.
          </Trans>
        </p>
      </CardContent>
    </Card>
  );
}

/** The product-telemetry panel: an opt-in consent switch + the schema summary. */
function TelemetryPanel(): JSX.Element {
  const { t, i18n } = useLingui();
  const locale = i18n.locale;
  const consent = useConsent();
  const schema = useTelemetrySchema();
  const setConsent = useSetConsent();

  const enabled = consent.data?.enabled ?? false;

  const onToggle = (next: boolean): void => {
    setConsent.mutate(next, {
      onError: (error) => {
        toast({
          title: t`Could not update consent`,
          description: error.message,
          variant: "destructive",
        });
      },
    });
  };

  return (
    <Card data-testid="telemetry-panel">
      <CardHeader>
        <CardTitle>
          <Trans>Product telemetry</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Anonymised daily product analytics. Opt-in, off by default, and
            revocable at any time. Separate from the licensing heartbeat — a
            different pipe, different consent, different data.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex items-center justify-between gap-4 rounded-md border px-3 py-2">
          <div className="min-w-0">
            <p className="text-sm font-medium">
              <Trans>Send daily product telemetry</Trans>
            </p>
            <p className="text-sm text-muted-foreground">
              {enabled ? <Trans>On</Trans> : <Trans>Off</Trans>}
              {consent.data?.changed_at !== null &&
              consent.data?.changed_at !== undefined ? (
                <>
                  {" · "}
                  <Trans>last changed</Trans> {instant(locale, consent.data.changed_at)}
                </>
              ) : null}
            </p>
          </div>
          <Switch
            label={t`Send daily product telemetry`}
            checked={enabled}
            disabled={consent.isPending || setConsent.isPending}
            onToggle={onToggle}
          />
        </div>

        <div>
          <p className="mb-1 text-sm font-medium">
            <Trans>What is sent when you opt in</Trans>
          </p>
          <ul className="mb-3 flex flex-wrap gap-1.5">
            {(schema.data?.sent ?? []).map((field) => (
              <li key={field}>
                <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                  {field}
                </code>
              </li>
            ))}
          </ul>
          <p className="mb-1 text-sm font-medium">
            <Trans>What is never sent</Trans>
          </p>
          <ul className="flex flex-wrap gap-1.5">
            {(schema.data?.never_sent ?? []).map((field) => (
              <li key={field}>
                <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                  {field}
                </code>
              </li>
            ))}
          </ul>
          <p className="mt-2 text-sm text-muted-foreground">
            <a
              href="/api/v1/telemetry/schema"
              className="underline underline-offset-2"
            >
              <Trans>View the full telemetry schema</Trans>
            </a>
            {schema.data?.version !== undefined ? (
              <>
                {" "}
                (<Trans>version</Trans> <span className="font-mono">{schema.data.version}</span>)
              </>
            ) : null}
          </p>
        </div>

        <div className="space-y-2 text-sm text-muted-foreground">
          <p data-testid="telemetry-enables">
            <span className="font-medium text-foreground">
              <Trans>Turning it on</Trans>
            </span>{" "}
            <Trans>
              shares anonymised usage counts that help us prioritise the features
              and hardware you actually use.
            </Trans>
          </p>
          <p data-testid="telemetry-cost">
            <span className="font-medium text-foreground">
              <Trans>Staying off</Trans>
            </span>{" "}
            <Trans>
              costs you nothing functional — every feature works the same. We
              simply have no usage signal from your machine.
            </Trans>
          </p>
          <p data-testid="telemetry-incentive">
            <Trans>
              If you can spare it, leaving telemetry on helps a small team build the
              right things — thank you.
            </Trans>
          </p>
        </div>
      </CardContent>
    </Card>
  );
}

/** The diagnostics-snapshot control: 202 → read back → download. */
function DiagnosticsSnapshotCard(): JSX.Element {
  const { t } = useLingui();
  const [working, setWorking] = useState(false);
  const [snapshot, setSnapshot] = useState<DiagnosticsSnapshot | undefined>(undefined);

  const build = async (): Promise<void> => {
    setWorking(true);
    setSnapshot(undefined);
    try {
      const accepted = await requestSnapshot();
      const ready = await getSnapshot(accepted.snapshot_id);
      setSnapshot(ready);
      toast({
        title: t`Diagnostics snapshot ready`,
        description: t`Snapshot ${ready.snapshot_id} assembled.`,
      });
    } catch (error) {
      toast({
        title: t`Could not assemble the snapshot`,
        description: error instanceof Error ? error.message : t`Unknown error`,
        variant: "destructive",
      });
    } finally {
      setWorking(false);
    }
  };

  const download = (): void => {
    if (snapshot === undefined) {
      return;
    }
    downloadText(
      JSON.stringify(snapshot, null, 2),
      `diagnostics-${snapshot.snapshot_id}.json`,
      "application/json",
    );
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Diagnostics snapshot</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Assemble a redacted local diagnostics bundle for your own
            troubleshooting — logs and engine state, never media or raw
            identifiers. Built from the local buffer, independent of telemetry
            consent.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-wrap items-center gap-3">
        <Button
          type="button"
          variant="outline"
          disabled={working}
          onClick={(): void => {
            void build();
          }}
        >
          <Trans>Build diagnostics snapshot</Trans>
        </Button>
        {snapshot !== undefined ? (
          <Button type="button" onClick={download}>
            <Trans>Download snapshot</Trans>
          </Button>
        ) : null}
      </CardContent>
    </Card>
  );
}

/** The Data screen. */
export function DataPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>Data</Trans>}
        description={
          <Trans>
            What this machine sends, and to whom. Two separate pipes: the
            mandatory licensing heartbeat and the opt-in product telemetry.
          </Trans>
        }
      />

      <div className="grid gap-4 lg:grid-cols-2">
        <HeartbeatPanel />
        <TelemetryPanel />
        <div className="lg:col-span-2">
          <DiagnosticsSnapshotCard />
        </div>
      </div>
    </>
  );
}
