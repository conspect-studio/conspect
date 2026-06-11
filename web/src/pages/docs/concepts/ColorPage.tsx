// Concept article: color management (ADR-W016). Section ids are part of the
// public anchor contract (see src/docs/registry.ts).
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";

import { PageHeader } from "../../../components/PageHeader";
import { DocList, DocSection, Prose } from "../components";

/** Color management concept article. */
export function ColorPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>Color management</Trans>}
        description={
          <Trans>
            Color spaces, limited vs full range, and HDR — and why a mismatch
            makes a tile look washed-out, crushed, or oddly tinted.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="color-spaces" title={<Trans>Color spaces</Trans>}>
          <Prose>
            <Trans>
              A video stream does not just carry pixel values — it carries an
              agreement about what those values <em>mean</em>. Three standards
              cover almost everything: <strong>BT.601</strong> (standard
              definition), <strong>BT.709</strong> (high definition — the
              default assumption for most content today), and{" "}
              <strong>BT.2020</strong> (ultra-HD and HDR, with a much wider
              color gamut). Each defines its own recipe for turning the YUV
              numbers in the stream back into red, green, and blue.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Decode a BT.601 stream with the BT.709 recipe (or vice versa)
              and every pixel is still "valid" — just subtly wrong: greens and
              reds shift, skin tones look off. Because many real-world feeds
              (especially IP cameras) ship with no color tags at all,
              Multiview detects what is signaled, fills the gaps with the same
              resolution-based rules real players use, converts every tile to
              the canvas color space before compositing, and tags the output
              so downstream players decode it correctly. A per-source override
              exists for the cases where a camera tags its stream wrongly.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="range" title={<Trans>Limited vs full range</Trans>}>
          <Prose>
            <Trans>
              The same 8-bit pixel can use two different scales.{" "}
              <strong>Limited ("TV") range</strong> puts black at code 16 and
              white at 235 — the broadcast convention virtually all video
              uses. <strong>Full ("PC") range</strong> uses the whole 0–255
              scale — common for computer graphics, screen captures, and some
              webcams.
            </Trans>
          </Prose>
          <DocList>
            <li>
              <Trans>
                Limited content interpreted as full: blacks turn grey, the
                picture looks <strong>washed out and low-contrast</strong>.
              </Trans>
            </li>
            <li>
              <Trans>
                Full content interpreted as limited: shadows <strong>crush to
                solid black</strong> and highlights clip to white.
              </Trans>
            </li>
          </DocList>
          <Prose>
            <Trans>
              This is one of the most common picture-quality bugs in any video
              chain. Multiview expands each tile's range exactly once on
              ingest, composites in a common working space, and compresses
              exactly once on output — and tags the result, because an
              untagged-but-correct stream still gets guessed wrong by some
              players.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="hdr" title={<Trans>HDR</Trans>}>
          <Prose>
            <Trans>
              HDR (high dynamic range) streams use a different brightness
              curve — PQ or HLG instead of the SDR gamma — and usually the
              BT.2020 gamut, letting them represent far brighter highlights
              and deeper shadow detail. An HDR tile dropped onto an SDR canvas
              without conversion looks dim and grey (its values are being
              read with the wrong curve), and an SDR tile pushed through an
              HDR curve blows out.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Multiview tone-maps HDR tiles into an SDR canvas (anchored so
              normal "reference white" content stays at a sensible
              brightness rather than scaling everything down), and only
              treats a stream as HDR when it is explicitly tagged as such —
              resolution alone never implies HDR. The canvas itself can also
              be configured as HDR (PQ or HLG) for monitoring HDR
              productions.
            </Trans>
          </Prose>
        </DocSection>
      </div>
    </>
  );
}
