// Concept article: codecs & transcoding (ADR-W016). Section ids are part of
// the public anchor contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../../../components/PageHeader";
import { HelpLink } from "../../../components/HelpLink";
import { Code, DocList, DocSection, Prose } from "../components";

/** Codecs & transcoding concept article. */
export function CodecsPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Codecs & transcoding</Trans>}
        description={
          <Trans>
            What transcoding actually is, the common video codecs, hardware
            acceleration, and how Multiview encodes its output.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="what-is-transcoding" title={<Trans>What transcoding is</Trans>}>
          <Prose>
            <Trans>
              Transcoding is decoding compressed video back to raw pictures
              and recompressing it — usually into a different codec, size, or
              bitrate. Every transcode costs three things: <strong>quality</strong>{" "}
              (each recompression is a new lossy generation), <strong>latency</strong>{" "}
              (decoding and re-encoding both buffer frames), and{" "}
              <strong>compute</strong> (encoding is by far the most expensive
              step in a video pipeline). Good systems therefore transcode only
              when they must, and exactly once.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              A multiviewer is inherently a transcoder: it must decode every
              source to composite them into one canvas, and then encode that
              canvas for its outputs. What it never does is transcode each
              tile separately — the cost would multiply by the number of
              tiles for no benefit.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="h264" title={<Trans>H.264 / AVC</Trans>}>
          <Prose>
            <Trans>
              H.264 (also called AVC) is the most widely supported video codec
              in existence: every browser, phone, set-top box, and hardware
              decoder of the last 15+ years plays it. It is the safe default
              for compatibility. Note that software H.264/H.265 encoders carry
              a GPL license obligation in Multiview's build, so the default
              (license-clean) image encodes them only with a hardware encoder;
              choosing a software H.264 encode means using the GPL image
              variant.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="hevc" title={<Trans>H.265 / HEVC</Trans>}>
          <Prose>
            <Trans>
              H.265 (HEVC) compresses roughly 30–50% better than H.264 at the
              same quality, which matters most at 4K and for 10-bit/HDR
              content. The trade-offs: encoding costs more compute, patent
              licensing is more complicated, and playback support — while now
              broad on hardware — is still patchier in browsers than H.264.
              Classic RTMP cannot carry it; newer endpoints and MPEG-TS based
              transports can.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="av1" title={<Trans>AV1</Trans>}>
          <Prose>
            <Trans>
              AV1 is the open, royalty-free codec developed by the Alliance
              for Open Media. It compresses better than HEVC and has fast
              hardware decode support arriving across recent GPUs and devices,
              with hardware encoding available on the newest generations.
              For live work its software encoders are still heavy, so AV1 is
              the right choice when the audience's players are known to
              support it and bandwidth is at a premium.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="hardware-acceleration" title={<Trans>Hardware acceleration</Trans>}>
          <Prose>
            <Trans>
              Modern GPUs and SoCs contain dedicated video engines — separate
              silicon for decode and encode that runs at a fraction of the
              power of a CPU doing the same work. Multiview uses them through
              each platform's standard interface: NVDEC/NVENC on NVIDIA,
              VAAPI on Intel and AMD (including Quick Sync), and VideoToolbox
              on Apple Silicon. With acceleration, decoding a wall of sources
              and encoding the canvas leaves the CPU almost idle; without it,
              a handful of HD decodes can saturate a machine.
            </Trans>
          </Prose>
          <DocList>
            <li>
              <Trans>
                Decode capacity is budgeted in megapixels per second — many
                small tiles can be cheaper than a few full-resolution ones.
              </Trans>
            </li>
            <li>
              <Trans>
                Hardware encoders also sidestep the GPL obligation of the
                software H.264/H.265 encoders, keeping the default build
                license-clean.
              </Trans>
            </li>
            <li>
              <Trans>
                A software fallback always exists, so a build without any GPU
                still works — just with less headroom.
              </Trans>
            </li>
          </DocList>
        </DocSection>

        <DocSection id="encode-once" title={<Trans>Encode once, fan out many</Trans>}>
          <Prose>
            <Trans>
              Multiview composites the canvas <em>once</em> per tick, encodes
              it <em>once per rendition</em>, and fans the same encoded
              packets out to every output that wants that rendition. Writing
              the wall to HLS while pushing it to an RTMP and an SRT
              destination at the same codec, resolution, and bitrate costs one
              encode — not three. A second encode is created only when an
              output genuinely needs a different codec, resolution, or
              bitrate.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Changing an output's codec today is a configuration change, not
              a live switch: stored edits take effect by exporting the config
              and restarting the engine (the API marks these resources{" "}
              <Code>apply: restart</Code>). The codec token is also only
              lightly checked at present — if the build cannot encode the
              requested codec, the pipeline logs a warning and falls back to
              MPEG-2 rather than failing the run. So choose a codec your build
              and transport are known to carry (classic RTMP, for example,
              carries H.264 only), and verify the output really uses it — for
              example with <Code>ffprobe</Code>.
            </Trans>
          </Prose>
          <HelpLink
            to="/help/concepts/transports#choosing"
            label={t`Which transports carry which codecs`}
          />
        </DocSection>
      </div>
    </>
  );
}
