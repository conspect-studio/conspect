// The enforcement-ladder COPY + classification (logic, no JSX) — kept separate
// from the badge component so each file's exports are uniform (react-refresh).
//
// The enforcement state is DATA the surface renders (ADR-0050 §6): the same
// level the engine, the API, and the portals read. Every rung keeps a running
// program ON AIR (invariant #1); the copy says so plainly, with no urgency
// theatre and no emoji, in British/Australian spelling ('licence').
import { useLingui } from "@lingui/react/macro";

import type { EnforcementLevel } from "../../api/conspectQueries";

/**
 * The exact ladder-level label (the kebab slug the portals share, rendered as
 * the badge text so the operator and the support log name the same rung).
 */
export function enforcementLabel(level: EnforcementLevel): string {
  return level;
}

/**
 * One sentence of operator copy per level (spec §3.2), resolved through the
 * active catalog. Plain, sentence case, British/Australian 'licence', no urgency
 * theatre, no emoji. Each sentence names what (if anything) degrades — and, on
 * every rung, that the program stays on air.
 */
export function useEnforcementSentence(): (level: EnforcementLevel) => string {
  const { t } = useLingui();
  return (level: EnforcementLevel): string => {
    switch (level) {
      case "active":
        return t`Your licence is active and nothing is restricted.`;
      case "warning":
        return t`Your licence lease is approaching expiry; conveniences still work and the program stays on air.`;
      case "config-locked":
        return t`The lease has lapsed past its grace period, so reconfiguration is locked until the next licensing contact; the running program stays on air.`;
      case "watermark":
        return t`The lease has lapsed further, so a corner watermark is added to the canvas; the running program stays on air.`;
      case "block-new-instance":
        return t`The lease is long past contact, so starting a new engine instance is blocked; any running program stays on air.`;
      case "unlicensed-build":
        return t`This is a source build with the licensing client compiled out, reported honestly; the program stays on air.`;
    }
  };
}

/**
 * The plain-text English sentence for a level (the message id Lingui extracts —
 * the catalog source string is exactly this). The `useEnforcementSentence` hook
 * resolves the SAME message ids through the active catalog, so a translated
 * catalog localises this copy without a second set of strings to drift (pinned
 * by the test that asserts the two agree).
 */
export function enforcementSentence(level: EnforcementLevel): string {
  switch (level) {
    case "active":
      return "Your licence is active and nothing is restricted.";
    case "warning":
      return "Your licence lease is approaching expiry; conveniences still work and the program stays on air.";
    case "config-locked":
      return "The lease has lapsed past its grace period, so reconfiguration is locked until the next licensing contact; the running program stays on air.";
    case "watermark":
      return "The lease has lapsed further, so a corner watermark is added to the canvas; the running program stays on air.";
    case "block-new-instance":
      return "The lease is long past contact, so starting a new engine instance is blocked; any running program stays on air.";
    case "unlicensed-build":
      return "This is a source build with the licensing client compiled out, reported honestly; the program stays on air.";
  }
}

/**
 * Whether the chrome ladder banner should be raised for a level. `active` is
 * quiet (no false alarm); `warning` and worse raise the banner. `unlicensed-build`
 * is an honest source-build report, not an escalation, so it does not raise the
 * insistent banner (the Licence screen states it).
 */
export function isLadderBannerLevel(level: EnforcementLevel): boolean {
  return (
    level === "warning" ||
    level === "config-locked" ||
    level === "watermark" ||
    level === "block-new-instance"
  );
}
