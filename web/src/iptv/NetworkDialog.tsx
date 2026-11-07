/* eslint-disable react-refresh/only-export-components -- text renderer lives beside the components that use it */
// Network-path diagnostics panel (docs/io/iptv-org.md §3a): which hosts the
// server talks to, over which addresses, and how the internet sees the server —
// IPv4 and IPv6 side by side per host. Opens from a probe result and from any
// source row. Copy as text/JSON (the server already redacts credentials/queries).
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Copy, Network } from 'lucide-react';

import type { IpFamily, NetworkAddress, NetworkHost, NetworkReport } from '../api/catalogue';
import { useSourceNetwork } from '../api/catalogueQueries';
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

const FAMILIES: readonly IpFamily[] = ['ipv4', 'ipv6'];

/** Plain-text rendering of a report, for “Copy as text”. */
export function networkReportText(r: NetworkReport): string {
  const out: string[] = [];
  out.push(
    `Egress: IPv4 ${r.egress.ipv4 ?? '-'}  IPv6 ${r.egress.ipv6 ?? '-'}  country ${r.egress.country ?? '-'}  colo ${r.egress.colo ?? '-'}`,
  );
  out.push(
    `Request headers: User-Agent ${r.request_headers.user_agent ?? '-'}  Referer ${r.request_headers.referrer ?? '-'}`,
  );
  for (const h of r.hosts) {
    out.push('', `Host ${h.host} (${h.role})`);
    out.push(
      `  DNS: CNAME ${h.dns.cname.join(' > ') || '-'}  A ${h.dns.a.join(', ') || '-'}  AAAA ${h.dns.aaaa.join(', ') || '-'}  TTL ${h.dns.ttl === null ? '-' : String(h.dns.ttl)}`,
    );
    for (const a of h.addresses) {
      out.push(
        `  ${a.family.toUpperCase()} ${a.ip} [${a.class}] connect ${a.connect_ms === null ? '-' : `${String(a.connect_ms)} ms`}  status ${a.status === null ? '-' : String(a.status)}${a.error !== null ? `  error ${a.error}` : ''}`,
      );
    }
    if (h.tls !== null) {
      out.push(
        `  TLS ${h.tls.version ?? '-'} ${h.tls.alpn ?? ''} subject ${h.tls.subject ?? '-'} issuer ${h.tls.issuer ?? '-'} expires ${h.tls.not_after ?? '-'}`,
      );
    }
  }
  r.hops.forEach((hop, i) => {
    out.push(
      '',
      `Hop ${String(i + 1)}: ${hop.url_redacted}  status ${hop.status === null ? '-' : String(hop.status)}  ${hop.ms === null ? '-' : `${String(hop.ms)} ms`}  server ${hop.server ?? '-'}  via ${hop.via ?? '-'}  age ${hop.age ?? '-'}`,
    );
    for (const [k, v] of Object.entries(hop.cdn_headers)) {
      out.push(`  ${k}: ${v}`);
    }
    if (hop.body_excerpt !== null) {
      out.push(`  body: ${hop.body_excerpt}`);
    }
  });
  if (r.assessment.length > 0) {
    out.push('', 'Assessment:', ...r.assessment.map((a) => `  - ${a}`));
  }
  if (r.curl.length > 0) {
    out.push('', 'curl:', ...r.curl);
  }
  return out.join('\n');
}

function copy(text: string, ok: string, fail: string): void {
  const clip = navigator.clipboard as Clipboard | undefined;
  const write = clip?.writeText(text);
  if (write === undefined) {
    toast({ title: fail, variant: 'destructive' });
    return;
  }
  write.then(
    () => {
      toast({ title: ok });
    },
    () => {
      toast({ title: fail, variant: 'destructive' });
    },
  );
}

