// Unit tests for the docs registry (ADR-W016): the registry is the single
// index from which nav, breadcrumbs, related articles, search, and anchor
// targets derive. These tests pin the registry ↔ router contract, the
// kebab-case/uniqueness rules for section ids, the public anchor contract
// other parts of the app deep-link to, and the redirect resolution logic.
import { describe, expect, it } from "vitest";

import { router } from "../app/router";
import {
  DOCS_REDIRECTS,
  DOCS_REGISTRY,
  getDocsPage,
  resolveAnchor,
  resolveAnchorIn,
} from "./registry";

/** Collect every concrete route path under /help from the real route table. */
function collectHelpRoutePaths(): string[] {
  const root = router.routes.find((route) => route.path === "/");
  const help = root?.children?.find((route) => route.path === "help");
  if (help === undefined) {
    throw new Error("router has no /help route");
  }
  const paths: string[] = [];
  for (const child of help.children ?? []) {
    if (child.index === true) {
      paths.push("/help");
    } else if (typeof child.path === "string") {
      paths.push(`/help/${child.path}`);
    }
  }
  return paths;
}

const KEBAB_CASE = /^[a-z0-9]+(-[a-z0-9]+)*$/;

describe("docs registry ↔ router contract", () => {
  it("registers every /help route exactly once", () => {
    const routePaths = collectHelpRoutePaths().sort();
    const registryPaths = DOCS_REGISTRY.map((page) => page.path).sort();
    expect(registryPaths).toEqual(routePaths);
  });

  it("has no duplicate page paths", () => {
    const paths = DOCS_REGISTRY.map((page) => page.path);
    expect(new Set(paths).size).toBe(paths.length);
  });
});

describe("docs registry section ids", () => {
  it("are kebab-case and unique within each page", () => {
    for (const page of DOCS_REGISTRY) {
      const ids = page.sections.map((section) => section.id);
      expect(new Set(ids).size, `duplicate section id on ${page.path}`).toBe(
        ids.length,
      );
      for (const id of ids) {
        expect(id, `non-kebab-case id "${id}" on ${page.path}`).toMatch(
          KEBAB_CASE,
        );
      }
    }
  });

  it("every page has at least one section", () => {
    for (const page of DOCS_REGISTRY) {
      expect(
        page.sections.length,
        `${page.path} has no sections`,
      ).toBeGreaterThan(0);
    }
  });
});

// The public deep-link contract (ADR-W016): these exact path#id pairs are
// linked from management pages and other docs. Anchor ids are append-only;
// renaming one requires a DOCS_REDIRECTS entry, never a silent removal.
const ANCHOR_CONTRACT: readonly (readonly [string, readonly string[]])[] = [
  [
    "/help/concepts/transports",
    [
      "rtsp",
      "ndi",
      "srt",
      "rtmp",
      "hls-ll-hls",
      "mpeg-ts",
      "file-and-synthetic",
      "choosing",
    ],
  ],
  ["/help/concepts/timing-sync", ["output-clock", "genlock", "ptp", "wall-clock"]],
  [
    "/help/concepts/codecs",
    [
      "what-is-transcoding",
      "h264",
      "hevc",
      "av1",
      "hardware-acceleration",
      "encode-once",
    ],
  ],
  ["/help/concepts/color", ["color-spaces", "range", "hdr"]],
  [
    "/help/concepts/resilience",
    ["tile-lifecycle", "last-good-frame", "reconnect"],
  ],
  [
    "/help/concepts/latency",
    ["glass-to-glass", "protocol-latency", "tradeoffs"],
  ],
  [
    "/help/concepts/glossary",
    [
      "ptp",
      "genlock",
      "tally",
      "umd",
      "salvo",
      "transcoding",
      "nv12",
      "chroma-subsampling",
      "pts",
      "gop",
      "bitrate",
      "multicast",
      "mldv2",
      "jitter-buffer",
      "last-good-frame",
      "ll-hls",
      "srt",
      "ndi",
      "rtsp",
      "rtmp",
      "mpeg-ts",
      "color-range",
      "hdr",
    ],
  ],
  [
    "/help/features",
    ["sources", "outputs", "layouts", "overlays", "tally", "salvos", "alarms"],
  ],
  [
    "/help/config",
    ["canvas", "layout", "sources", "cells", "overlays", "outputs"],
  ],
];

