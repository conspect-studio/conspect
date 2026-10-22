// Concept article: latency (ADR-W016). Section ids are part of the public
// anchor contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../../components/PageHeader";
import { HelpLink } from "../../../components/HelpLink";
import { DocList, DocSection, Prose } from "../components";

/** Latency concept article. */
export function LatencyPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Latency</Trans>}
        description={
          <Trans>
            Where the delay between a camera and your screen comes from, what
            each protocol adds, and the trade-offs against robustness.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="glass-to-glass" title={<Trans>Glass-to-glass latency</Trans>}>
          <Prose>
            <Trans>
              Glass-to-glass latency is the time from light hitting a camera
              sensor ("first glass") to the picture appearing on a display
              ("second glass"). It is a chain of small budgets, and every hop
              adds some: the camera's own encoder, the network transport, any
              receive/jitter buffering, decoding, compositing, re-encoding
              the canvas, the output transport, and finally the viewer's
              player buffer. No single component "is" the latency — the chain
              is.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              In a multiview chain there are two transports in the path: the
              one each source arrives on, and the one the composed wall goes
              out on. Both choices matter, and they are independent — a wall
              of low-latency RTSP cameras served out over plain HLS is still
              tens of seconds behind for its viewers.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="protocol-latency" title={<Trans>What each protocol adds</Trans>}>
          <DocList>
            <li>
              <Trans>
                <strong>NDI:</strong> about a frame on a fast LAN — the lowest
                of the supported transports.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>WebRTC (WHIP/WHEP):</strong> well under a second to a
                browser, end to end — the lowest-latency way to reach a web page,
                at the cost of a per-viewer peer connection.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>MPEG-TS / UDP multicast:</strong> well under a second
                on a managed network (no recovery buffering at all).
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>RTSP:</strong> typically 0.2–1 s — a small network
                buffer plus the camera's encoder delay.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>SRT:</strong> the configured latency window (commonly
                120 ms to a few seconds) — you are explicitly buying loss
                recovery with delay.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>RTMP:</strong> a few seconds of TCP and player
                buffering.
              </Trans>
            </li>
            <li>
              <Trans>
                <strong>HLS:</strong> 6–30 s — players deliberately start
                several segments behind the live edge. <strong>LL-HLS</strong>{" "}
                brings this to roughly 2–5 s with partial segments.
              </Trans>
            </li>
          </DocList>
          <Prose>
            <Trans>
              These are classes, not promises — encoder settings (especially
              keyframe interval), network conditions, and the viewer's player
              all move the number.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="tradeoffs" title={<Trans>Trade-offs</Trans>}>
          <Prose>
            <Trans>
              Latency, robustness, and scale pull against each other.
              Buffering is what absorbs network jitter and gives
              retransmission time to repair loss — so cutting the buffer cuts
              your protection with it. A 120 ms SRT window is snappy but
              fragile on a lossy link; a 2 s window rides out trouble at the
              cost of 2 s. Plain HTTP segments are the most robust and
              scalable delivery there is, and also the slowest.
            </Trans>
          </Prose>
          <DocList>
            <li>
              <Trans>
                Decide who needs low latency. An operator wall watching for
                faults wants seconds at most; overflow viewing in a browser
                can tolerate plain HLS.
              </Trans>
            </li>
            <li>
              <Trans>
                Spend buffer where the network is bad (the internet leg), not
                where it is good (the facility LAN).
              </Trans>
            </li>
            <li>
              <Trans>
                Multiview itself never trades the output cadence away: under
                load it degrades the cheapest tiles first and keeps the
                program output's timing intact.
              </Trans>
            </li>
          </DocList>
          <HelpLink
            to="/help/concepts/transports#choosing"
            label={t`Choosing a transport`}
          />
        </DocSection>
      </div>
    </>
  );
}
