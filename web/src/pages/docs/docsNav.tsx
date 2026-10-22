// The in-app documentation table of contents, derived from the docs registry
// (ADR-W016) — a page cannot appear here without being registered, searchable,
// and linkable. Labels/summaries are Lingui messages localized at render time;
// route paths are stable, non-localized identifiers under the SPA `/help`
// prefix.
//
// `/help` is used (not `/docs`) on purpose: the backend control plane serves
// the live API playground (Scalar) at the top-level `/docs` path, so the SPA
// keeps its in-app guide under `/help` to avoid clashing with that route.
import type { MessageDescriptor } from "@lingui/core";
import {
  ArrowLeftRight,
  BookA,
  BookOpen,
  Boxes,
  Clock,
  Container,
  FileCog,
  Film,
  HardDrive,
  HeartPulse,
  MonitorCheck,
  Palette,
  Plug,
  Radar,
  Sparkles,
  Timer,
  Workflow,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { DOCS_REGISTRY } from "../../docs/registry";

/** A single documentation table-of-contents entry. */
export interface DocsNavItem {
  /** Stable absolute route path (also used as a React key). */
  readonly path: string;
  /** Localized label message (render via `useLingui`). */
  readonly title: MessageDescriptor;
  /** Short localized summary shown on the docs landing page. */
  readonly summary: MessageDescriptor;
  /** Leading icon (decorative; the label carries meaning). */
  readonly Icon: LucideIcon;
}

/** Per-page icons (decorative). Pages without an entry fall back to BookOpen. */
const PAGE_ICONS: Readonly<Record<string, LucideIcon>> = {
  "/help": BookOpen,
  "/help/containers": Container,
  "/help/compose": Boxes,
  "/help/config": FileCog,
  "/help/api": Plug,
  "/help/features": Sparkles,
  "/help/devices": HardDrive,
  "/help/devices/adopt": Radar,
  "/help/display-nodes": MonitorCheck,
  "/help/sync": Workflow,
  "/help/concepts/transports": ArrowLeftRight,
  "/help/concepts/timing-sync": Clock,
  "/help/concepts/codecs": Film,
  "/help/concepts/color": Palette,
  "/help/concepts/resilience": HeartPulse,
  "/help/concepts/latency": Timer,
  "/help/concepts/glossary": BookA,
};

function toNavItem(path: string): DocsNavItem[] {
  const page = DOCS_REGISTRY.find((entry) => entry.path === path);
  if (page === undefined) {
    return [];
  }
  return [
    {
      path: page.path,
      title: page.title,
      summary: page.summary,
      Icon: PAGE_ICONS[page.path] ?? BookOpen,
    },
  ];
}

const isConceptPath = (path: string): boolean =>
  path.startsWith("/help/concepts/");

/** The guide pages (everything outside the concept library), reading order. */
export const DOCS_NAV_GUIDES: readonly DocsNavItem[] = DOCS_REGISTRY.filter(
  (page) => !isConceptPath(page.path),
).flatMap((page) => toNavItem(page.path));

/** The concept-library pages, reading order. */
export const DOCS_NAV_CONCEPTS: readonly DocsNavItem[] = DOCS_REGISTRY.filter(
  (page) => isConceptPath(page.path),
).flatMap((page) => toNavItem(page.path));

/** All documentation nav entries, in sidebar order. */
export const DOCS_NAV: readonly DocsNavItem[] = [
  ...DOCS_NAV_GUIDES,
  ...DOCS_NAV_CONCEPTS,
];
