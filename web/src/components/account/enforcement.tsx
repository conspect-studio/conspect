// The enforcement-ladder badge component. Copy + classification live in
// `./enforcement-copy.ts` (logic, no JSX) so this file exports only a component
// (react-refresh). The badge hue is paired with the level text (never colour
// alone — WCAG 1.4.1).
import type { JSX } from "react";

import type { EnforcementLevel } from "../../api/conspectQueries";
import { Badge } from "../ui/badge";
import { enforcementLabel } from "./enforcement-copy";

type BadgeVariant =
  | "live"
  | "stale"
  | "reconnecting"
  | "nosignal"
  | "offline"
  | "secondary";

/**
 * The badge hue for a level. Hue is paired with the level text (never colour
 * alone): `active` reads calm, the escalating rungs read progressively more
 * pointed, and the source-build honesty rung is neutral.
 */
function levelVariant(level: EnforcementLevel): BadgeVariant {
  switch (level) {
    case "active":
      return "live";
    case "warning":
      return "stale";
    case "config-locked":
      return "reconnecting";
    case "watermark":
      return "reconnecting";
    case "block-new-instance":
      return "nosignal";
    case "unlicensed-build":
      return "secondary";
  }
}

/** Props for {@link EnforcementBadge}. */
export interface EnforcementBadgeProps {
  /** The enforcement level to present. */
  readonly level: EnforcementLevel;
}

/** A badge naming the enforcement-ladder level (hue + the level text). */
export function EnforcementBadge({ level }: EnforcementBadgeProps): JSX.Element {
  return <Badge variant={levelVariant(level)}>{enforcementLabel(level)}</Badge>;
}
