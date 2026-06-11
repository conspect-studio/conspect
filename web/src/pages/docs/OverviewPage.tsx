// Docs: overview / getting started.
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Link } from "react-router-dom";

import { PageHeader } from "../../components/PageHeader";
import { Code, CodeBlock, DocList, DocSection, Prose } from "./components";
import { DocsSearch } from "./DocsSearch";
import { DOCS_NAV } from "./docsNav";

/** The documentation landing page. */
export function OverviewPage(): JSX.Element {
  const { i18n } = useLingui();
  // The landing page links onward to every other section (skip its own entry).
  const onward = DOCS_NAV.filter((item) => item.path !== "/help");
  return (
    <>
      <PageHeader
        title={<Trans>Documentation</Trans>}
        description={
          <Trans>
            How Multiview works, how to run it, and how to drive it from
            config-as-code or the API.
          </Trans>
        }
      />

      <div className="mb-4 max-w-md">
        <DocsSearch />
      </div>

      <div className="space-y-4">
        <DocSection id="what-multiview-is" title={<Trans>What Multiview is</Trans>}>
          <Prose>
            <Trans>
              Multiview is a hardware-accelerated live video multiview generator.
              It ingests many live sources, composites them into a templated grid
              on the GPU, and serves the combined picture as a continuous output
              stream. It is designed for monitoring walls and production
              multiviewers where the output must never stall.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Inputs are sampled, never trusted to pace the output. A single
              fixed-cadence clock emits exactly one frame per tick. When a source
              drops, its tile holds its last good frame and shows a clear state
              while the engine reconnects in the background — the rest of the wall
              keeps running.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="design-pillars" title={<Trans>Design pillars</Trans>}>
          <DocList>
            <li>
              <Trans>
                <strong>Continuous output.</strong> One monotonic output clock
                produces a valid, correctly-timestamped frame every tick,
                independent of any input.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>Last-good-frame resilience.</strong> Each tile rides a
                live / stale / reconnecting / no-signal state machine; a failing
                input cannot block the compositor.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>Composite once, fan out many.</strong> The canvas is
                encoded once per rendition and the same packets feed every
                transport, rather than re-encoding per destination.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>Control never back-pressures the engine.</strong> The
                management API, realtime streams, and preview are best-effort and
                physically incapable of stalling the output path.
              </Trans>
            </li>
          </DocList>
        </DocSection>

        <DocSection id="getting-started" title={<Trans>Getting started</Trans>}>
          <Prose>
            <Trans>
              The fastest way to see Multiview running is the quick-start compose
              stack. It publishes a synthetic test feed, composites a 2x2 canvas,
              and serves the result over HTTP so you can open it in a player — no
              real cameras or private feeds required.
            </Trans>
          </Prose>
          <CodeBlock label="Shell command">
            {`docker compose -f deploy/compose.yaml up -d
# then open the HLS playlist in a player:
#   http://[::1]:8888/multiview.m3u8`}
          </CodeBlock>
          <Prose>
            <Trans>
              You can also run the binary directly against a config file with{" "}
              <Code>multiview run path/to/multiview.toml</Code>. See the sections
              below for the container, compose, config, and API details.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="where-next" title={<Trans>Where to go next</Trans>}>
          <ul className="space-y-2">
            {onward.map(({ path, title, summary }) => (
              <li key={path}>
                <Link
                  to={path}
                  className="font-medium text-foreground underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                >
                  {i18n._(title)}
                </Link>
                <span className="block text-muted-foreground">
                  {i18n._(summary)}
                </span>
              </li>
            ))}
          </ul>
        </DocSection>
      </div>
    </>
  );
}
