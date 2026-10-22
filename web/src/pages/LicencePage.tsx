// Licence (/settings/licence) — the entitlement panel + the enforcement-ladder
// badge/sentence + the offline lease exchange.
//
// The licence resource is DATA the surface renders (ADR-0050 §6): the same
// computed `enforcement` level the engine and the portals read. Every rung keeps
// a running program ON AIR — the copy says so plainly. The offline exchange lets
// a machine with no internet path export a salted challenge (CBOR) and install a
// signed lease (CBOR) it carried back from an online portal; the §3.5 standing
// paragraph states the Ed25519 / pinned-key guarantee.
import { useMemo, useRef, useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { getChallenge, installLease } from "../api/conspect";
import type { LicenceStatusDoc } from "../api/conspectQueries";
import { useHeartbeatStatus, useLicence } from "../api/conspectQueries";
import { EnforcementBadge } from "../components/account/enforcement";
import { useEnforcementSentence } from "../components/account/enforcement-copy";
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

/**
 * Read a `File`'s bytes via `FileReader` (universally supported; `Blob`'s newer
 * `arrayBuffer()` is not available in every runtime). Rejects on read error.
 */
function readFileBytes(file: File): Promise<Uint8Array> {
  return new Promise<Uint8Array>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = (): void => {
      const result = reader.result;
      if (result instanceof ArrayBuffer) {
        resolve(new Uint8Array(result));
      } else {
        reject(new Error("Unexpected file read result"));
      }
    };
    reader.onerror = (): void => {
      reject(reader.error ?? new Error("File read failed"));
    };
    reader.readAsArrayBuffer(file);
  });
}

/** Trigger a browser download of `bytes` under `filename`. */
function downloadBytes(bytes: Uint8Array, filename: string, mime: string): void {
  const blob = new Blob([bytes.slice().buffer], { type: mime });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}

/** A labelled key/value row in a definition list. */
function Field({
  label,
  children,
}: {
  readonly label: JSX.Element;
  readonly children: JSX.Element | string;
}): JSX.Element {
  return (
    <div className="flex flex-wrap items-baseline justify-between gap-2 py-1.5">
      <dt className="text-sm text-muted-foreground">{label}</dt>
      <dd className="text-sm font-medium">{children}</dd>
    </div>
  );
}