describe("docs anchor contract", () => {
  it.each(ANCHOR_CONTRACT)("%s carries its contract anchors", (path, ids) => {
    const page = getDocsPage(path);
    expect(page, `page ${path} missing from registry`).toBeDefined();
    const sectionIds = new Set(page?.sections.map((section) => section.id));
    for (const id of ids) {
      expect(sectionIds.has(id), `${path}#${id} missing`).toBe(true);
    }
  });
});

describe("related articles", () => {
  it("every related path resolves to a registered page and is not self", () => {
    for (const page of DOCS_REGISTRY) {
      for (const related of page.related) {
        expect(
          getDocsPage(related),
          `${page.path} relates to unknown ${related}`,
        ).toBeDefined();
        expect(related).not.toBe(page.path);
      }
    }
  });
});

describe("anchor redirects", () => {
  it("resolveAnchorIn follows a single redirect hop", () => {
    const redirects = { "/help/concepts/color#gamut": "/help/concepts/color#color-spaces" };
    expect(resolveAnchorIn(redirects, "/help/concepts/color", "gamut")).toEqual({
      path: "/help/concepts/color",
      id: "color-spaces",
    });
  });

  it("resolveAnchorIn follows chains and survives cycles", () => {
    const redirects = {
      "/a#one": "/a#two",
      "/a#two": "/b#three",
      "/c#loop": "/c#loop",
    };
    expect(resolveAnchorIn(redirects, "/a", "one")).toEqual({
      path: "/b",
      id: "three",
    });
    // A (mis-authored) cycle terminates rather than spinning forever.
    expect(resolveAnchorIn(redirects, "/c", "loop")).toEqual({
      path: "/c",
      id: "loop",
    });
  });

  it("resolveAnchorIn returns unknown anchors unchanged", () => {
    expect(resolveAnchorIn({}, "/help/api", "playground")).toEqual({
      path: "/help/api",
      id: "playground",
    });
  });

  it("no redirect target is itself a redirect source (no ping-pong)", () => {
    // A target that is also a source would make DocsLayout's resolve-then-
    // navigate effect alternate between the two URLs forever. The map is
    // empty today; this pins the invariant for future rename entries.
    const sources = new Set(Object.keys(DOCS_REDIRECTS));
    for (const [from, to] of Object.entries(DOCS_REDIRECTS)) {
      expect(
        sources.has(to),
        `redirect ${from} → ${to} targets another redirect source`,
      ).toBe(false);
    }
  });

  it("every live redirect target resolves to a registered page section", () => {
    for (const [from, to] of Object.entries(DOCS_REDIRECTS)) {
      const hashIndex = to.indexOf("#");
      expect(hashIndex, `redirect target ${to} has no #anchor`).toBeGreaterThan(0);
      const path = to.slice(0, hashIndex);
      const id = to.slice(hashIndex + 1);
      const page = getDocsPage(path);
      expect(page, `redirect ${from} → unknown page ${path}`).toBeDefined();
      expect(
        page?.sections.some((section) => section.id === id),
        `redirect ${from} → missing section ${to}`,
      ).toBe(true);
    }
  });

  it("resolveAnchor uses the live redirect map", () => {
    // With no live redirects this is the identity; once a rename lands the
    // target-resolution test above pins its validity.
    const result = resolveAnchor("/help/concepts/codecs", "h264");
    expect(result).toEqual({ path: "/help/concepts/codecs", id: "h264" });
  });
});
