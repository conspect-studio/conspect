// The docs registry (ADR-W016) — the single index of every in-app help page.
//
// Sidebar nav, breadcrumbs, the related-articles footer, the search index,
// and anchor deep-link targets ALL derive from this structure, so a page
// cannot exist without being searchable and linkable. Titles/summaries are
// Lingui messages (`msg`) localized at render/search time; keywords are
// search-only and never displayed. Section ids are kebab-case and append-only:
// a rename requires a `DOCS_REDIRECTS` entry, never a silent removal — the
// unit tests in registry.test.ts pin this contract.
import { msg } from "@lingui/core/macro";

import type { DocsAnchorTarget, DocsPageEntry } from "./types";

export type { DocsAnchorTarget, DocsPageEntry, DocsSectionEntry } from "./types";

/** Every in-app docs page, in sidebar reading order. */
export const DOCS_REGISTRY: readonly DocsPageEntry[] = [
  {
    path: "/help",
    title: msg`Overview`,
    summary: msg`What Multiview is, the design pillars, and how to get started.`,
    keywords: ["introduction", "getting started", "quick start", "multiview"],
    sections: [
      {
        id: "what-multiview-is",
        title: msg`What Multiview is`,
        keywords: ["multiviewer", "monitoring wall", "compositor", "gpu"],
      },
      {
        id: "design-pillars",
        title: msg`Design pillars`,
        keywords: ["continuous output", "resilience", "isolation"],
      },
      {
        id: "getting-started",
        title: msg`Getting started`,
        keywords: ["compose", "docker", "quick start", "run"],
      },
      {
        id: "where-next",
        title: msg`Where to go next`,
        keywords: ["index", "contents"],
      },
    ],
    related: ["/help/containers", "/help/features"],
  },
  {
    path: "/help/containers",
    title: msg`Running in containers`,
    summary: msg`docker run and docker compose, GPU access, volumes, healthchecks, and the API token.`,
    keywords: ["docker", "container", "image", "deployment"],
    sections: [
      {
        id: "images",
        title: msg`Images`,
        keywords: ["ghcr", "nvidia", "vaapi", "lgpl", "gpl"],
      },
      {
        id: "docker-run",
        title: msg`docker run`,
        keywords: ["docker run", "mount", "config"],
      },
      {
        id: "gpu-access",
        title: msg`GPU access`,
        keywords: ["nvidia", "cuda", "vaapi", "render node", "cdi", "container toolkit"],
      },
      {
        id: "volumes",
        title: msg`Volumes`,
        keywords: ["mount", "hls output", "config file"],
      },
      {
        id: "healthcheck",
        title: msg`Healthcheck`,
        keywords: ["liveness", "playlist", "monitoring"],
      },
      {
        id: "api-token",
        title: msg`API access token`,
        keywords: ["auth", "bearer", "token", "control", "environment variable"],
      },
    ],
    related: ["/help/compose", "/help/config"],
  },
  {
    path: "/help/compose",
    title: msg`Compose reference`,
    summary: msg`The services in the quick-start compose file and how to bring them up and down.`,
    keywords: ["docker compose", "quick start", "stack"],
    sections: [
      {
        id: "services",
        title: msg`The three services`,
        keywords: ["testsrc", "nginx", "hls", "rtsp"],
      },
      {
        id: "up-and-down",
        title: msg`Bring it up and down`,
        keywords: ["start", "stop", "compose up", "compose down"],
      },
      {
        id: "gpu-overlays",
        title: msg`GPU overlays`,
        keywords: ["nvidia", "vaapi", "overlay file", "render group"],
      },
      {
        id: "ports-and-roadmap",
        title: msg`Exposed ports and roadmap`,
        keywords: ["ports", "8554", "8888", "roadmap"],
      },
    ],
    related: ["/help/containers", "/help/config"],
  },
  {
    path: "/help/config",
    title: msg`Config-as-code`,
    summary: msg`The TOML schema: canvas, layout, sources, cells, overlays, and outputs.`,
    keywords: ["toml", "schema", "configuration", "declarative"],
    sections: [
      {
        id: "document-shape",
        title: msg`Document shape`,
        keywords: ["schema_version", "toml", "json"],
      },
      {
        id: "canvas",
        title: msg`Canvas`,
        keywords: ["resolution", "fps", "frame rate", "pixel format", "background", "color profile"],
      },
      {
        id: "layout",
        title: msg`Layout`,
        keywords: ["grid", "preset", "absolute", "areas", "tracks"],
      },
      {
        id: "sources",
        title: msg`Sources`,
        keywords: ["input", "kind", "url", "captions", "credentials"],
      },
      {
        id: "cells",
        title: msg`Cells`,
        keywords: ["tile", "fit", "contain", "cover", "qos", "priority"],
      },
      {
        id: "overlays",
        title: msg`Overlays`,
        keywords: ["clock", "label", "tally border"],
      },
      {
        id: "outputs",
        title: msg`Outputs`,
        keywords: ["hls", "ll-hls", "rtsp", "rtmp", "srt", "ndi", "display", "connector", "codec", "sink"],
      },
      {
        id: "validation",
        title: msg`Validation and import / export`,
        keywords: ["validate", "import", "export", "unique ids"],
      },
    ],
    related: ["/help/features", "/help/concepts/color", "/help/concepts/codecs"],
  },
  {
    path: "/help/api",
    title: msg`API & realtime`,
    summary: msg`The REST base, long-running operations, ETags, the WebSocket and SSE streams, and the live API playground.`,
    keywords: ["rest", "api", "websocket", "sse", "openapi", "scalar"],
    sections: [
      {
        id: "playground",
        title: msg`Live API playground`,
        keywords: ["scalar", "openapi", "interactive", "docs"],
      },
      {
        id: "rest-conventions",
        title: msg`REST conventions`,
        keywords: ["etag", "if-match", "idempotency", "problem json", "202"],
      },
      {
        id: "realtime-streams",
        title: msg`Realtime streams`,
        keywords: ["websocket", "sse", "events", "best effort"],
      },
      {
        id: "authentication",
        title: msg`Authentication`,
        keywords: ["bearer token", "role", "admin", "operator", "viewer"],
      },
    ],
    related: ["/help/config", "/help"],
  },
  {
    path: "/help/features",
    title: msg`Feature guide`,
    summary: msg`Layouts, sources, outputs, overlays, tally, salvos, and alarms — what each one is.`,
    keywords: ["features", "multiviewer", "guide"],
    sections: [
      {
        id: "layouts",
        title: msg`Layouts`,
        keywords: ["grid", "preset", "fit", "tiles", "canvas"],
      },
      {
        id: "sources",
        title: msg`Sources`,
        keywords: ["input", "ingest", "rtsp", "hls", "srt", "ndi", "reconnect"],
      },
      {
        id: "outputs",
        title: msg`Outputs`,
        keywords: ["transport", "hls", "encode once", "fan out"],
      },
      {
        id: "overlays",
        title: msg`Overlays`,
        keywords: ["clock", "label", "umd", "under-monitor display"],
      },
      {
        id: "tally",
        title: msg`Tally`,
        keywords: ["on-air", "program", "preview", "border", "red", "green"],
      },
      {
        id: "salvos",
        title: msg`Salvos`,
        keywords: ["recall", "preset", "atomic", "switch"],
      },
      {
        id: "alarms",
        title: msg`Alarms`,
        keywords: ["probe", "black", "freeze", "silence", "loudness", "severity"],
      },
    ],
    related: [
      "/help/config",
      "/help/concepts/transports",
      "/help/concepts/resilience",
    ],
  },
  {
    path: "/help/concepts/transports",
    title: msg`Transports compared`,
    summary: msg`RTSP, NDI, SRT, RTMP, HLS, MPEG-TS, and files: how each carries video and which to use where.`,
    keywords: [
      "transport",
      "protocol",
      "ingest",
      "stream",
      "rtsp vs ndi",
      "comparison",
    ],
    sections: [
      {
        id: "rtsp",
        title: msg`RTSP`,
        keywords: ["rtsp", "rtp", "camera", "ip camera", "554", "interleaved"],
      },
      {
        id: "ndi",
        title: msg`NDI`,
        keywords: ["ndi", "network device interface", "discovery", "lan", "production"],
      },
      {
        id: "srt",
        title: msg`SRT`,
        keywords: [
          "srt",
          "secure reliable transport",
          "contribution",
          "internet",
          "encryption",
          "caller",
          "listener",
        ],
      },
      {
        id: "rtmp",
        title: msg`RTMP`,
        keywords: ["rtmp", "flash", "push", "streaming platform"],
      },
      {
        id: "hls-ll-hls",
        title: msg`HLS and LL-HLS`,
        keywords: ["hls", "ll-hls", "playlist", "segments", "http", "cdn", "m3u8"],
      },
      {
        id: "mpeg-ts",
        title: msg`MPEG-TS over UDP and multicast`,
        keywords: ["mpeg-ts", "transport stream", "udp", "multicast", "broadcast", "rtp"],
      },
      {
        id: "file-and-synthetic",
        title: msg`Files and synthetic sources`,
        keywords: ["file", "test pattern", "bars", "clock", "loop"],
      },
      {
        id: "choosing",
        title: msg`Choosing a transport`,
        keywords: ["comparison", "latency", "robustness", "which transport", "table"],
      },
    ],
    related: ["/help/concepts/latency", "/help/concepts/codecs", "/help/features"],
  },
  {
    path: "/help/concepts/timing-sync",
    title: msg`Timing & sync`,
    summary: msg`The output clock, genlock, PTP, and wall-clock time — how frames are paced and aligned.`,
    keywords: [
      "timing",
      "sync",
      "synchronization",
      "clock",
      "reference",
      "frame accurate",
    ],
    sections: [
      {
        id: "output-clock",
        title: msg`The output clock`,
        keywords: ["cadence", "tick", "monotonic", "pacing", "never stalls", "frame rate"],
      },
      {
        id: "genlock",
        title: msg`Genlock`,
        keywords: ["genlock", "black burst", "tri-level", "phase", "house sync", "reference"],
      },
      {
        id: "ptp",
        title: msg`PTP (Precision Time Protocol)`,
        keywords: [
          "ptp",
          "ieee 1588",
          "smpte st 2059",
          "st 2110",
          "grandmaster",
          "precision time",
          "facility clock",
        ],
      },
      {
        id: "wall-clock",
        title: msg`Wall-clock time`,
        keywords: ["ntp", "utc", "time of day", "program-date-time", "timecode"],
      },
    ],
    related: [
      "/help/concepts/latency",
      "/help/concepts/resilience",
      "/help/concepts/glossary",
    ],
  },
  {
    path: "/help/concepts/codecs",
    title: msg`Codecs & transcoding`,
    summary: msg`What transcoding is, the common video codecs, hardware acceleration, and the encode-once model.`,
    keywords: ["codec", "encode", "decode", "compression", "video quality"],
    sections: [
      {
        id: "what-is-transcoding",
        title: msg`What transcoding is`,
        keywords: ["transcoding", "transcode", "re-encode", "decode", "recompress", "generation loss"],
      },
      {
        id: "h264",
        title: msg`H.264 / AVC`,
        keywords: ["h264", "avc", "baseline", "compatibility"],
      },
      {
        id: "hevc",
        title: msg`H.265 / HEVC`,
        keywords: ["h265", "hevc", "efficiency", "4k", "10-bit"],
      },
      {
        id: "av1",
        title: msg`AV1`,
        keywords: ["av1", "royalty free", "aom", "next generation"],
      },
      {
        id: "hardware-acceleration",
        title: msg`Hardware acceleration`,
        keywords: ["nvenc", "nvdec", "vaapi", "videotoolbox", "quick sync", "gpu encode"],
      },
      {
        id: "encode-once",
        title: msg`Encode once, fan out many`,
        keywords: ["rendition", "fan out", "efficiency", "encode once mux many"],
      },
    ],
    related: [
      "/help/concepts/transports",
      "/help/config",
      "/help/containers",
    ],
  },
  {
    path: "/help/concepts/color",
    title: msg`Color management`,
    summary: msg`Color spaces, limited vs full range, and HDR — why mismatches look washed-out or crushed.`,
    keywords: ["color", "colour", "washed out", "crushed blacks", "saturation"],
    sections: [
      {
        id: "color-spaces",
        title: msg`Color spaces`,
        keywords: ["bt.601", "bt.709", "bt.2020", "primaries", "matrix", "gamut", "rec 709"],
      },
      {
        id: "range",
        title: msg`Limited vs full range`,
        keywords: ["limited range", "full range", "tv range", "pc range", "16-235", "grey blacks"],
      },
      {
        id: "hdr",
        title: msg`HDR`,
        keywords: ["hdr", "pq", "hlg", "tone mapping", "sdr", "high dynamic range"],
      },
    ],
    related: ["/help/concepts/codecs", "/help/config"],
  },
  {
    path: "/help/concepts/resilience",
    title: msg`Resilience & the tile lifecycle`,
    summary: msg`The LIVE / STALE / RECONNECTING / NO SIGNAL ladder, last-good frames, and why output never stalls.`,
    keywords: ["resilience", "failure", "signal loss", "stale", "no signal", "badge"],
    sections: [
      {
        id: "tile-lifecycle",
        title: msg`The tile lifecycle`,
        keywords: ["live", "stale", "reconnecting", "no signal", "state machine", "badge"],
      },
      {
        id: "last-good-frame",
        title: msg`Last-good frames`,
        keywords: ["hold frame", "freeze", "placeholder", "slate"],
      },
      {
        id: "reconnect",
        title: msg`Reconnect behaviour`,
        keywords: ["reconnect", "backoff", "supervisor", "recovery", "dropout"],
      },
    ],
    related: ["/help/concepts/timing-sync", "/help/features"],
  },
  {
    path: "/help/concepts/latency",
    title: msg`Latency`,
    summary: msg`Glass-to-glass latency, what each protocol adds, and the trade-offs against robustness.`,
    keywords: ["latency", "delay", "lag", "real time", "glass to glass"],
    sections: [
      {
        id: "glass-to-glass",
        title: msg`Glass-to-glass latency`,
        keywords: ["end to end", "capture to display", "budget", "pipeline delay"],
      },
      {
        id: "protocol-latency",
        title: msg`What each protocol adds`,
        keywords: ["hls latency", "srt latency", "rtsp latency", "ndi latency", "buffer"],
      },
      {
        id: "tradeoffs",
        title: msg`Trade-offs`,
        keywords: ["robustness", "jitter", "packet loss", "buffer size", "tuning"],
      },
    ],
    related: ["/help/concepts/transports", "/help/concepts/timing-sync"],
  },
  {
    path: "/help/concepts/glossary",
    title: msg`Glossary`,
    summary: msg`Broadcast and streaming terms used across Multiview, alphabetized and searchable.`,
    keywords: ["glossary", "terms", "definitions", "dictionary"],
    sections: [
      {
        id: "bitrate",
        title: msg`Bitrate`,
        keywords: ["bitrate", "bandwidth", "kbps", "mbps", "quality"],
      },
      {
        id: "chroma-subsampling",
        title: msg`Chroma subsampling`,
        keywords: ["chroma", "4:2:0", "4:2:2", "4:4:4", "color resolution"],
      },
      {
        id: "color-range",
        title: msg`Color range`,
        keywords: ["limited", "full", "tv range", "pc range"],
      },
      {
        id: "genlock",
        title: msg`Genlock`,
        keywords: ["genlock", "sync", "reference", "phase"],
      },
      {
        id: "gop",
        title: msg`GOP (Group of Pictures)`,
        keywords: ["gop", "keyframe", "i-frame", "idr", "group of pictures"],
      },
      {
        id: "hdr",
        title: msg`HDR (High Dynamic Range)`,
        keywords: ["hdr", "pq", "hlg", "brightness"],
      },
      {
        id: "jitter-buffer",
        title: msg`Jitter buffer`,
        keywords: ["jitter", "buffer", "network", "smoothing"],
      },
      {
        id: "last-good-frame",
        title: msg`Last-good frame`,
        keywords: ["hold", "freeze frame", "resilience"],
      },
      {
        id: "ll-hls",
        title: msg`LL-HLS (Low-Latency HLS)`,
        keywords: ["ll-hls", "low latency", "parts", "apple"],
      },
      {
        id: "mldv2",
        title: msg`MLDv2`,
        keywords: ["mld", "mldv2", "multicast listener discovery", "ipv6", "igmp"],
      },
      {
        id: "mpeg-ts",
        title: msg`MPEG-TS (Transport Stream)`,
        keywords: ["mpeg-ts", "transport stream", "ts", "broadcast", "pid"],
      },
      {
        id: "multicast",
        title: msg`Multicast`,
        keywords: ["multicast", "group", "one to many", "igmp", "network"],
      },
      {
        id: "ndi",
        title: msg`NDI`,
        keywords: ["ndi", "network device interface", "lan video"],
      },
      {
        id: "nv12",
        title: msg`NV12`,
        keywords: ["nv12", "pixel format", "yuv", "4:2:0", "semi-planar"],
      },
      {
        id: "ptp",
        title: msg`PTP (Precision Time Protocol)`,
        keywords: ["ptp", "ieee 1588", "st 2059", "grandmaster", "clock"],
      },
      {
        id: "pts",
        title: msg`PTS (Presentation Timestamp)`,
        keywords: ["pts", "timestamp", "presentation", "dts"],
      },
      {
        id: "rtmp",
        title: msg`RTMP`,
        keywords: ["rtmp", "push", "streaming platform"],
      },
      {
        id: "rtsp",
        title: msg`RTSP`,
        keywords: ["rtsp", "camera", "real time streaming protocol"],
      },
      {
        id: "salvo",
        title: msg`Salvo`,
        keywords: ["salvo", "recall", "preset", "router"],
      },
      {
        id: "srt",
        title: msg`SRT`,
        keywords: ["srt", "secure reliable transport", "contribution"],
      },
      {
        id: "tally",
        title: msg`Tally`,
        keywords: ["tally", "on-air", "program", "preview", "red", "green"],
      },
      {
        id: "transcoding",
        title: msg`Transcoding`,
        keywords: ["transcoding", "re-encode", "convert"],
      },
      {
        id: "umd",
        title: msg`UMD (Under-Monitor Display)`,
        keywords: ["umd", "under monitor display", "label", "source name"],
      },
    ],
    related: ["/help/concepts/transports", "/help/concepts/timing-sync"],
  },
];

