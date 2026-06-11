// Concept article: the glossary (ADR-W016). Alphabetized; every term is an
// anchored section (id = kebab-case term) so management pages and search can
// deep-link straight to a definition. Section ids are part of the public
// anchor contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";

import { PageHeader } from "../../../components/PageHeader";
import { Code, DocSection, Prose } from "../components";

/** Glossary concept article. */
export function GlossaryPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>Glossary</Trans>}
        description={
          <Trans>
            Broadcast and streaming terms used across Multiview, alphabetized.
            Use the documentation search to jump straight to a term.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="bitrate" title={<Trans>Bitrate</Trans>}>
          <Prose>
            <Trans>
              The amount of data a stream uses per second, usually in megabits
              per second (Mb/s). For a given codec, more bitrate means better
              picture quality and more bandwidth used. Encoders trade bitrate
              against quality and resolution; a multiview canvas is typically
              encoded at one fixed bitrate per rendition.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="chroma-subsampling" title={<Trans>Chroma subsampling</Trans>}>
          <Prose>
            <Trans>
              Storing color information at lower resolution than brightness,
              exploiting the eye's lower sensitivity to color detail. Written
              as ratios: 4:4:4 keeps full color, 4:2:2 halves it horizontally
              (production quality), and 4:2:0 quarters it (virtually all
              delivery video). Almost every stream Multiview ingests and emits
              is 4:2:0.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="color-range" title={<Trans>Color range</Trans>}>
          <Prose>
            <Trans>
              Whether pixel values use the limited "TV" scale (black at 16,
              white at 235 in 8-bit) or the full "PC" scale (0–255). Reading a
              stream with the wrong range makes it look washed-out (limited
              read as full) or crushed (full read as limited). One of the most
              common picture-quality faults in any video chain.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="genlock" title={<Trans>Genlock</Trans>}>
          <Prose>
            <Trans>
              Locking every device's frame timing to one shared reference
              signal so all frames start at the same instant across a
              facility. Classically an analog black-burst or tri-level signal;
              in IP facilities PTP carries the same role. Genlock aligns{" "}
              <em>phase</em>; timecode merely labels frames.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="gop" title={<Trans>GOP (Group of Pictures)</Trans>}>
          <Prose>
            <Trans>
              The repeating pattern between keyframes in compressed video: a
              self-contained keyframe (I-frame) followed by frames that only
              encode differences. A viewer can only start decoding at a
              keyframe, so the GOP length sets a floor on channel-change and
              segment latency. Live encoders typically use a fixed GOP of one
              or two seconds.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="hdr" title={<Trans>HDR (High Dynamic Range)</Trans>}>
          <Prose>
            <Trans>
              Video using a brightness curve (PQ or HLG) and usually the
              BT.2020 gamut that represents far brighter highlights and deeper
              shadows than standard dynamic range (SDR). HDR tiles must be
              tone-mapped onto an SDR canvas or they look dim and grey.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="jitter-buffer" title={<Trans>Jitter buffer</Trans>}>
          <Prose>
            <Trans>
              A small receive-side buffer that absorbs the uneven arrival of
              network packets so frames can be consumed smoothly. Deeper
              buffers ride out worse networks but add latency. Each Multiview
              input has its own bounded buffering; an overflowing input drops
              its oldest data rather than growing without limit.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="last-good-frame" title={<Trans>Last-good frame</Trans>}>
          <Prose>
            <Trans>
              The most recent valid frame a tile has received, held on screen
              whenever its source hiccups so a brief dropout is invisible.
              Tiles escalate from holding the last-good frame to a
              reconnect indicator and finally a "no signal" slate if the
              source stays away.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="ll-hls" title={<Trans>LL-HLS (Low-Latency HLS)</Trans>}>
          <Prose>
            <Trans>
              The low-latency extension of HLS: segments are published in
              partial chunks and players can request the next part before it
              is finished, cutting typical delay from 6–30 s to roughly
              2–5 s while keeping plain-HTTP delivery and CDN scalability.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="mldv2" title={<Trans>MLDv2</Trans>}>
          <Prose>
            <Trans>
              Multicast Listener Discovery version 2 — the IPv6 protocol a
              receiver uses to tell the network which multicast groups (and,
              with source-specific multicast, which senders) it wants. It is
              the IPv6 successor to IPv4's IGMP and the primary group-join
              mechanism in Multiview's IPv6-first multicast support.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="mpeg-ts" title={<Trans>MPEG-TS (Transport Stream)</Trans>}>
          <Prose>
            <Trans>
              The MPEG transport stream container used throughout broadcast:
              fixed 188-byte packets multiplexing video, audio, subtitles, and
              data on numbered PIDs, designed to stay decodable through noise
              and joins. Carried over UDP/multicast in facilities and inside
              SRT across the internet.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="multicast" title={<Trans>Multicast</Trans>}>
          <Prose>
            <Trans>
              Network delivery where one sender transmits a single copy of a
              stream to a group address and the network fans it out to every
              subscribed receiver. Extremely efficient for in-facility
              distribution, but requires a managed network that routes
              multicast. Receivers subscribe via MLDv2 (IPv6) or IGMP (legacy
              IPv4).
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="ndi" title={<Trans>NDI</Trans>}>
          <Prose>
            <Trans>
              Network Device Interface — a production-LAN video transport in
              which senders advertise themselves by name and receivers
              discover them automatically. Near-frame latency at mezzanine
              quality, at the cost of high bandwidth and LAN-only reach.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="nv12" title={<Trans>NV12</Trans>}>
          <Prose>
            <Trans>
              The 8-bit 4:2:0 pixel layout (full-resolution brightness plane
              plus interleaved half-resolution color plane, 1.5 bytes per
              pixel) that GPUs and hardware codecs natively produce and
              consume. It is Multiview's working pixel format end to end —
              frames stay NV12 through the pipeline and color conversion
              happens on the GPU at tile size.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="ptp" title={<Trans>PTP (Precision Time Protocol)</Trans>}>
          <Prose>
            <Trans>
              IEEE 1588 — distributing time over Ethernet from a grandmaster
              clock so every device agrees on "now" to about a microsecond.
              Under the SMPTE ST 2059 broadcast profile, frame boundaries are
              computable from the time itself, making PTP the IP facility's
              replacement for both genlock and timecode distribution.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="pts" title={<Trans>PTS (Presentation Timestamp)</Trans>}>
          <Prose>
            <Trans>
              The timestamp on each frame saying when it should be shown.
              Real-world streams wrap, jump, and reset their PTS, so Multiview
              normalizes every input's timestamps onto one internal timeline
              and stamps its output purely from its own output clock — raw
              input timestamps never reach the output.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="rtmp" title={<Trans>RTMP</Trans>}>
          <Prose>
            <Trans>
              A TCP push protocol that remains the most widely accepted way to
              send a live stream to a platform. Classic RTMP carries H.264 and
              AAC only; enhanced endpoints negotiate newer codecs. Latency is
              typically a few seconds.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="rtsp" title={<Trans>RTSP</Trans>}>
          <Prose>
            <Trans>
              The Real Time Streaming Protocol — the standard way IP cameras
              and encoders serve a live stream by URL (for example{" "}
              <Code>rtsp://camera.local:8554/main</Code>). The control channel
              negotiates the session and the media flows as RTP, over UDP or
              interleaved in TCP, with latency well under a second.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="salvo" title={<Trans>Salvo</Trans>}>
          <Prose>
            <Trans>
              A named, atomically applied recall of multiview state: one salvo
              can switch the layout, rebind sources into cells, and update
              tally and labels in a single action. Borrowed from router
              control, where a salvo fires many crosspoint takes at once.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="srt" title={<Trans>SRT</Trans>}>
          <Prose>
            <Trans>
              Secure Reliable Transport — an open-source UDP protocol for
              carrying streams across unmanaged networks. It repairs packet
              loss by retransmission inside a configurable latency window and
              encrypts with AES, making it the standard choice for
              contribution over the internet.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="tally" title={<Trans>Tally</Trans>}>
          <Prose>
            <Trans>
              The signalling that shows which sources are on-air: classically
              a red light on the program camera and green on preview. On a
              multiview wall, tally renders as colored borders around tiles,
              driven by an upstream switcher or router.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="transcoding" title={<Trans>Transcoding</Trans>}>
          <Prose>
            <Trans>
              Decoding compressed video and recompressing it into a different
              codec, resolution, or bitrate. Each transcode costs quality (a
              new lossy generation), latency, and compute, so well-designed
              systems transcode once and reuse the result everywhere.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="umd" title={<Trans>UMD (Under-Monitor Display)</Trans>}>
          <Prose>
            <Trans>
              The text label strip under (or on) each tile showing the source
              name — named for the physical display modules mounted under
              monitors in traditional galleries. UMD text can be static or
              driven dynamically by a router or automation system.
            </Trans>
          </Prose>
        </DocSection>
      </div>
    </>
  );
}
