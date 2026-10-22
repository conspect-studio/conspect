// In-app guide: display nodes (display-out brief; ADR-0044/0045). Section ids
// are part of the public anchor contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../components/PageHeader";
import { HelpLink } from "../../components/HelpLink";
import { Code, DocSection, Prose } from "./components";

/** Display-nodes guide. */
export function DisplayNodesHelpPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Display nodes</Trans>}
        description={
          <Trans>
            Multiview's own playout endpoints: small computers that turn
            ordinary displays into managed, frame-accurate wall heads.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="display-node-model" title={<Trans>What a display node is</Trans>}>
          <Prose>
            <Trans>
              A display node is a single-board computer or spare PC running
              Multiview's node software, driving one or more physical
              displays. Nodes are adopted as managed devices with the{" "}
              <Code>displaynode</Code> driver and present the program, a
              declared output, or one head of a video wall. Because the node
              software is ours end-to-end, display nodes reach the
              frame-accurate sync tier — every head of a wall shows the same
              frame index.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="display-node-enrollment" title={<Trans>Enrollment</Trans>}>
          <Prose>
            <Trans>
              Nodes enrol with a keypair identity: the node proves itself with
              its key, not with a network address, so a node keeps its
              identity across DHCP changes and re-images. That is why the
              management address is optional for the{" "}
              <Code>displaynode</Code> driver — an enrolled node finds the
              controller and authenticates itself. After a network loss the
              node reconnects with its enrolled identity, re-pulls its
              stream, and re-converges its sync offset before un-muting its
              output.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="display-node-resilience" title={<Trans>Resilience</Trans>}>
          <Prose>
            <Trans>
              A node inherits the product's output doctrine: if its feed
              stops, it holds the last good frame, then shows its local slate
              — it never blanks mid-show because of a network blip, and the
              multiview program itself is never affected by a node's fate.
            </Trans>
          </Prose>
          <HelpLink to="/help/sync" label={t`About sync tiers`} />
        </DocSection>
      </div>
    </>
  );
}