/**
 * Anchor redirects: `"path#old-id"` → `"path#new-id"`. Anchor ids are
 * append-only; when a section id must change, the old pair is added here so
 * published deep links keep resolving. Currently empty — no id has ever been
 * renamed.
 */
export const DOCS_REDIRECTS: Readonly<Record<string, string>> = {};

const PAGES_BY_PATH: ReadonlyMap<string, DocsPageEntry> = new Map(
  DOCS_REGISTRY.map((page) => [page.path, page]),
);

/** Look up a registered page by its route path. */
export function getDocsPage(path: string): DocsPageEntry | undefined {
  return PAGES_BY_PATH.get(path);
}

/** Maximum redirect hops followed before giving up (guards mis-authored cycles). */
const MAX_REDIRECT_HOPS = 5;

/**
 * Resolve an anchor through a redirect map, following bounded chains. Unknown
 * anchors and cycles resolve to the input unchanged.
 */
export function resolveAnchorIn(
  redirects: Readonly<Record<string, string>>,
  path: string,
  id: string,
): DocsAnchorTarget {
  let current: DocsAnchorTarget = { path, id };
  for (let hop = 0; hop < MAX_REDIRECT_HOPS; hop += 1) {
    const next = redirects[`${current.path}#${current.id}`];
    if (next === undefined) {
      return current;
    }
    const hashIndex = next.indexOf("#");
    if (hashIndex <= 0) {
      return current;
    }
    const target: DocsAnchorTarget = {
      path: next.slice(0, hashIndex),
      id: next.slice(hashIndex + 1),
    };
    if (target.path === current.path && target.id === current.id) {
      return current;
    }
    current = target;
  }
  return current;
}

/** Resolve an anchor through the live redirect map. */
export function resolveAnchor(path: string, id: string): DocsAnchorTarget {
  return resolveAnchorIn(DOCS_REDIRECTS, path, id);
}
