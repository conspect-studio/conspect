// Mesh (/settings/mesh) — local-mesh discovery + relay (ADR-0051).
//
// DISCOVERY is a LOCKED row: always-on mDNS announce/browse, no off switch. The
// panel discloses what is announced (a signed, salted digest summary — never raw
// identity). RELAY is a real opt-in toggle wired to PUT /api/v1/mesh/relay: a
// willing online machine relays an offline neighbour's heartbeat as a dumb,
// end-to-end-signed carrier. The computed role (direct/relay/leaf) and, for a
// leaf, the peer it leafs through, are shown. The nearby-peers panel is the
// read-only untrusted inventory (peers are never auto-trusted). When the machine
// is isolated, it points at the offline challenge export on the Licence screen.
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Link } from "react-router-dom";

import type { MeshStatusDoc } from "../api/conspectQueries";
import { useMeshPeers, useMeshStatus, useSetRelay } from "../api/conspectQueries";
import { Switch } from "../components/account/Switch";
import { PageHeader } from "../components/PageHeader";
import { Badge } from "../components/ui/badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
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

/** A short, stable label for the computed mesh role. */
function roleLabel(role: MeshStatusDoc["role"]): string {
  return role.kind;
}

/** The always-on discovery panel (LOCKED — no toggle). */
function DiscoveryPanel(): JSX.Element {
  return (
    <Card data-testid="discovery-panel">
      <CardHeader>
        <CardTitle>
          <Trans>Discovery</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            This machine announces and browses for neighbours on the local network
            so an offline machine can have its heartbeat relayed. Discovery is part
            of running the account plane — there is no off switch.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex items-center justify-between rounded-md border bg-muted/40 px-3 py-2">
          <span className="text-sm font-medium">
            <Trans>Status</Trans>
          </span>
          <span className="text-sm font-semibold">
            <Trans>Always on</Trans>
          </span>
        </div>

        <div>
          <p className="mb-1 text-sm font-medium">
            <Trans>What is announced</Trans>
          </p>
          <p className="mb-2 text-sm text-muted-foreground">
            <Trans>
              A signed summary only — salted digests and a signed entitlement
              summary, never raw identity. The exhaustive list:
            </Trans>
          </p>
          <ul
            data-testid="discovery-announce-list"
            className="flex flex-wrap gap-1.5"
          >
            {[
              "salted_fingerprint_digest",
              "signed_entitlement_summary",
              "enforcement_level",
              "lease_bounds",
            ].map((field) => (
              <li key={field}>
                <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                  {field}
                </code>
              </li>
            ))}
          </ul>
        </div>
      </CardContent>
    </Card>
  );
}

