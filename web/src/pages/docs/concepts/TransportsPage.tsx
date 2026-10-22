// Concept article: the source/output transports compared (ADR-W016).
// Operator-first and vendor-neutral; section ids are part of the public
// anchor contract (see src/docs/registry.ts).
import type { JSX, ReactNode } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../../components/PageHeader";
import { HelpLink } from "../../../components/HelpLink";
import { Code, DocList, DocSection, Prose } from "../components";

/** One row of the transport comparison table. */
function ComparisonRow({
  transport,
  latency,
  network,
  robustness,
  use,
}: {
  readonly transport: ReactNode;
  readonly latency: ReactNode;
  readonly network: ReactNode;
  readonly robustness: ReactNode;
  readonly use: ReactNode;
}): JSX.Element {
  return (
    <tr className="border-b last:border-b-0">
      <th scope="row" className="py-2 pe-3 text-start font-medium text-foreground">
        {transport}
      </th>
      <td className="py-2 pe-3">{latency}</td>
      <td className="py-2 pe-3">{network}</td>
      <td className="py-2 pe-3">{robustness}</td>
      <td className="py-2">{use}</td>
    </tr>
  );
}

/** Transports-compared concept article. */
export function TransportsPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Transports compared</Trans>}
        description={
          <Trans>
            How RTSP, NDI, SRT, RTMP, HLS, MPEG-TS, and file sources carry
            video — and which one fits which job.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="rtsp" title={<Trans>RTSP</Trans>}>
          <Prose>
            <Trans>
              RTSP (Real Time Streaming Protocol) is the standard way IP
              cameras and encoders publish a live stream on a local network.
              The client asks the device to describe and play its stream; the
              media itself then flows as RTP packets, either over UDP or
              interleaved in the same TCP connection. Latency is low (commonly
              a few hundred milliseconds) and the URL form is simple:{" "}
              <Code>rtsp://camera.local:8554/main</Code>.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              RTSP carries whatever the camera encodes — usually H.264 or
              H.265 — so the picture quality and bitrate are set at the
              camera. It has no built-in recovery beyond TCP retransmission;
              on flaky links a stream simply stalls and must reconnect, which
              Multiview handles per tile without disturbing the rest of the
              wall.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="ndi" title={<Trans>NDI</Trans>}>
          <Prose>
            <Trans>
              NDI (Network Device Interface) is a production-LAN video
              transport: senders announce themselves on the network, receivers
              discover them by name, and video flows with very low latency
              (around a frame) at high bitrates using NDI's own lightweight
              codec. There are no URLs to manage — a source is selected by its
              advertised name.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              <strong>RTSP vs NDI in one paragraph:</strong> RTSP is a
              pull-style protocol for reaching a specific device by address —
              ideal for fixed camera installs, works across routed networks,
              and spends very little bandwidth because the camera's compressed
              stream is reused as-is. NDI is a discovery-based fabric for a
              production LAN — sources appear by name, latency is near-frame,
              and quality is mezzanine-grade, but it expects a fast local
              network (a 1080p59.94 feed is roughly 100 Mb/s) and does not
              traverse the internet. Choose RTSP to monitor cameras where they
              are; choose NDI when the multiview sits inside a production
              network that already speaks it.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="srt" title={<Trans>SRT</Trans>}>
          <Prose>
            <Trans>
              SRT (Secure Reliable Transport) is an open-source UDP transport
              built for contribution over unmanaged networks — the public
              internet included. It recovers lost packets by selective
              retransmission inside a configurable latency window, encrypts
              with AES, and connects in caller, listener, or rendezvous mode
              so either side can initiate through firewalls.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              The latency window is the key dial: it is the time budget SRT
              has to repair loss. A small window (~120 ms) keeps the feed
              snappy but tolerates little loss; a large window (1–4 s) rides
              out bad networks at the cost of delay. The payload is an MPEG
              transport stream, so codec choices follow the MPEG-TS rules.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="rtmp" title={<Trans>RTMP</Trans>}>
          <Prose>
            <Trans>
              RTMP is a TCP push protocol that remains the lingua franca for
              sending a stream <em>to</em> a service — most streaming
              platforms still ingest it. Classic RTMP carries H.264 video and
              AAC audio only; newer "enhanced" endpoints negotiate H.265 and
              AV1. As an input it appears when an encoder or app pushes to
              Multiview; as an output it is how Multiview pushes the composed
              canvas to a platform.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Being plain TCP, RTMP has no loss-recovery tuning: on a poor
              link it buffers and falls behind rather than dropping. Latency
              is typically a few seconds.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="hls-ll-hls" title={<Trans>HLS and LL-HLS</Trans>}>
          <Prose>
            <Trans>
              HLS (HTTP Live Streaming) delivers video as a playlist of small
              media segments over plain HTTP. That makes it the most
              firewall-friendly and scalable transport — any web server or
              CDN can fan it out to thousands of viewers — at the cost of
              latency: a player typically starts several segments behind the
              live edge, so 6–30 seconds of delay is normal. Low-Latency HLS
              (LL-HLS) publishes partial segments and lets players request the
              next part before it is complete, bringing latency down to
              roughly 2–5 seconds while keeping HTTP delivery.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              As an input, HLS arrives in bursts (whole segments at a time),
              so Multiview paces it back to real time internally before it is
              sampled onto the wall. As an output it is the easiest way to let
              many people open the multiview in a browser or player.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="mpeg-ts" title={<Trans>MPEG-TS over UDP and multicast</Trans>}>
          <Prose>
            <Trans>
              The MPEG transport stream is the container broadcast
              infrastructure speaks: a self-synchronizing packet format that
              multiplexes video, audio, subtitles, and data on numbered PIDs.
              Sent over UDP — usually to a multicast group so one sender
              feeds any number of receivers on the network — it is the
              classic in-facility distribution method, with latency well under
              a second.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Plain UDP has no recovery at all: a lost packet is a glitch on
              screen. That is acceptable on a managed facility LAN and wrong
              for the internet (use SRT there, which wraps the same transport
              stream in a recovering envelope). Multicast group management is
              IPv6-first: receivers join groups via MLDv2, with IPv4/IGMP as
              the legacy path.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="webrtc" title={<Trans>WebRTC (WHIP and WHEP)</Trans>}>
          <Prose>
            <Trans>
              WebRTC carries sub-second video and audio straight to and from a
              browser with no plugin. Multiview speaks the two open HTTP
              signalling profiles built on it: <strong>WHIP</strong> (WebRTC-HTTP
              Ingestion Protocol) for a browser or encoder to publish a feed into
              a <Code>webrtc</Code> source, and <strong>WHEP</strong> (WebRTC-HTTP
              Egress Protocol) for a browser to play a <Code>webrtc</Code> output
              or a live preview. Both negotiate once over HTTPS — the client POSTs
              an SDP offer and gets the answer back — then media flows directly
              over the negotiated peer connection.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              This is the lowest-latency way to reach a web page (well under a
              second, glass to glass) and the transport the in-app live preview
              uses when the build supports it, falling back to a ~1 fps still
              image otherwise. Reaching viewers across restrictive networks can
              need a TURN relay; that is configured by the operator on the server,
              not in the browser. A <Code>webrtc</Code> output is published at a
              derived WHEP URL and a <Code>webrtc</Code> source is published to at
              a derived WHIP URL — the forms show both so a publisher or viewer
              can be configured without reading the protocol specs.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="file-and-synthetic" title={<Trans>Files and synthetic sources</Trans>}>
          <Prose>
            <Trans>
              A file source plays media from disk, paced to real time so it
              behaves like a live feed on the wall (and can loop). Synthetic
              sources — color bars, solid fills, and a ticking clock — are
              generated in-process with no network or decode at all. They are
              ideal for layout work, burn-in tests, and proving the output
              path end to end before any real feed is connected.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="choosing" title={<Trans>Choosing a transport</Trans>}>
          <Prose>
            <Trans>
              An honest comparison — figures are typical classes, not
              guarantees; the network you run on dominates:
            </Trans>
          </Prose>
          <div className="overflow-x-auto">
            <table className="w-full min-w-[40rem] border-collapse text-start text-sm">
              <caption className="sr-only">
                <Trans>
                  Transport comparison: latency class, network fit, robustness,
                  and typical use.
                </Trans>
              </caption>
              <thead>
                <tr className="border-b text-start">
                  <th scope="col" className="py-2 pe-3 text-start font-semibold text-foreground">
                    <Trans>Transport</Trans>
                  </th>
                  <th scope="col" className="py-2 pe-3 text-start font-semibold text-foreground">
                    <Trans>Latency class</Trans>
                  </th>
                  <th scope="col" className="py-2 pe-3 text-start font-semibold text-foreground">
                    <Trans>Network fit</Trans>
                  </th>
                  <th scope="col" className="py-2 pe-3 text-start font-semibold text-foreground">
                    <Trans>Robustness</Trans>
                  </th>
                  <th scope="col" className="py-2 text-start font-semibold text-foreground">
                    <Trans>Typical use</Trans>
                  </th>
                </tr>
              </thead>
              <tbody>
                <ComparisonRow
                  transport={<Trans>RTSP</Trans>}
                  latency={<Trans>Low (~0.2–1 s)</Trans>}
                  network={<Trans>LAN / routed networks</Trans>}
                  robustness={<Trans>TCP only; stalls then reconnects</Trans>}
                  use={<Trans>IP cameras, fixed encoders</Trans>}
                />
                <ComparisonRow
                  transport={<Trans>NDI</Trans>}
                  latency={<Trans>Very low (~1 frame)</Trans>}
                  network={<Trans>Fast production LAN</Trans>}
                  robustness={<Trans>Good on LAN; not for WAN</Trans>}
                  use={<Trans>Studio sources, production fabric</Trans>}
                />
                <ComparisonRow
                  transport={<Trans>SRT</Trans>}
                  latency={<Trans>Low–medium (window-tuned)</Trans>}
                  network={<Trans>Internet / unmanaged links</Trans>}
                  robustness={<Trans>Loss recovery + encryption</Trans>}
                  use={<Trans>Remote contribution feeds</Trans>}
                />
                <ComparisonRow
                  transport={<Trans>RTMP</Trans>}
                  latency={<Trans>Medium (~2–5 s)</Trans>}
                  network={<Trans>Internet (TCP)</Trans>}
                  robustness={<Trans>Buffers and lags under loss</Trans>}
                  use={<Trans>Push to/from streaming platforms</Trans>}
                />
                <ComparisonRow
                  transport={<Trans>HLS / LL-HLS</Trans>}
                  latency={<Trans>High (6–30 s) / medium (2–5 s)</Trans>}
                  network={<Trans>Anywhere HTTP works, CDN-scalable</Trans>}
                  robustness={<Trans>Very high (plain HTTP)</Trans>}
                  use={<Trans>Wide distribution, browser viewing</Trans>}
                />
                <ComparisonRow
                  transport={<Trans>MPEG-TS / UDP multicast</Trans>}
                  latency={<Trans>Very low (&lt;1 s)</Trans>}
                  network={<Trans>Managed facility LAN</Trans>}
                  robustness={<Trans>No recovery; loss = glitch</Trans>}
                  use={<Trans>In-facility broadcast distribution</Trans>}
                />
                <ComparisonRow
                  transport={<Trans>WebRTC (WHIP / WHEP)</Trans>}
                  latency={<Trans>Very low (&lt;1 s)</Trans>}
                  network={<Trans>Browser-direct; TURN for hard NATs</Trans>}
                  robustness={<Trans>Good; degrades over poor links</Trans>}
                  use={<Trans>Browser publish/play, live preview</Trans>}
                />
                <ComparisonRow
                  transport={<Trans>File / synthetic</Trans>}
                  latency={<Trans>None (local)</Trans>}
                  network={<Trans>None needed</Trans>}
                  robustness={<Trans>Fully deterministic</Trans>}
                  use={<Trans>Testing, placeholders, layout work</Trans>}
                />
              </tbody>
            </table>
          </div>
          <DocList>
            <li>
              <Trans>
                Monitoring cameras where they are installed: RTSP.
              </Trans>
            </li>
            <li>
              <Trans>
                Inside a production network that already speaks it: NDI.
              </Trans>
            </li>
            <li>
              <Trans>
                A feed crossing the internet: SRT (tune the latency window to
                the link).
              </Trans>
            </li>
            <li>
              <Trans>
                Letting many viewers watch the composed wall: HLS, or LL-HLS
                when delay matters.
              </Trans>
            </li>
          </DocList>
          <HelpLink
            to="/help/concepts/latency#protocol-latency"
            label={t`What each protocol adds to latency`}
          />
        </DocSection>
      </div>
    </>
  );
}
