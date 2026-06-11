// Types for the docs registry (ADR-W016). Every /help page registers here;
// the sidebar nav, breadcrumbs, related-articles footer, search index, and
// anchor targets all derive from this one structure.
import type { MessageDescriptor } from "@lingui/core";

/** One anchored section of a docs page (`<section id>` in the rendered page). */
export interface DocsSectionEntry {
  /**
   * Kebab-case anchor id — part of the public URL contract (`path#id`).
   * Append-only: renaming an id requires a `DOCS_REDIRECTS` entry.
   */
  readonly id: string;
  /** Section heading, as a Lingui message (localized at render/search time). */
  readonly title: MessageDescriptor;
  /** Extra search terms for this section (lowercase, not user-visible). */
  readonly keywords: readonly string[];
}

/** One registered docs page. */
export interface DocsPageEntry {
  /** Absolute SPA route path under /help (stable, non-localized). */
  readonly path: string;
  /** Page title, as a Lingui message (nav, breadcrumb, search results). */
  readonly title: MessageDescriptor;
  /** One-line summary (landing page and search context). */
  readonly summary: MessageDescriptor;
  /** Extra search terms for the page (lowercase, not user-visible). */
  readonly keywords: readonly string[];
  /** The page's anchored sections, in reading order. */
  readonly sections: readonly DocsSectionEntry[];
  /** Paths of related pages, shown in the "Related" footer. */
  readonly related: readonly string[];
}

/** A resolved anchor target after applying redirects. */
export interface DocsAnchorTarget {
  readonly path: string;
  readonly id: string;
}