/** The entitlement panel — tier, hardware class, gpu allowance, usage, lease. */
function EntitlementPanel({ status }: { readonly status: LicenceStatusDoc }): JSX.Element {
  const { i18n } = useLingui();
  const locale = i18n.locale;
  const sentence = useEnforcementSentence();
  const gpuLimit =
    status.gpu_limit.kind === "limited"
      ? String(status.gpu_limit.value)
      : undefined;

  // "Renewing vs lapsed" is read from the computed enforcement level (data, not
  // a wall-clock comparison in render): active/warning leases renew at the next
  // contact; any harder rung means the lease has lapsed past its term.
  const expired =
    status.enforcement !== "active" && status.enforcement !== "warning";

  return (
    <Card data-testid="entitlement-panel">
      <CardHeader>
        <CardTitle>
          <Trans>Entitlement</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Your licensed tier, hardware class, and the lease this machine holds.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent>
        <div className="mb-4 flex flex-wrap items-center gap-2">
          <span data-testid="enforcement-badge">
            <EnforcementBadge level={status.enforcement} />
          </span>
        </div>
        <p data-testid="enforcement-sentence" className="mb-4 text-sm text-muted-foreground">
          {sentence(status.enforcement)}
        </p>

        <dl className="divide-y">
          <Field label={<Trans>Tier</Trans>}>
            <span className="font-mono">{status.tier}</span>
          </Field>
          <Field label={<Trans>Hardware class — licensed</Trans>}>
            <span className="font-mono">{status.hardware_class.licensed}</span>
          </Field>
          <Field label={<Trans>Hardware class — detected</Trans>}>
            <span className="font-mono">{status.hardware_class.detected}</span>
          </Field>
          <Field label={<Trans>GPU allowance</Trans>}>
            {gpuLimit !== undefined ? (
              <span className="font-mono">{gpuLimit}</span>
            ) : (
              <Trans>unlimited</Trans>
            )}
          </Field>
          <Field label={<Trans>GPUs in use</Trans>}>
            <span className="font-mono tabular-nums">{String(status.gpus_in_use)}</span>
          </Field>
          <Field label={<Trans>Lease serial</Trans>}>
            <span className="font-mono">{status.lease.serial}</span>
          </Field>
          <Field label={expired ? <Trans>Expired</Trans> : <Trans>Renews on</Trans>}>
            {expired ? (
              instant(locale, status.lease.expires_at)
            ) : (
              <>
                <Trans>renews at</Trans> {instant(locale, status.lease.next_contact_due)}
              </>
            )}
          </Field>
          <Field label={<Trans>Grace until</Trans>}>
            {instant(locale, status.lease.grace_until)}
          </Field>
        </dl>

        <div data-testid="licence-flags" className="mt-4 flex flex-wrap gap-2">
          <Badge variant={status.config_locked ? "reconnecting" : "live"}>
            {status.config_locked ? (
              <Trans>Reconfiguration locked</Trans>
            ) : (
              <Trans>Reconfiguration allowed</Trans>
            )}
          </Badge>
          <Badge variant={status.watermark ? "reconnecting" : "live"}>
            {status.watermark ? (
              <Trans>Canvas watermark on</Trans>
            ) : (
              <Trans>No watermark</Trans>
            )}
          </Badge>
          <Badge variant={status.blocks_new_instances ? "nosignal" : "live"}>
            {status.blocks_new_instances ? (
              <Trans>New instances blocked</Trans>
            ) : (
              <Trans>New instances allowed</Trans>
            )}
          </Badge>
        </div>

        {status.reasons.length > 0 ? (
          <div className="mt-4">
            <p className="mb-1 text-xs text-muted-foreground">
              <Trans>Reasons</Trans>
            </p>
            <div className="flex flex-wrap gap-1.5">
              {status.reasons.map((reason) => (
                <code
                  key={reason}
                  className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs"
                >
                  {reason}
                </code>
              ))}
            </div>
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}

/** The heartbeat summary on the Licence screen (read-only, sourced locally). */
function HeartbeatSummary(): JSX.Element {
  const { i18n } = useLingui();
  const locale = i18n.locale;
  const heartbeat = useHeartbeatStatus();
  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Licensing contact</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            The transport the active lease arrived over, and when the next
            licensing contact is due.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent>
        {heartbeat.isPending ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Loading licensing contact…</Trans>
          </p>
        ) : heartbeat.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load the licensing contact:</Trans>{" "}
            {heartbeat.error.message}
          </p>
        ) : (
          <dl className="divide-y">
            <Field label={<Trans>Transport</Trans>}>
              <span className="font-mono">{heartbeat.data.transport}</span>
            </Field>
            <Field label={<Trans>Last contact</Trans>}>
              {instant(locale, heartbeat.data.last_at)}
            </Field>
            <Field label={<Trans>Next due</Trans>}>
              {instant(locale, heartbeat.data.next_due)}
            </Field>
          </dl>
        )}
      </CardContent>
    </Card>
  );
}

