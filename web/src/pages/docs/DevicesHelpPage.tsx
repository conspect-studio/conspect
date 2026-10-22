// In-app guide: managed devices (managed-devices.md). Section ids are part of
// the public anchor contract (see src/docs/registry.ts).
//
// Wording doctrine: vendor support is stated as "Supports <class>" from our
// own integration work against public/observed interfaces — never an
// endorsement claim and never the word "Official" (CODE_OF_CONDUCT.md;
// vendor-neutral docs).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../components/PageHeader";
import { HelpLink } from "../../components/HelpLink";
import { Code, DocDefinitions, DocSection, DocTerm, Prose } from "./components";

/** Managed-devices guide. */
export function DevicesHelpPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Managed devices</Trans>}
        description={
          <Trans>
            Adopt the hardware around the multiview — decoders, display nodes,
            cast targets — and manage it as declarative desired state.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="what-devices-are" title={<Trans>What a managed device is</Trans>}>
          <Prose>
            <Trans>
              A managed device is a piece of hardware Multiview supervises: a
              hardware decoder feeding a TV, a display node driving a video
              wall, or a cast target. The device entry you adopt is desired
              state — driver, management address, mode, credentials — and a
              supervised driver continuously converges the real device onto
              it: it probes, reconnects, re-applies the mode, and reports
              status.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Devices never carry the program: Multiview's program output
              never depends on any device. If a device fails, sources bound to
              it ride the normal tile ladder and outputs ride their failover
              policy — the multiview keeps running while the driver works on
              recovery.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="device-states" title={<Trans>Lifecycle states</Trans>}>
          <DocDefinitions>
            <DocTerm term={<Trans>DISCOVERED</Trans>}>
              <Trans>
                Present only in the untrusted discovery inventory — not a
                device yet. Adoption is always an explicit operator action.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>ADOPTING</Trans>}>
              <Trans>The record was created and the first probe is in flight.</Trans>
            </DocTerm>
            <DocTerm term={<Trans>ONLINE / DEGRADED</Trans>}>
              <Trans>
                Reachable. DEGRADED means the device itself reports a fault —
                a stalled decode, over-temperature — while its management
                channel still answers. Program output is unaffected either
                way.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>AUTH_FAILED</Trans>}>
              <Trans>
                The device rejected its credentials. Distinct from
                UNREACHABLE: there are no blind retries — probing pauses until
                the credentials secret reference is updated, then resumes
                automatically.
              </Trans>
            </DocTerm>
            <DocTerm term={<Trans>UNREACHABLE</Trans>}>
              <Trans>
                The device stopped answering. The control plane reconnects
                with backoff and jitter (exactly like input reconnect), raises
                the configured offline alarm after a dwell, and re-converges
                the desired state when the device returns. "Probe now"
                overrides a parked backoff.
              </Trans>
            </DocTerm>
          </DocDefinitions>
        </DocSection>

        <DocSection id="drivers" title={<Trans>Drivers and what they support</Trans>}>
          <Prose>
            <Trans>
              Drivers are compiled in; each maps its device family onto the
              same capability shape (encode, decode, display, sync, audio,
              reboot, firmware update). Support statements below describe our
              own integration against publicly observable interfaces — they
              are not vendor endorsements.
            </Trans>
          </Prose>
          <DocDefinitions>
            <DocTerm term={<Code>zowietek</Code>}>
              <Trans>
                Supports ZowieBox-class encoder/decoder appliances: probing,
                encoder/decoder mode convergence, stream enumeration for
                source binding, decode-slot binding for outputs, reboot, and
                temperature. Sync is bounded-skew only (see the sync guide).
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>displaynode</Code>}>
              <Trans>
                Multiview's own display-node software for single-board
                computers and spare PCs driving displays; enrolled nodes are
                located by their keypair identity rather than an address, and
                present frame-accurate synchronized output.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>cast</Code>}>
              <Trans>
                Supports casting a stream to media renderers on the network
                (ad-hoc, best-effort). Cast targets are never part of a
                synchronized canvas and ad-hoc sessions never raise major
                alarms.
              </Trans>
            </DocTerm>
          </DocDefinitions>
          <Prose>
            <Trans>
              ZowieTek, ZowieBox and related marks are trademarks of Zowietek
              Electronics, Ltd. This project is independent and not affiliated
              with or endorsed by Zowietek.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="binding-streams" title={<Trans>Binding device streams</Trans>}>
          <Prose>
            <Trans>
              A device's streams become ordinary managed Sources, and its
              decode slots become ordinary managed Outputs, each carrying a{" "}
              <Code>device_ref</Code> annotation in its body. The engine's
              ingest and serving paths are untouched — the binding only tells
              the driver which device the resource belongs to (and blocks
              deleting the device while something still references it). Where
              a vendor does not document a stream URL, the UI flags the
              candidate as unverified and you supply the URL yourself — it is
              never silently guessed.
            </Trans>
          </Prose>
          <HelpLink to="/help/devices/adopt" label={t`How to adopt a device`} />
        </DocSection>
      </div>
    </>
  );
}