/** The relay panel: a real opt-in toggle + the computed role/via. */
function RelayPanel(): JSX.Element {
  const { t } = useLingui();
  const status = useMeshStatus();
  const setRelay = useSetRelay();

  const relayEnabled = status.data?.relay_enabled ?? false;

  const onToggle = (next: boolean): void => {
    setRelay.mutate(next, {
      onError: (error) => {
        toast({
          title: t`Could not update relay setting`,
          description: error.message,
          variant: "destructive",
        });
      },
    });
  };

  return (
    <Card data-testid="relay-panel">
      <CardHeader>
        <CardTitle>
          <Trans>Relay</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            When on, this machine relays an offline neighbour's licensing heartbeat
            to the server and carries the signed response back. The payload is
            end-to-end signed — this machine cannot read, forge, or alter it, and
            earns no entitlement from carrying it.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex items-center justify-between gap-4 rounded-md border px-3 py-2">
          <div className="min-w-0">
            <p className="text-sm font-medium">
              <Trans>Relay neighbours' heartbeats</Trans>
            </p>
            <p className="text-sm text-muted-foreground">
              {relayEnabled ? <Trans>On</Trans> : <Trans>Off</Trans>}
            </p>
          </div>
          <Switch
            label={t`Relay neighbours' heartbeats`}
            checked={relayEnabled}
            disabled={status.isPending || setRelay.isPending}
            onToggle={onToggle}
          />
        </div>

        {status.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load the mesh status:</Trans> {status.error.message}
          </p>
        ) : status.data !== undefined ? (
          <dl className="divide-y text-sm" data-testid="mesh-role">
            <div className="flex items-baseline justify-between gap-2 py-1.5">
              <dt className="text-muted-foreground">
                <Trans>Role</Trans>
              </dt>
              <dd className="font-mono font-medium">{roleLabel(status.data.role)}</dd>
            </div>
            {status.data.role.kind === "leaf" ? (
              <div className="flex items-baseline justify-between gap-2 py-1.5">
                <dt className="text-muted-foreground">
                  <Trans>Leafing through</Trans>
                </dt>
                <dd className="truncate font-mono text-xs">{status.data.role.via}</dd>
              </div>
            ) : null}
            <div className="flex items-baseline justify-between gap-2 py-1.5">
              <dt className="text-muted-foreground">
                <Trans>Peers discovered</Trans>
              </dt>
              <dd className="font-medium tabular-nums">{String(status.data.peers_count)}</dd>
            </div>
          </dl>
        ) : null}

        <p className="rounded-md border bg-muted/40 p-3 text-sm text-muted-foreground">
          <Trans>
            If this machine is isolated — no internet path and no relaying
            neighbour — you can still license it offline:
          </Trans>{" "}
          <Link
            to="/settings/licence#challenge"
            className="underline underline-offset-2"
          >
            <Trans>export a challenge</Trans>
          </Link>{" "}
          <Trans>and install the signed lease by hand.</Trans>
        </p>
      </CardContent>
    </Card>
  );
}

/** The read-only untrusted discovered-peer inventory. */
function PeersPanel(): JSX.Element {
  const { t } = useLingui();
  const peers = useMeshPeers();
  const rows = peers.data ?? [];

  return (
    <Card data-testid="peers-panel">
      <CardHeader>
        <CardTitle>
          <Trans>Nearby peers</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Machines discovered on the local network. This inventory is untrusted
            and read-only — a peer is never auto-trusted.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent>
        {peers.isPending ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Loading peers…</Trans>
          </p>
        ) : peers.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load peers:</Trans> {peers.error.message}
          </p>
        ) : rows.length === 0 ? (
          <div className="rounded-md border border-dashed p-8 text-center">
            <p className="text-sm text-muted-foreground">
              <Trans>No peers discovered.</Trans>
            </p>
          </div>
        ) : (
          <Table>
            <TableCaption>{t`Discovered peers (untrusted, read-only).`}</TableCaption>
            <TableHeader>
              <TableRow>
                <TableHead>{t`Peer`}</TableHead>
                <TableHead>{t`Claimed`}</TableHead>
                <TableHead>{t`Relaying for us`}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((peer) => (
                <TableRow key={peer.key}>
                  <TableCell>
                    <code className="font-mono text-xs">
                      {peer.name ?? `${peer.key.slice(0, 12)}…`}
                    </code>
                  </TableCell>
                  <TableCell>
                    {peer.claimed ? <Trans>Yes</Trans> : <Trans>No</Trans>}
                  </TableCell>
                  <TableCell>
                    {peer.relaying_for_us ? (
                      <Badge variant="live">
                        <Trans>Yes</Trans>
                      </Badge>
                    ) : (
                      <Trans>No</Trans>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </CardContent>
    </Card>
  );
}

/** The Mesh screen. */
export function MeshPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>Mesh</Trans>}
        description={
          <Trans>
            The local mesh: always-on discovery, and an opt-in relay that lets an
            offline machine reach the licence server through a neighbour.
          </Trans>
        }
      />

      <div className="grid gap-4 lg:grid-cols-2">
        <DiscoveryPanel />
        <RelayPanel />
        <div className="lg:col-span-2">
          <PeersPanel />
        </div>
      </div>
    </>
  );
}
