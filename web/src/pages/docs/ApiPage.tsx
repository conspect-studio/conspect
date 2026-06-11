// Docs: API & realtime. Links out to the backend Scalar playground at /docs and
// explains the REST + realtime contract the SPA itself consumes.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import { ExternalLink } from "lucide-react";

import { PageHeader } from "../../components/PageHeader";
import { Button } from "../../components/ui/button";
import {
  Code,
  DocDefinitions,
  DocList,
  DocSection,
  DocTerm,
  Prose,
  StatusBadge,
} from "./components";

/** API & realtime documentation. */
export function ApiPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>API & realtime</Trans>}
        description={
          <Trans>
            The management API, its conventions, the realtime event streams, and
            where to find the live, interactive reference.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="playground" title={<Trans>Live API playground</Trans>}>
          <Prose>
            <Trans>
              The control plane serves an interactive OpenAPI reference at the
              backend <Code>/docs</Code> path. It is generated from the same spec
              the UI client is generated from, so it always matches the running
              server. Open it to browse every endpoint, see schemas, and try
              requests.
            </Trans>
          </Prose>
          {/* An absolute path link to the backend route (outside the SPA
              router), opened in a new tab. This is why the in-app guide lives
              under /help, not /docs. */}
          <Button asChild variant="outline">
            <a href="/docs" target="_blank" rel="noreferrer">
              <ExternalLink className="size-4" aria-hidden="true" />
              <Trans>Open the API playground</Trans>
              <span className="sr-only">
                <Trans>(opens in a new tab)</Trans>
              </span>
            </a>
          </Button>
        </DocSection>

        <DocSection id="rest-conventions" title={<Trans>REST conventions</Trans>}>
          <DocDefinitions>
            <DocTerm term={<Trans>Base path</Trans>}>
              <Trans>
                All endpoints live under <Code>/api/v1</Code>.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>Long-running ops</Trans>}>
              <Trans>
                Actions that take time return <Code>202 Accepted</Code> with an
                operation id; the final result arrives on the realtime stream, not
                in the HTTP response body.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>Optimistic concurrency</Trans>}>
              <Trans>
                Mutable resources carry an <Code>ETag</Code>. Send it back as{" "}
                <Code>If-Match</Code> on a write; a stale tag is rejected with{" "}
                <Code>412 Precondition Failed</Code> so two editors never silently
                clobber each other.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>Idempotency</Trans>}>
              <Trans>
                Start, stop, and swap actions accept an <Code>Idempotency-Key</Code>{" "}
                so a retried request is applied at most once.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>Errors</Trans>}>
              <Trans>
                Failures use RFC 9457 problem documents (
                <Code>application/problem+json</Code>) with a machine-readable
                type and a human-readable detail.
              </Trans>
            </DocTerm>
          </DocDefinitions>
        </DocSection>

        <DocSection id="realtime-streams" title={<Trans>Realtime streams</Trans>}>
          <Prose>
            <Trans>
              The UI subscribes to engine events for tile states, operation
              results, alarms, and telemetry. These streams are strictly
              best-effort: they can drop or conflate events under load and are
              physically incapable of slowing the engine, so a client must
              tolerate gaps and reconcile against the REST state.
            </Trans>
          </Prose>
          <DocList>
            <li>
              <Trans>
                <strong>WebSocket</strong> (primary) at <Code>/api/v1/ws</Code> —
                bidirectional, used for the live UI.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>Server-Sent Events</strong> (fallback) at{" "}
                <Code>/api/v1/events</Code> — one-way, for environments where a
                WebSocket cannot be used.
              </Trans>
            </li>
          </DocList>
        </DocSection>

        <DocSection
          id="authentication"
          title={
            <span className="inline-flex items-center gap-2">
              <Trans>Authentication</Trans>
              <StatusBadge status="available" />
            </span>
          }
        >
          <Prose>
            <Trans>
              The API authenticates each request with a bearer token in the{" "}
              <Code>Authorization</Code> header. A coarse role (admin, operator, or
              viewer) gates what actions a token may take, and a per-object check
              confines a token to the resources it owns.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              In this UI, set your token under Settings, then API access. For a
              container, supply it as the <Code>MULTIVIEW_CONTROL_TOKEN</Code>{" "}
              environment variable — the presented token is{" "}
              <Code>admin.&lt;secret&gt;</Code>. When that variable is unset, the
              server generates a token and logs it once at startup. The
              unauthenticated surface is just <Code>/</Code>, <Code>/docs</Code>,
              and <Code>/api/v1/openapi.json</Code>; every <Code>/api/v1</Code>{" "}
              data route requires the token.
            </Trans>
          </Prose>
        </DocSection>
      </div>
    </>
  );
}
