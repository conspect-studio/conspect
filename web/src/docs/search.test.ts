// Unit tests for the registry-derived docs search (ADR-W016, MiniSearch).
// The index is built from page titles/summaries/keywords and section titles/
// keywords; queries are prefix + fuzzy so operator typos still land.
import { describe, expect, it } from "vitest";

import type { MessageDescriptor } from "@lingui/core";

import { createDocsSearch } from "./search";

// Tests run without a compiled catalog: translate by falling back to the
// macro-embedded source message (exactly what the app does pre-activation).
function translate(descriptor: MessageDescriptor): string {
  return descriptor.message ?? descriptor.id;
}

const search = createDocsSearch(translate);

describe("docs search", () => {
  it('finds the timing article for "ptp"', () => {
    const hits = search("ptp");
    expect(hits.length).toBeGreaterThan(0);
    expect(hits.map((hit) => hit.path)).toContain("/help/concepts/timing-sync");
  });

  it('finds the transports article for "ndi"', () => {
    const hits = search("ndi");
    expect(hits.length).toBeGreaterThan(0);
    expect(hits.map((hit) => hit.path)).toContain("/help/concepts/transports");
  });

  it('finds the codecs article for "transcoding"', () => {
    const hits = search("transcoding");
    expect(hits.length).toBeGreaterThan(0);
    expect(hits.map((hit) => hit.path)).toContain("/help/concepts/codecs");
  });

  it("returns no results for an empty or whitespace query", () => {
    expect(search("")).toEqual([]);
    expect(search("   ")).toEqual([]);
  });

  it("section hits carry the section id and titles for deep links", () => {
    const hits = search("ptp");
    const sectionHit = hits.find(
      (hit) =>
        hit.path === "/help/concepts/timing-sync" && hit.sectionId === "ptp",
    );
    expect(sectionHit).toBeDefined();
    expect(sectionHit?.title.length ?? 0).toBeGreaterThan(0);
    expect(sectionHit?.pageTitle.length ?? 0).toBeGreaterThan(0);
  });

  it("prefix-matches partial words", () => {
    const hits = search("transc");
    expect(hits.map((hit) => hit.path)).toContain("/help/concepts/codecs");
  });

  it("fuzzy-matches a one-letter typo", () => {
    const hits = search("genlok");
    expect(hits.map((hit) => hit.path)).toContain("/help/concepts/timing-sync");
  });
});
