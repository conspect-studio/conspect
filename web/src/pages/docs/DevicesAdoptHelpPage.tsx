// In-app guide: discovering and adopting devices (ADR-0041 untrusted
// discovery + explicit confirm-adopt). Section ids are part of the public
// anchor contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../components/PageHeader";
import { HelpLink } from "../../components/HelpLink";
import { Code, DocSection, Prose } from "./components";

/** Adopting-devices guide. */
export function DevicesAdoptHelpPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Adopting devices</Trans>}
        description={
          <Trans>
            Find devices with a network scan, then adopt them with an explicit
            confirmation — discovery alone never changes anything.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="untrusted-discovery" title={<Trans>Discovery is untrusted</Trans>}>
          <Prose>
            <Trans>
              A discovery scan browses the network (mDNS/DNS-SD) for a bounded
              few seconds and lists what answered. Anything on the network can
              answer a broadcast, so the results are an untrusted inventory of
              hints: they are not devices, they carry no credentials, and
              Multiview never adopts them automatically. Every adoption is an
              explicit operator confirmation — the row's Adopt button only
              prefills the dialog; nothing exists until you confirm it.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Addresses are presented IPv6-first; IPv4 results are explicitly
              labelled legacy. Scans are single-flight and rate-limited — a
              scan requested while one runs attaches to the running scan.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="adopt-steps" title={<Trans>Adopting step by step</Trans>}>
          <Prose>
            <Trans>
              1. On the Devices page, run "Scan for devices" (or skip the scan
              and adopt by address directly). 2. Press Adopt on a discovered
              row, or "Adopt device" in the header. 3. Give the device a
              stable identifier and name, check the driver and management
              address, and optionally set a desired mode and an
              offline-alarm severity. 4. Confirm. The device is created in
              ADOPTING and its supervised driver immediately probes it toward
              ONLINE.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="adopt-credentials" title={<Trans>Credentials</Trans>}>
          <Prose>
            <Trans>
              Devices that need authentication take a secret reference (e.g.{" "}
              <Code>op://Site/foyer-decoder/credentials</Code>), never a
              plaintext password: the configuration only ever stores and
              exports the reference. If a device rejects its credentials it
              parks in AUTH_FAILED with no blind retries; updating the secret
              reference re-probes automatically.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="adopt-after" title={<Trans>After adoption</Trans>}>
          <Prose>
            <Trans>
              The device page shows live state, streams to bind as sources,
              decode slots to bind as outputs, sync membership, and
              maintenance actions (probe, identify, test pattern, mode,
              reboot). Adoption is captured by config export, so a restart
              re-adopts and re-converges the same fleet idempotently.
            </Trans>
          </Prose>
          <HelpLink to="/help/devices" label={t`About managed devices`} />
        </DocSection>
      </div>
    </>
  );
}