function FamilyCell({ family, host }: { readonly family: IpFamily; readonly host: NetworkHost }): JSX.Element {
  const list = host.addresses.filter((a) => a.family === family);
  if (list.length === 0) {
    return (
      <td className="align-top">
        <Trans>No {family === 'ipv4' ? 'IPv4' : 'IPv6'} address</Trans>
      </td>
    );
  }
  return (
    <td className="align-top">
      <ul className="space-y-1">
        {list.map((a: NetworkAddress) => (
          <li key={a.ip}>
            <code dir="ltr">{a.ip}</code> <span className="text-xs">({a.class})</span>
            <br />
            <span>
              {a.error !== null ? (
                <Trans>Failed: {a.error}</Trans>
              ) : a.status !== null ? (
                <Trans>HTTP {a.status}</Trans>
              ) : (
                <Trans>Connected</Trans>
              )}
              {a.connect_ms !== null ? ` · ${String(a.connect_ms)} ms` : ''}
            </span>
          </li>
        ))}
      </ul>
    </td>
  );
}

/** The report body. */
export function NetworkReportView({ report }: { readonly report: NetworkReport }): JSX.Element {
  const { t } = useLingui();
  return (
    <div className="max-h-[60vh] space-y-4 overflow-y-auto text-sm" data-testid="network-report">
      <section aria-labelledby="net-egress">
        <h3 id="net-egress" className="font-semibold"><Trans>This server, as the internet sees it</Trans></h3>
        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5">
          <dt className="text-muted-foreground">IPv4</dt>
          <dd dir="ltr">{report.egress.ipv4 ?? '—'}</dd>
          <dt className="text-muted-foreground">IPv6</dt>
          <dd dir="ltr">{report.egress.ipv6 ?? '—'}</dd>
          <dt className="text-muted-foreground"><Trans>Country</Trans></dt>
          <dd>{report.egress.country ?? '—'}</dd>
          <dt className="text-muted-foreground"><Trans>CDN edge</Trans></dt>
          <dd>{report.egress.colo ?? '—'}</dd>
          <dt className="text-muted-foreground"><Trans>Request headers</Trans></dt>
          <dd className="break-all" dir="ltr">
            User-Agent: {report.request_headers.user_agent ?? '—'}; Referer:{' '}
            {report.request_headers.referrer ?? '—'}
          </dd>
        </dl>
      </section>

      {report.assessment.length > 0 ? (
        <section aria-labelledby="net-assess">
          <h3 id="net-assess" className="font-semibold"><Trans>Assessment</Trans></h3>
          <ul className="list-disc ps-5">
            {report.assessment.map((a, i) => (
              <li key={i}>{a}</li>
            ))}
          </ul>
        </section>
      ) : null}

      <section aria-labelledby="net-hosts">
        <h3 id="net-hosts" className="font-semibold"><Trans>Hosts, IPv4 vs IPv6</Trans></h3>
        {report.hosts.map((h) => (
          <div key={h.host} className="mt-2 rounded-md border p-2" data-testid={`net-host-${h.host}`}>
            <p className="font-medium">
              <span dir="ltr">{h.host}</span>{' '}
              <span className="text-xs text-muted-foreground">({h.role})</span>
            </p>
            <p className="text-xs text-muted-foreground" dir="ltr">
              CNAME {h.dns.cname.join(' → ') || '—'} · A {h.dns.a.join(', ') || '—'} · AAAA{' '}
              {h.dns.aaaa.join(', ') || '—'}
              {h.dns.ttl !== null ? ` · TTL ${String(h.dns.ttl)}` : ''}
            </p>
            <table className="mt-1 w-full">
              <caption className="sr-only">{t`Per-family result for ${h.host}`}</caption>
              <thead>
                <tr>
                  {FAMILIES.map((f) => (
                    <th key={f} scope="col" className="w-1/2 text-start">
                      {f === 'ipv4' ? 'IPv4' : 'IPv6'}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                <tr>
                  {FAMILIES.map((f) => (
                    <FamilyCell key={f} family={f} host={h} />
                  ))}
                </tr>
              </tbody>
            </table>
            {h.tls !== null ? (
              <p className="mt-1 text-xs" dir="ltr">
                TLS {h.tls.version ?? '—'} {h.tls.alpn ?? ''} · {h.tls.subject ?? '—'} ·{' '}
                {h.tls.issuer ?? '—'} · {h.tls.not_after ?? '—'}
              </p>
            ) : null}
          </div>
        ))}
      </section>

      <section aria-labelledby="net-hops">
        <h3 id="net-hops" className="font-semibold"><Trans>HTTP hops</Trans></h3>
        <ol className="list-decimal space-y-2 ps-5">
          {report.hops.map((hop, i) => (
            <li key={i}>
              <code className="break-all" dir="ltr">{hop.url_redacted}</code>{' '}
              <strong>{hop.status ?? '—'}</strong>
              {hop.ms !== null ? ` · ${String(hop.ms)} ms` : ''}
              {hop.server !== null ? ` · ${hop.server}` : ''}
              {Object.entries(hop.cdn_headers).length > 0 ? (
                <span className="block text-xs text-muted-foreground" dir="ltr">
                  {Object.entries(hop.cdn_headers).map(([k, v]) => `${k}: ${v}`).join('; ')}
                </span>
              ) : null}
              {hop.body_excerpt !== null ? (
                <blockquote className="mt-1 border-s-2 ps-2 text-xs">{hop.body_excerpt}</blockquote>
              ) : null}
            </li>
          ))}
        </ol>
      </section>
    </div>
  );
}

/** The dialog body and footer for a loaded report. */
function ReportBody({ report }: { readonly report: NetworkReport }): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <NetworkReportView report={report} />
      <DialogFooter className="flex-wrap">
        <Button
          type="button"
          variant="outline"
          onClick={(): void => {
            copy(networkReportText(report), t`Copied as text`, t`Could not copy`);
          }}
        >
          <Copy aria-hidden="true" />
          <Trans>Copy as text</Trans>
        </Button>
        <Button
          type="button"
          variant="outline"
          onClick={(): void => {
            copy(JSON.stringify(report, null, 2), t`Copied as JSON`, t`Could not copy`);
          }}
        >
          <Copy aria-hidden="true" />
          <Trans>Copy as JSON</Trans>
        </Button>
        {report.curl.length > 0 ? (
          <Button
            type="button"
            variant="outline"
            onClick={(): void => {
              copy(report.curl.join('\n'), t`Copied curl commands`, t`Could not copy`);
            }}
          >
            <Copy aria-hidden="true" />
            <Trans>Copy curl</Trans>
          </Button>
        ) : null}
      </DialogFooter>
    </>
  );
}

function FetchedBody({ sourceId }: { readonly sourceId: string }): JSX.Element {
  const q = useSourceNetwork(sourceId);
  if (q.isPending) {
    return <p role="status" className="text-sm"><Trans>Inspecting the network path…</Trans></p>;
  }
  if (q.isError) {
    return <p role="alert" className="text-sm text-destructive">{q.error.message}</p>;
  }
  return <ReportBody report={q.data} />;
}

/** Network dialog: give it a `report` (probe result) or a `sourceId` (fetched on open). */
export function NetworkDialog({
  open,
  onClose,
  report,
  sourceId,
}: {
  readonly open: boolean;
  readonly onClose: () => void;
  readonly report?: NetworkReport | null | undefined;
  readonly sourceId?: string | null | undefined;
}): JSX.Element {
  return (
    <Dialog
      open={open}
      onOpenChange={(o): void => {
        if (!o) {
          onClose();
        }
      }}
    >
      <DialogContent className="max-w-3xl">
        <DialogHeader>
          <DialogTitle><Trans>Network path</Trans></DialogTitle>
          <DialogDescription>
            <Trans>
              The hosts this server talks to, the addresses it uses, and how the internet sees it.
            </Trans>
          </DialogDescription>
        </DialogHeader>
        {open && report != null ? (
          <ReportBody report={report} />
        ) : open && sourceId != null ? (
          <FetchedBody sourceId={sourceId} />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

/** A “Network” button that opens the dialog. */
export function NetworkButton({
  label,
  report,
  sourceId,
}: {
  readonly label: string;
  readonly report?: NetworkReport | null | undefined;
  readonly sourceId?: string | null | undefined;
}): JSX.Element {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button
        type="button"
        variant="outline"
        size="sm"
        aria-label={label}
        onClick={(): void => {
          setOpen(true);
        }}
      >
        <Network aria-hidden="true" />
        <Trans>Network</Trans>
      </Button>
      <NetworkDialog
        open={open}
        report={report}
        sourceId={sourceId}
        onClose={(): void => {
          setOpen(false);
        }}
      />
    </>
  );
}
