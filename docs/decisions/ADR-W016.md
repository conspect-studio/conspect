# ADR-W016: In-app documentation system — search, anchors, deep links, concept library

- **Status:** Accepted
- **Area:** Web/API stack · embedded SPA documentation
- **Date:** 2026-06-10
- **Source:** operator direction (2026-06-10): docs need keyword search, deep linking from the
  management UI, and concept articles; builds on the existing `/help` JSX docs and ADR-W013/W014

## Context

The SPA ships six hand-authored JSX help pages (overview, containers, compose, config, API,
features) with **no search, no stable anchors, and no links from management pages into help**.
Concept-level material (what PTP is and why it matters, what transcoding is, how RTSP differs from
NDI or SRT, color management, the tile lifecycle) exists only in internal research briefs
(`docs/research/`) that never ship to users. Operators managing a broadcast tool need that
grounding where they work.

## Decision

1. **Docs registry as the single index.** Every help page registers in a typed
   `docsRegistry` module: `{ path, title, summary, keywords, sections: [{ id, title, keywords }] }`.
   The sidebar nav, breadcrumbs, related-articles, search index, and deep-link targets are all
   derived from this one structure — a page cannot exist without being searchable and linkable.
2. **Client-side keyword search** with **MiniSearch** (~8 KB gzip, MIT, zero deps): fuzzy +
   prefix search over title/summary/keywords/section titles, exposed as a search box in the docs
   sidebar **and** a global help-search affordance in the app shell. Results deep-link to
   `path#section`. No server round-trip; works fully offline in the embedded SPA.
3. **Stable anchors.** `DocSection` takes a mandatory `id` (kebab-case, part of the public URL
   contract); headings render anchor links; `DocsLayout` scrolls to and highlights the target on
   navigation. Anchor ids are append-only — renames require a redirect entry in the registry.
4. **Deep links from the management UI.** A `HelpLink` component (`to="/help/...#anchor"`,
   accessible label, BookOpen icon) is placed on every management page header and on
   concept-bearing form fields (e.g. the codec picker links to the codecs article, the source-kind
   picker to the transports article). Help is one click from the place the question arises.
5. **Concept library.** A new `/help/concepts/*` section, mined from the research briefs but
   written operator-first and vendor-neutral: transports compared (RTSP, NDI, SRT, RTMP, HLS/LL-HLS,
   MPEG-TS, file), timing & sync (the output clock, genlock as a concept, PTP/ST 2059, wall-clock),
   codecs & transcoding (H.264/HEVC/AV1, hardware acceleration, encode-once), color management,
   resilience & the tile lifecycle (LIVE/STALE/RECONNECTING/NO_SIGNAL), latency, and a searchable
   glossary (PTP, tally, UMD, salvo, NV12, chroma subsampling, …). Authored as JSX with the
   existing prose primitives, all strings Lingui-wrapped.

## Rationale

A registry-derived system makes search/anchors/nav impossible to forget for future pages (the type
system enforces registration). MiniSearch beats hand-rolled scoring on quality (fuzzy matching,
field boosts) at negligible cost and stays client-side — mandatory for a rust-embed single-binary
deploy. JSX authoring is retained: it is already i18n/theme/a11y-integrated, and a markdown
pipeline would add a remark/rehype toolchain for no user-visible gain at this content volume.
Concept articles are summaries written for operators, not copies of the briefs; briefs remain the
engineering source of truth.

## Alternatives considered

- **fuse.js / FlexSearch / lunr** (rejected: fuse has no field-boosted index; FlexSearch is larger
  and overkill below thousands of documents; lunr is the heaviest of the four).
- **Markdown/MDX content pipeline** (rejected for now: breaks Lingui string extraction, adds build
  toolchain; revisit if the library outgrows hand-authored JSX, per the prior docs audit ~200+
  articles).
- **Server-side search endpoint** (rejected: violates the offline single-binary model and adds a
  control-plane surface for static content).

## Consequences

`web/` gains a `minisearch` dependency (MIT — license-clean). The registry adds a small authoring
step per page, enforced by types and a unit test (every route under `/help` must appear in the
registry; every registry path must resolve). Anchor ids become a public contract; the registry
carries redirects for renames. Concept articles increase the docs bundle — they ship as
route-level lazy chunks so the management UI's initial load is unaffected.
