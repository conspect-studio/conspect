// Docs: config-as-code (the TOML schema). Summarizes the real fields from
// crates/multiview-config and the shipped examples/*.toml.
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { HelpLink } from "../../components/HelpLink";
import { PageHeader } from "../../components/PageHeader";
import {
  Code,
  CodeBlock,
  DocDefinitions,
  DocList,
  DocSection,
  DocTerm,
  Prose,
} from "./components";

/** Config-as-code documentation. */
export function ConfigPage(): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <PageHeader
        title={<Trans>Config-as-code</Trans>}
        description={
          <Trans>
            A Multiview deployment is one declarative document. It is authored as
            TOML and round-trips losslessly to JSON (the API wire form).
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="document-shape" title={<Trans>Document shape</Trans>}>
          <Prose>
            <Trans>
              Every document starts with a <Code>schema_version</Code> and then
              declares the canvas, the layout strategy, the managed sources, the
              cells (tiles) that place those sources, overlays, and outputs.
              Optional blocks add fault probes, tally profiles, salvos, and video
              walls.
            </Trans>
          </Prose>
          <CodeBlock label="Example TOML config">
            {`schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"          # exact rational string — never a float
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
gap = 8
areas = ["a b", "c d"]`}
          </CodeBlock>
        </DocSection>

        <DocSection id="canvas" title={<Trans>Canvas</Trans>}>
          <Prose>
            <Trans>
              The output canvas: geometry, cadence, working pixel format,
              background, and color space.
            </Trans>
          </Prose>
          <DocDefinitions>
            <DocTerm term={<Code>width</Code>}>
              <Trans>Canvas width in pixels.</Trans>
            </DocTerm>
            <DocTerm term={<Code>height</Code>}>
              <Trans>Canvas height in pixels.</Trans>
            </DocTerm>
            <DocTerm term={<Code>fps</Code>}>
              <Trans>
                Output cadence as a <Code>"num/den"</Code> rational string, for
                example <Code>"30000/1001"</Code> for exact NTSC 29.97. A bare
                float is rejected on purpose, because frame rates must be exact.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>pixel_format</Code>}>
              <Trans>
                Working pixel format: <Code>nv12</Code> (8-bit) or{" "}
                <Code>p010</Code> (10-bit).
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>background</Code>}>
              <Trans>Background fill as a hex color (e.g. {"#101014"}).</Trans>
            </DocTerm>
            <DocTerm term={<Code>[canvas.color]</Code>}>
              <Trans>
                The working color space. <Code>profile</Code> names a preset (e.g.{" "}
                <Code>sdr-bt709-limited</Code>); <Code>custom</Code> lets you set
                primaries, transfer, matrix, and range explicitly.
              </Trans>
            </DocTerm>
          </DocDefinitions>
          <HelpLink
            to="/help/concepts/color#color-spaces"
            label={t`About color spaces and range`}
          />
        </DocSection>

        <DocSection id="layout" title={<Trans>Layout</Trans>}>
          <Prose>
            <Trans>
              The <Code>[layout]</Code> block selects how cells are placed, tagged
              by <Code>kind</Code>:
            </Trans>
          </Prose>
          <DocDefinitions>
            <DocTerm term={<Code>preset</Code>}>
              <Trans>
                A named factory arrangement: <Code>2x2</Code>, <Code>3x3</Code>,{" "}
                <Code>1+5</Code>, or <Code>pip</Code>.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>grid</Code>}>
              <Trans>
                A CSS-grid layout with <Code>columns</Code> and <Code>rows</Code>{" "}
                track lists (<Code>fr</Code> / <Code>px</Code> / <Code>%</Code>), a{" "}
                <Code>gap</Code>, and an <Code>areas</Code> map that names where
                each cell sits.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>absolute</Code>}>
              <Trans>
                Each cell carries its own normalized rectangle (0.0–1.0) in a{" "}
                <Code>rect</Code> field.
              </Trans>
            </DocTerm>
          </DocDefinitions>
        </DocSection>

        <DocSection id="sources" title={<Trans>Sources</Trans>}>
          <Prose>
            <Trans>
              Each <Code>[[sources]]</Code> entry is a managed input with a stable{" "}
              <Code>id</Code>, an optional <Code>display_name</Code>, and a{" "}
              <Code>kind</Code> that selects the transport. The network kinds carry
              a <Code>url</Code>; NDI binds by <Code>name</Code>; file inputs use a{" "}
              <Code>path</Code>; <Code>test</Code> is a built-in synthetic pattern
              with no parameters.
            </Trans>
          </Prose>
          <DocList>
            <li>
              <Trans>
                Kinds: <Code>test</Code>, <Code>rtsp</Code>, <Code>hls</Code>,{" "}
                <Code>ts</Code>, <Code>srt</Code>, <Code>rtmp</Code>,{" "}
                <Code>ndi</Code>, <Code>file</Code>.
              </Trans>
            </li>
            <li>
              <Trans>
                Optional <Code>captions</Code> select a caption or subtitle track
                (auto, teletext page, CEA-608/708 service, a track id, or a
                sidecar file). The engine never decodes a track it will not show.
              </Trans>
            </li>
            <li>
              <Trans>
                Credentials are referenced, never inlined: <Code>auth</Code> holds
                a <Code>secret_ref</Code> pointer, not a plaintext secret.
              </Trans>
            </li>
          </DocList>
          <CodeBlock label="Example source">
            {`[[sources]]
id = "in_live"
display_name = "RTSP camera"
kind = "rtsp"
url = "rtsp://camera.local:8554/main"
[sources.rtsp]
transport = "tcp"`}
          </CodeBlock>
        </DocSection>

        <DocSection id="cells" title={<Trans>Cells</Trans>}>
          <Prose>
            <Trans>
              Each <Code>[[cells]]</Code> entry is one tile. It is placed by a grid{" "}
              <Code>area</Code> or an absolute <Code>rect</Code>, and binds a
              source through <Code>[cells.source]</Code> — usually a reference to a
              managed source by <Code>input_id</Code>.
            </Trans>
          </Prose>
          <DocList>
            <li>
              <Trans>
                <Code>fit</Code> controls scaling: <Code>fill</Code>,{" "}
                <Code>contain</Code>, <Code>cover</Code>, <Code>none</Code>, or{" "}
                <Code>scale_down</Code>.
              </Trans>
            </li>
            <li>
              <Trans>
                Styling: <Code>z</Code> stacking order, <Code>opacity</Code>,{" "}
                <Code>corner_radius</Code>, and a <Code>border</Code>.
              </Trans>
            </li>
            <li>
              <Trans>
                A per-cell <Code>qos</Code> block sets a degradation{" "}
                <Code>priority</Code> and strategy, so the engine sheds the
                cheapest tiles first under load before touching the program output.
              </Trans>
            </li>
          </DocList>
          <CodeBlock label="Example cell">
            {`[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_live"`}
          </CodeBlock>
        </DocSection>

        <DocSection id="overlays" title={<Trans>Overlays</Trans>}>
          <Prose>
            <Trans>
              Each <Code>[[overlays]]</Code> entry attaches a layer to the whole{" "}
              <Code>canvas</Code> or to a single cell. Common kinds are{" "}
              <Code>clock</Code>, <Code>label</Code>, and <Code>tally_border</Code>
              . Kind-specific parameters (such as a clock <Code>format</Code>) sit
              alongside the common fields and round-trip losslessly.
            </Trans>
          </Prose>
          <CodeBlock label="Example overlay">
            {`[[overlays]]
id = "ov_clock"
kind = "clock"
target = "canvas"
anchor = "bottom_right"
z = 100
format = "%H:%M:%S"
source = "wall"   # the always-ticking clock doubles as a falter sentinel`}
          </CodeBlock>
        </DocSection>

        <DocSection id="outputs" title={<Trans>Outputs</Trans>}>
          <Prose>
            <Trans>
              Each <Code>[[outputs]]</Code> entry is one sink, tagged by{" "}
              <Code>kind</Code>. The canvas is composited and encoded once, then
              fanned out to every output. Today, file-based HLS output works; the
              live network output servers are on the roadmap (see the feature
              guide).
            </Trans>
          </Prose>
          <DocDefinitions>
            <DocTerm term={<Code>hls</Code>}>
              <Trans>
                Writes an HLS playlist and segments to <Code>path</Code> with a{" "}
                <Code>codec</Code> and a <Code>segment_ms</Code> duration.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>ll_hls</Code>}>
              <Trans>
                Low-latency HLS, adding <Code>part_target_ms</Code> and{" "}
                <Code>gop_ms</Code> tuning.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>rtsp_server</Code>}>
              <Trans>
                Serves the canvas at a <Code>mount</Code> with a{" "}
                <Code>latency_profile</Code> hint.
              </Trans>
            </DocTerm>
            <DocTerm
              term={
                <>
                  <Code>rtmp</Code> / <Code>srt</Code>
                </>
              }
            >
              <Trans>
                Push the encoded stream to a destination <Code>url</Code>.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>ndi</Code>}>
              <Trans>Advertise the canvas as an NDI source by <Code>name</Code>.</Trans>
            </DocTerm>
            <DocTerm term={<Code>display</Code>}>
              <Trans>
                Scan the canvas out to a local HDMI/DisplayPort screen by KMS{" "}
                <Code>connector</Code> (a <Code>display-kms</Code> build), with
                an optional exact <Code>mode</Code> override or a{" "}
                <Code>forced_mode</Code> for screens that report no EDID. The
                refresh is an exact rational like <Code>"60000/1001"</Code>,
                never a decimal.
              </Trans>
            </DocTerm>
          </DocDefinitions>
          <CodeBlock label="Example output">
            {`[[outputs]]
kind = "hls"
path = "/var/lib/multiview/hls/multiview.m3u8"
codec = "mpeg2video"
segment_ms = 2000`}
          </CodeBlock>
          <HelpLink
            to="/help/concepts/codecs#what-is-transcoding"
            label={t`About codecs and transcoding`}
          />
          <HelpLink
            to="/help/concepts/transports#choosing"
            label={t`About output transports`}
          />
        </DocSection>

        <DocSection id="validation" title={<Trans>Validation and import / export</Trans>}>
          <Prose>
            <Trans>
              The document is validated as a whole: source and cell ids must be
              unique, every <Code>input_id</Code> must resolve to a declared
              source, every grid cell must reference an existing area, and the
              cadence must be a usable rational. Because TOML and JSON encode the
              same model, a config can be authored by hand and exported as JSON
              over the API, or vice versa.
            </Trans>
          </Prose>
        </DocSection>
      </div>
    </>
  );
}
