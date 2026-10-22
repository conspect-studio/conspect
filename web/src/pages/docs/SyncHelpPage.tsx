// In-app guide: synchronized output and the honest tier ladder
// (managed-devices.md §8, ADR-M010). Section ids are part of the public
// anchor contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../components/PageHeader";
import { HelpLink } from "../../components/HelpLink";
import { Code, DocDefinitions, DocSection, DocTerm, Prose } from "./components";

/** Synchronized-output guide. */
export function SyncHelpPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Synchronized output</Trans>}
        description={
          <Trans>
            What "in sync" honestly means per device class, and how sync
            groups measure — never assume — their achieved tier.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="sync-tiers" title={<Trans>The tier ladder</Trans>}>
          <Prose>
            <Trans>
              Different hardware can only achieve different sync quality, and
              Multiview reports the tier it MEASURES rather than the one a
              datasheet promises:
            </Trans>
          </Prose>
          <DocDefinitions>
            <DocTerm term={<Trans>Same machine, multiple outputs</Trans>}>
              <Trans>
                Frame-accurate with sub-millisecond flip deltas — all heads
                are driven from one atomic commit.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>Display nodes</Trans>}>
              <Trans>
                Frame-accurate: every node presents the same frame index,
                with at most a fraction of one refresh of phase residual.
                This is the tier for video walls.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>Vendor decoders (ZowieBox-class)</Trans>}>
              <Trans>
                Bounded skew: the decoder runs its own clock, so its
                presentation drifts within roughly ±100–500 ms between
                re-aligns. An optional scheduled re-align tightens it but
                blanks that device for one to three seconds — never silently,
                never mid-show without the policy enabled.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>Cast targets</Trans>}>
              <Trans>
                Not synchronized: cast pipelines buffer for multiple seconds
                under their own control. Cast devices are therefore never
                offered as sync-group members.
              </Trans>
            </DocTerm>
          </DocDefinitions>
        </DocSection>

        <DocSection id="sync-groups" title={<Trans>Sync groups</Trans>}>
          <Prose>
            <Trans>
              A sync group names the devices that should present together,
              with a per-member <Code>offset_ms</Code> trim (a fixed
              presentation delay for path-length differences) and a{" "}
              <Code>target_skew_ms</Code> drift threshold. The group claims
              the weakest member's tier — adding one vendor decoder to a wall
              of display nodes makes the whole group bounded-skew, and the UI
              says so rather than over-claiming. A member drifting past the
              target raises a warning alarm; "Measure" runs an on-demand skew
              measurement whose result arrives on the realtime stream.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="sync-honesty" title={<Trans>Measured, never assumed</Trans>}>
          <Prose>
            <Trans>
              The achieved tier shown on devices and groups comes from
              measurements (and is absent until one exists). Program output
              timing is never paced by any device: sync alignment is applied
              at the presentation edge, and a failed or drifting member can
              never stall the multiview itself.
            </Trans>
          </Prose>
          <HelpLink to="/help/display-nodes" label={t`About display nodes`} />
        </DocSection>
      </div>
    </>
  );
}