/** The offline lease exchange: export challenge + install lease + the methods. */
function OfflineExchange(): JSX.Element {
  const { t } = useLingui();
  const [file, setFile] = useState<File | undefined>(undefined);
  const [installing, setInstalling] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [installed, setInstalled] = useState<
    { readonly serial: string; readonly validTo: string } | undefined
  >(undefined);
  const fileRef = useRef<HTMLInputElement>(null);

  const exportChallenge = async (): Promise<void> => {
    setExporting(true);
    try {
      const bytes = await getChallenge();
      downloadBytes(bytes, "licence-challenge.cbor", "application/cbor");
      toast({
        title: t`Challenge exported`,
        description: t`Carry the file to an online portal to obtain a signed lease.`,
      });
    } catch (error) {
      toast({
        title: t`Could not export the challenge`,
        description: error instanceof Error ? error.message : t`Unknown error`,
        variant: "destructive",
      });
    } finally {
      setExporting(false);
    }
  };

  const doInstall = async (): Promise<void> => {
    // Read the selected file from the input at click time (robust to React state
    // batching between the upload and the click).
    const selected = fileRef.current?.files?.[0] ?? file;
    if (selected === undefined) {
      return;
    }
    setInstalling(true);
    try {
      const bytes = await readFileBytes(selected);
      const result = await installLease(bytes);
      setInstalled({ serial: result.serial, validTo: result.valid_to });
      toast({
        title: t`Lease installed`,
        description: t`Serial ${result.serial}, valid to ${result.valid_to}.`,
      });
      setFile(undefined);
      if (fileRef.current !== null) {
        fileRef.current.value = "";
      }
    } catch (error) {
      toast({
        title: t`Could not install the lease`,
        description: error instanceof Error ? error.message : t`Unknown error`,
        variant: "destructive",
      });
    } finally {
      setInstalling(false);
    }
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>
          <Trans>Offline exchange</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            When this machine has no internet path to the licence server, exchange
            a lease by hand: export a signed challenge here, obtain a lease from an
            online portal, then install the lease below.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        <section id="challenge" className="scroll-mt-16 space-y-2">
          <h3 className="text-sm font-semibold">
            <Trans>1. Export the challenge</Trans>
          </h3>
          <p className="text-sm text-muted-foreground">
            <Trans>
              Downloads a salted challenge as a CBOR file. It carries no raw
              identifiers — only salted digests the licence server can verify.
            </Trans>
          </p>
          <Button
            type="button"
            variant="outline"
            disabled={exporting}
            onClick={(): void => {
              void exportChallenge();
            }}
          >
            <Trans>Export challenge</Trans>
          </Button>
        </section>

        <section id="install-lease" className="scroll-mt-16 space-y-2">
          <h3 className="text-sm font-semibold">
            <Trans>2. Install the lease</Trans>
          </h3>
          <p className="text-sm text-muted-foreground">
            <Trans>
              Upload the signed lease file you received. It is verified against the
              pinned licence-server key before it is installed.
            </Trans>
          </p>
          <div className="grid gap-1.5">
            <Label htmlFor="lease-file">
              <Trans>Lease file</Trans>
            </Label>
            <input
              id="lease-file"
              ref={fileRef}
              type="file"
              accept=".cbor,application/cbor"
              className="block w-full text-sm file:me-3 file:rounded-md file:border file:border-input file:bg-background file:px-3 file:py-1.5 file:text-sm"
              onChange={(e): void => {
                setFile(e.target.files?.[0]);
              }}
            />
          </div>
          <Button
            type="button"
            disabled={file === undefined || installing}
            onClick={(): void => {
              void doInstall();
            }}
          >
            <Trans>Install lease</Trans>
          </Button>
          {installed !== undefined ? (
            <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
              <Trans>Installed lease</Trans>{" "}
              <code className="font-mono">{installed.serial}</code>,{" "}
              <Trans>valid to</Trans>{" "}
              <code className="font-mono">{installed.validTo}</code>.
            </p>
          ) : null}
        </section>

        <section className="space-y-2">
          <h3 className="text-sm font-semibold">
            <Trans>Install methods</Trans>
          </h3>
          <ol data-testid="install-methods" className="list-decimal space-y-1 ps-5 text-sm text-muted-foreground">
            <li>
              <Trans>
                Upload the lease file on this screen (the method above).
              </Trans>
            </li>
            <li>
              <Trans>
                Drop the lease file into the machine's configured licence directory;
                it is picked up on the next start.
              </Trans>
            </li>
            <li>
              <Trans>
                Have an online neighbour relay the exchange over the local mesh — no
                file to carry (see the Mesh screen).
              </Trans>
            </li>
          </ol>
        </section>

        <p
          data-testid="spoof-standing"
          className="rounded-md border bg-muted/40 p-3 text-sm text-muted-foreground"
        >
          <Trans>
            Leases and challenges are Ed25519-signed and verified against a public
            key pinned in the binary. A spoofed or man-in-the-middle licence server
            cannot forge a valid lease — it lacks the private key — so impersonating
            the server achieves nothing. The exchange is safe to carry over any
            medium.
          </Trans>
        </p>
      </CardContent>
    </Card>
  );
}

/** The Licence screen. */
export function LicencePage(): JSX.Element {
  const licence = useLicence();
  const status = useMemo<LicenceStatusDoc | null>(
    () => licence.data?.status ?? null,
    [licence.data],
  );

  return (
    <>
      <PageHeader
        title={<Trans>Licence</Trans>}
        description={
          <Trans>
            Your entitlement, the enforcement state, and the offline lease
            exchange. Enforcement degrades only conveniences; a running program
            always stays on air.
          </Trans>
        }
      />

      <div className="grid gap-4 lg:grid-cols-2">
        {licence.isPending ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Loading licence…</Trans>
          </p>
        ) : licence.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load the licence:</Trans> {licence.error.message}
          </p>
        ) : status !== null ? (
          <EntitlementPanel status={status} />
        ) : (
          <Card data-testid="entitlement-panel">
            <CardHeader>
              <CardTitle>
                <Trans>No licence installed</Trans>
              </CardTitle>
              <CardDescription>
                <Trans>
                  No licence is installed on this machine. Use the offline exchange
                  to install a signed lease.
                </Trans>
              </CardDescription>
            </CardHeader>
          </Card>
        )}

        <HeartbeatSummary />
        <div className="lg:col-span-2">
          <OfflineExchange />
        </div>
      </div>
    </>
  );
}
