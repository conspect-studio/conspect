// Concept article: resilience & the tile lifecycle (ADR-W016). Section ids
// are part of the public anchor contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../../components/PageHeader";
import { HelpLink } from "../../../components/HelpLink";
import { DocDefinitions, DocSection, DocTerm, Prose } from "../components";

/** Resilience & tile lifecycle concept article. */
export function ResiliencePage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Resilience & the tile lifecycle</Trans>}
        description={
          <Trans>
            What the LIVE, STALE, RECONNECTING, and NO SIGNAL badges mean, and
            why the output never stalls on a bad input.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="tile-lifecycle" title={<Trans>The tile lifecycle</Trans>}>
          <Prose>
            <Trans>
              Every tile rides its own four-state ladder, independently of
              every other tile. The state is shown as a badge in the UI and
              decides what the tile draws:
            </Trans>
          </Prose>
          <DocDefinitions>
            <DocTerm term={<Trans>LIVE</Trans>}>
              <Trans>
                Fresh frames are arriving; the tile shows the newest one. This
                is the normal state.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>STALE</Trans>}>
              <Trans>
                No fresh frame has arrived for a short hold window. The tile
                keeps showing its last good frame — visually nothing changes
                yet, but the engine has noticed.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>RECONNECTING</Trans>}>
              <Trans>
                The source has been quiet long enough that the engine is
                actively re-establishing it. The tile still shows the last
                good frame, now with a reconnect indicator, so an operator can
                tell a frozen feed from a live one.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>NO SIGNAL</Trans>}>
              <Trans>
                Recovery has not succeeded within the no-signal timeout. The
                tile switches to a clear "signal lost" slate — an honest
                statement that what is on screen is no longer recent video.
              </Trans>
            </DocTerm>
          </DocDefinitions>
          <Prose>
            <Trans>
              The moment a fresh frame arrives — from any state — the tile
              snaps straight back to LIVE. The hold, stale, and no-signal
              windows are automatic: the engine uses built-in,
              broadcast-tuned values and derives its stream-liveness checks
              from each source's own frame cadence, so a low-frame-rate
              source is not misread as stalled. There is nothing for an
              operator to configure.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="last-good-frame" title={<Trans>Last-good frames</Trans>}>
          <Prose>
            <Trans>
              Each input continuously writes its newest decoded frame into a
              small per-tile store; the compositor reads the latest complete
              frame from that store at every output tick and never waits for
              an input. When a source hiccups, the store simply keeps handing
              out the most recent frame it has — the "last good" one. A brief
              network blip is therefore invisible: the tile holds a
              fraction-of-a-second-old picture until fresh frames resume.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              This is also why one bad source can never take down the wall:
              the disturbance is contained to that tile's rectangle. The
              output keeps emitting a complete, valid frame every tick
              regardless — even if every source died at once, the canvas
              would show slates and a still-ticking clock, not stop.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="reconnect" title={<Trans>Reconnect behaviour</Trans>}>
          <Prose>
            <Trans>
              Source connections are supervised. A stalled read times out
              rather than hanging forever; the supervisor then reconnects with
              exponential backoff (so a dead camera is not hammered) and keeps
              trying indefinitely — sources recover without operator action
              whenever the far end comes back. Mid-stream surprises that
              normally break players, like a camera changing resolution or a
              stream resetting its timestamps, are absorbed inside the tile:
              the decoder re-initializes behind the frame store while the
              compositor keeps drawing the last good frame.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Glitchy, corrupt, and malformed inputs are the expected
              workload, not an edge case: decode errors on one stream are
              tolerated and recovered per tile, and a broken track inside a
              transport (for example a damaged subtitle rendition) is warned
              about and recovered without killing the streams alongside it.
            </Trans>
          </Prose>
          <HelpLink
            to="/help/concepts/timing-sync#output-clock"
            label={t`Why the output clock never depends on inputs`}
          />
        </DocSection>
      </div>
    </>
  );
}
