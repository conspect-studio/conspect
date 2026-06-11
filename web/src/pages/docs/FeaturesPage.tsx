// Docs: feature guide. Explains the multiviewer concepts and is honest about
// which output paths are available versus on the roadmap.
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { HelpLink } from "../../components/HelpLink";
import { PageHeader } from "../../components/PageHeader";
import { Code, DocList, DocSection, Prose, StatusBadge } from "./components";

/** Feature guide documentation. */
export function FeaturesPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Feature guide</Trans>}
        description={
          <Trans>
            The building blocks of a Multiview deployment, and what each one does.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="layouts" title={<Trans>Layouts</Trans>}>
          <Prose>
            <Trans>
              A layout decides where every tile sits on the canvas. Use a factory
              preset (such as 2x2 or 1+5), a CSS-grid with named areas and
              fractional tracks, or absolute normalized rectangles for full
              control. A cell's fit mode controls how a source is scaled into its
              tile — contain to letterbox, cover to crop and fill, or fill to
              stretch.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="sources" title={<Trans>Sources</Trans>}>
          <Prose>
            <Trans>
              A source is a managed input. Each one owns its ingest, decode, color
              handling, and reconnect behaviour, and is referenced by tiles via a
              stable id. Supported kinds include RTSP, HLS, MPEG-TS, SRT, RTMP,
              NDI, file, and a built-in synthetic test pattern. When a source
              stops delivering frames, its tile holds the last good frame and
              moves through a clear state machine — live, stale, reconnecting,
              no-signal — while the engine reconnects without disturbing the rest
              of the wall.
            </Trans>
          </Prose>
          <HelpLink
            to="/help/concepts/transports#choosing"
            label={t`About source transports`}
          />
          <HelpLink
            to="/help/concepts/resilience#tile-lifecycle"
            label={t`About the tile lifecycle`}
          />
        </DocSection>

        <DocSection
          id="outputs"
          title={
            <span className="inline-flex items-center gap-2">
              <Trans>Outputs</Trans>
            </span>
          }
        >
          <Prose>
            <Trans>
              The composited canvas is encoded once per rendition and the same
              packets are fanned out to every output. Be honest about the current
              state of each transport:
            </Trans>
          </Prose>
          <DocList>
            <li className="flex flex-wrap items-baseline gap-2">
              <StatusBadge status="available" />
              <span>
                <Trans>
                  <strong>HLS / file output.</strong> Writes a playlist and
                  segments to disk. This is the path the quick-start uses and that
                  works today.
                </Trans>
              </span>
            </li>
            <li className="flex flex-wrap items-baseline gap-2">
              <StatusBadge status="roadmap" />
              <span>
                <Trans>
                  <strong>Low-latency HLS.</strong> Partial-segment HLS for lower
                  glass-to-glass latency.
                </Trans>
              </span>
            </li>
            <li className="flex flex-wrap items-baseline gap-2">
              <StatusBadge status="roadmap" />
              <span>
                <Trans>
                  <strong>Live RTSP / NDI / RTMP / SRT servers and push.</strong>{" "}
                  The network output servers are not yet wired into the run
                  command.
                </Trans>
              </span>
            </li>
          </DocList>
          <HelpLink
            to="/help/concepts/codecs#encode-once"
            label={t`About the encode-once output model`}
          />
        </DocSection>

        <DocSection id="overlays" title={<Trans>Overlays</Trans>}>
          <Prose>
            <Trans>
              Overlays draw on top of the canvas or a single tile: clocks,
              text labels and under-monitor displays, and tally borders. A clock
              overlay sourced from the wall clock keeps ticking every frame, which
              doubles as a visible sentinel that the output is still alive.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="tally" title={<Trans>Tally</Trans>}>
          <Prose>
            <Trans>
              Tally shows which sources are on-air. A tally profile maps the bits
              of an external tally bus to colors (for example red for program,
              green for preview) and maps bus index positions to specific tiles. A{" "}
              <Code>tally_border</Code> overlay then lights the tile's edge to
              match. This lets an upstream router or mixer drive the borders on the
              wall.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="salvos" title={<Trans>Salvos</Trans>}>
          <Prose>
            <Trans>
              A salvo is a named, atomically-applied recall. One salvo can switch
              the layout, rebind sources into cells, set tally colors, and update
              under-monitor display text — all at once — so an operator can move a
              whole arrangement (say, a VTR review setup) with a single action
              instead of a series of separate edits.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="alarms" title={<Trans>Alarms</Trans>}>
          <Prose>
            <Trans>
              Alarms are raised by content-aware fault probes attached to a tile.
              Probes can detect a black picture, a frozen picture, audio silence,
              and loudness that drifts from an EBU R128 target. Each probe has a
              severity and dwell timers, so a brief glitch does not trip an alarm
              while a sustained fault does. Severities follow the standard
              cleared / indeterminate / warning / minor / major / critical scale.
            </Trans>
          </Prose>
        </DocSection>
      </div>
    </>
  );
}
