// Registry-derived docs search (ADR-W016). Builds a MiniSearch index over
// page titles/summaries/keywords and section titles/keywords; queries run
// fully client-side (the embedded SPA works offline). Title and keyword
// matches are boosted; prefix + fuzzy matching absorbs partial words and
// small typos.
import MiniSearch from "minisearch";

import type { MessageDescriptor } from "@lingui/core";

import { DOCS_REGISTRY } from "./registry";

/** Translates a registry message for indexing/display. */
export type DocsTranslate = (descriptor: MessageDescriptor) => string;

/** One search hit, ready to render and deep-link. */
export interface DocsSearchHit {
  /** Destination page path. */
  readonly path: string;
  /** Matched section id, or null when the page itself matched. */
  readonly sectionId: string | null;
  /** Localized title of the match (section title, or page title). */
  readonly title: string;
  /** Localized title of the page the match belongs to. */
  readonly pageTitle: string;
}

/** A ready-to-query docs search function over the registry-derived index. */
export type DocsSearch = (query: string) => DocsSearchHit[];

interface IndexedDoc {
  readonly id: string;
  readonly path: string;
  readonly sectionId: string | null;
  readonly title: string;
  readonly pageTitle: string;
  readonly summary: string;
  readonly keywords: string;
}

const MAX_HITS = 8;

/**
 * Build the search index from the registry. `translate` localizes titles and
 * summaries so search text matches what the operator sees on screen.
 */
export function createDocsSearch(translate: DocsTranslate): DocsSearch {
  const documents: IndexedDoc[] = [];
  for (const page of DOCS_REGISTRY) {
    const pageTitle = translate(page.title);
    documents.push({
      id: page.path,
      path: page.path,
      sectionId: null,
      title: pageTitle,
      pageTitle,
      summary: translate(page.summary),
      keywords: page.keywords.join(" "),
    });
    for (const section of page.sections) {
      documents.push({
        id: `${page.path}#${section.id}`,
        path: page.path,
        sectionId: section.id,
        title: translate(section.title),
        pageTitle,
        summary: "",
        keywords: section.keywords.join(" "),
      });
    }
  }

  const index = new MiniSearch<IndexedDoc>({
    fields: ["title", "keywords", "summary"],
    storeFields: ["path", "sectionId", "title", "pageTitle"],
    searchOptions: {
      boost: { title: 3, keywords: 2 },
      prefix: true,
      fuzzy: 0.2,
    },
  });
  index.addAll(documents);

  return (query: string): DocsSearchHit[] => {
    const trimmed = query.trim();
    if (trimmed.length === 0) {
      return [];
    }
    return index.search(trimmed).slice(0, MAX_HITS).map((result) => {
      // storeFields round-trip the IndexedDoc fields; MiniSearch types them
      // via an `any` index signature, so read through `unknown` and narrow.
      const fields: Record<string, unknown> = result;
      const path = typeof fields.path === "string" ? fields.path : "";
      const sectionId =
        typeof fields.sectionId === "string" ? fields.sectionId : null;
      const title = typeof fields.title === "string" ? fields.title : "";
      const pageTitle =
        typeof fields.pageTitle === "string" ? fields.pageTitle : "";
      return { path, sectionId, title, pageTitle };
    });
  };
}
