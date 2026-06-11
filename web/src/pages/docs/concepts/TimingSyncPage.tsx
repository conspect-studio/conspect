// Concept article: timing & sync — the output clock, genlock, PTP, and
// wall-clock time (ADR-W016). Section ids are part of the public anchor
// contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../../components/PageHeader";
import { HelpLink } from "../../../components/HelpLink";
import { Code, DocList, DocSection, Prose } from "../components";

/** Timing & sync concept article. */
export function TimingSyncPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Timing & sync</Trans>}
        description={
          <Trans>
            How Multiview paces its output, what genlock and PTP mean, and how
            wall-clock time fits in.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="output-clock" title={<Trans>The output clock</Trans>}>
          <Prose>
            <Trans>
              Multiview's output is paced by exactly one thing: a fixed-cadence
              internal clock. At every tick — for example every 1/50 s for a
              50 fps canvas — it emits one finished frame, forever. Sources
              never set that pace. Each input writes its newest decoded frame
              into a per-tile store, and at each tick the compositor{" "}
              <em>samples</em> whatever is freshest there. A source that
              bursts, stalls, or dies changes only what its own tile shows —
              never when the next output frame goes out.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              This is why the wall keeps running through any input failure:
              the output's timing is independent of every input by
              construction. Frame rates are handled as exact fractions (NTSC
              29.97 is <Code>30000/1001</Code>, never a rounded decimal), so
              the cadence cannot drift over long runs.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="genlock" title={<Trans>Genlock</Trans>}>
          <Prose>
            <Trans>
              Genlock is the broadcast practice of locking every device's
              frame timing to one shared reference signal — historically an
              analog "black burst" or tri-level sync feed distributed around
              the facility. When everything is genlocked, every camera and
              every output starts its frames at the same instant, so video can
              be switched and mixed without glitches or re-buffering.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              The key idea to keep: a <em>reference</em> is trusted timing
              infrastructure that carries only rate and phase — it is not a
              video source. Facilities lock outputs to a reference and then
              frame-align sources to those outputs; nobody lets program
              content drive a device's clock, because content can stall or
              lie. Multiview follows the same discipline in software: its
              output cadence can be disciplined by a reference, but never by a
              source.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="ptp" title={<Trans>PTP (Precision Time Protocol)</Trans>}>
          <Prose>
            <Trans>
              PTP (IEEE 1588) is genlock's IP-network successor. A
              "grandmaster" clock distributes time over ordinary Ethernet, and
              every device on the network disciplines its own clock to it —
              typically to within a microsecond. The broadcast profile, SMPTE
              ST 2059, adds the convention that frame boundaries are
              computable from the time itself, so two PTP-locked devices
              agree not just on the time of day but on exactly when each
              frame starts. This single mechanism replaces both the genlock
              signal (phase) and timecode distribution (labels) in an
              IP facility, and is the timing backbone of SMPTE ST 2110
              installations.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Frame-accurate facilities use PTP because it is the only way to
              make many independent devices agree on "now" tightly enough to
              cut between them mid-frame. Two cameras of the same event locked
              to the same grandmaster produce frames that genuinely correspond
              in time — something no amount of buffering can recreate after
              the fact.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              How Multiview relates: the fixed-cadence output clock free-runs
              on the host's monotonic clock by default — no timing hardware
              needed. In a PTP facility the same clock can be{" "}
              <em>disciplined</em> to the grandmaster (its rate gently steered,
              never stepped), and if the reference disappears the output
              coasts and then free-runs — it never stops. Inputs remain
              sampled, never pacing, in every mode; PTP changes how well
              sources line up with each other, not who controls the output.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="wall-clock" title={<Trans>Wall-clock time</Trans>}>
          <Prose>
            <Trans>
              Wall-clock time is ordinary time-of-day (UTC), kept by the
              host's system clock and disciplined by NTP or PTP. Multiview
              uses it for <em>labels only</em>: on-screen clock overlays, log
              timestamps, and the date-time stamps in HLS playlists. It never
              paces frames, because a system clock can step backwards or jump
              when corrected — exactly what an output clock must never do.
            </Trans>
          </Prose>
          <DocList>
            <li>
              <Trans>
                <strong>Output clock</strong> — decides <em>when</em> each
                frame is emitted (monotonic, fixed cadence).
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>Wall clock</strong> — says <em>what time of day</em>{" "}
                it is (for clocks and stream metadata).
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>Timecode</strong> — a per-frame <em>label</em>{" "}
                (HH:MM:SS:FF). Two feeds can carry identical timecode and
                still be out of phase — which is why genlock/PTP exist
                alongside it.
              </Trans>
            </li>
          </DocList>
          <HelpLink
            to="/help/concepts/glossary#ptp"
            label={t`Glossary: PTP`}
          />
        </DocSection>
      </div>
    </>
  );
}
