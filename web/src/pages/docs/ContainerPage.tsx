// Docs: container management (docker run / compose, GPU, volumes, healthchecks,
// the API token). Mirrors deploy/compose.yaml + the GPU overlay files.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";

import { PageHeader } from "../../components/PageHeader";
import {
  Code,
  CodeBlock,
  DocDefinitions,
  DocList,
  DocSection,
  DocTerm,
  Prose,
  StatusBadge,
} from "./components";

/** Container management documentation. */
export function ContainerPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>Running in containers</Trans>}
        description={
          <Trans>
            Run Multiview with Docker — single container or the compose stack —
            with optional GPU access, persistent volumes, and healthchecks.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="images" title={<Trans>Images</Trans>}>
          <Prose>
            <Trans>
              Two image families are published. The default image is LGPL-clean
              and software-only; the NVIDIA image adds CUDA-based decode, encode,
              and compositing.
            </Trans>
          </Prose>
          <DocDefinitions>
            <DocTerm term={<Code>ghcr.io/aperim/multiview:latest</Code>}>
              <Trans>
                Default software image. Also carries the VAAPI userspace and Mesa
                or Intel drivers, so it can use an Intel or AMD render node when
                one is passed through.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>ghcr.io/aperim/multiview:latest-nvidia</Code>}>
              <Trans>
                CUDA-enabled image (NVDEC / NVENC plus a wgpu compositor) for
                NVIDIA GPUs.
              </Trans>
            </DocTerm>
          </DocDefinitions>
          <Prose>
            <Trans>
              The default image encodes <Code>mpeg2video</Code> (plays in VLC or
              ffplay). For browser-friendly H.264 or H.265 output, use the{" "}
              <Code>-gpl</Code> image variant — note that H.264 / H.265 make the
              build GPL.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="docker-run" title={<Trans>docker run</Trans>}>
          <Prose>
            <Trans>
              Mount a config file and an output directory, then point the binary
              at the config. The current binary composites, encodes, and writes
              HLS or file output to disk.
            </Trans>
          </Prose>
          <CodeBlock label="Shell command">
            {`docker run --rm \\
  -v "$PWD/multiview.toml:/etc/multiview/multiview.toml:ro" \\
  -v multiview-hls:/var/lib/multiview/hls \\
  ghcr.io/aperim/multiview:latest \\
  run /etc/multiview/multiview.toml`}
          </CodeBlock>
        </DocSection>

        <DocSection id="gpu-access" title={<Trans>GPU access</Trans>}>
          <Prose>
            <Trans>
              GPU acceleration is opt-in. The compose stack ships two overlay
              files that add the right device access; you can apply the same
              settings to a plain <Code>docker run</Code>.
            </Trans>
          </Prose>

          <h3 className="pt-1 font-semibold text-foreground">
            <Trans>NVIDIA</Trans>
          </h3>
          <Prose>
            <Trans>
              Requires the NVIDIA driver and the NVIDIA Container Toolkit on the
              host. The modern path requests devices through CDI; NVENC and NVDEC
              also need the <Code>video</Code> driver capability.
            </Trans>
          </Prose>
          <CodeBlock label="Shell command">
            {`docker run --rm \\
  --device nvidia.com/gpu=all \\
  -e NVIDIA_DRIVER_CAPABILITIES=compute,video,utility \\
  -e NVIDIA_VISIBLE_DEVICES=all \\
  ghcr.io/aperim/multiview:latest-nvidia \\
  run /etc/multiview/multiview.toml`}
          </CodeBlock>

          <h3 className="pt-1 font-semibold text-foreground">
            <Trans>Intel / AMD (VAAPI)</Trans>
          </h3>
          <Prose>
            <Trans>
              Pass through the host render node and add the container user to the
              group that owns it. The render group id varies by host — look it up
              with <Code>getent group render</Code>.
            </Trans>
          </Prose>
          <CodeBlock label="Shell command">
            {`docker run --rm \\
  --device /dev/dri/renderD128:/dev/dri/renderD128 \\
  --group-add "$(getent group render | cut -d: -f3)" \\
  -e LIBVA_DEVICE=/dev/dri/renderD128 \\
  ghcr.io/aperim/multiview:latest \\
  run /etc/multiview/multiview.toml`}
          </CodeBlock>
        </DocSection>

        <DocSection id="volumes" title={<Trans>Volumes</Trans>}>
          <DocList>
            <li>
              <Trans>
                <Code>/etc/multiview/multiview.toml</Code> — the config file,
                mounted read-only.
              </Trans>
            </li>
            <li>
              <Trans>
                <Code>/var/lib/multiview/hls</Code> — the HLS output directory.
                Back it with a named volume so a sidecar (or your host) can serve
                the playlist and segments.
              </Trans>
            </li>
          </DocList>
        </DocSection>

        <DocSection id="healthcheck" title={<Trans>Healthcheck</Trans>}>
          <Prose>
            <Trans>
              The compose stack uses a real liveness check: the HLS playlist must
              have been rewritten within the last 30 seconds. Because the
              segmenter rewrites the playlist every segment, a fresh timestamp
              proves the output clock is still producing frames. The check uses
              only coreutils, so it works on the slim runtime.
            </Trans>
          </Prose>
          <CodeBlock label="Healthcheck command">
            {`f=/var/lib/multiview/hls/multiview.m3u8
test -f "$f" && [ $(( $(date +%s) - $(stat -c %Y "$f") )) -lt 30 ]`}
          </CodeBlock>
        </DocSection>

        <DocSection
          id="api-token"
          title={
            <span className="inline-flex items-center gap-2">
              <Trans>API access token</Trans>
              <StatusBadge status="available" />
            </span>
          }
        >
          <Prose>
            <Trans>
              When the config has a <Code>[control]</Code> section, the binary
              binds that listener and serves the web UI, REST API, and docs
              alongside the engine. Authenticate callers with a bearer token —
              supply it through the <Code>MULTIVIEW_CONTROL_TOKEN</Code>{" "}
              environment variable so it is never baked into an image or written
              to the config file. The presented token is{" "}
              <Code>admin.&lt;secret&gt;</Code>; if the variable is unset, the
              server generates one and logs it once at startup. Publish the
              listener's port (default <Code>8080</Code>) to reach it.
            </Trans>
          </Prose>
          <CodeBlock label="Shell command">
            {`docker run --rm -p 8080:8080 \\
  -e MULTIVIEW_CONTROL_TOKEN="$MULTIVIEW_CONTROL_TOKEN" \\
  ghcr.io/aperim/multiview:latest \\
  run /etc/multiview/multiview.toml
# then open http://[::1]:8080/ (UI) and /docs (API playground)`}
          </CodeBlock>
          <Prose>
            <Trans>
              The unauthenticated surface is just <Code>/</Code>,{" "}
              <Code>/docs</Code>, and <Code>/api/v1/openapi.json</Code>; every{" "}
              <Code>/api/v1</Code> data route requires the token. The image must
              be built with the <Code>web</Code> feature for the UI to be
              embedded (the published image is).
            </Trans>
          </Prose>
        </DocSection>
      </div>
    </>
  );
}
