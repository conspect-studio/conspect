// Pins the enforcement-ladder presentation: every level resolves to a badge
// variant + ONE sentence of copy (spec §3.2), in British/Australian spelling
// ('licence'), sentence case, no urgency theatre, no emoji. The chrome banner
// only shows at warning-or-worse; `active` is quiet.
import { describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";

import { renderHook } from "@testing-library/react";

import { EnforcementBadge } from "./enforcement";
import {
  enforcementSentence,
  isLadderBannerLevel,
  useEnforcementSentence,
} from "./enforcement-copy";
import type { EnforcementLevel } from "../../api/conspectQueries";
import { renderWithProviders, TestProviders } from "../../test/render";

const ALL_LEVELS: readonly EnforcementLevel[] = [
  "active",
  "warning",
  "config-locked",
  "watermark",
  "block-new-instance",
  "unlicensed-build",
];

describe("enforcementSentence", () => {
  it("returns one sentence for every level (no level is unhandled)", () => {
    for (const level of ALL_LEVELS) {
      const sentence = enforcementSentence(level);
      expect(sentence.length).toBeGreaterThan(0);
    }
  });

  it("uses British/Australian 'licence' spelling and never the American spelling", () => {
    for (const level of ALL_LEVELS) {
      const sentence = enforcementSentence(level).toLowerCase();
      expect(sentence).not.toMatch(/\blicense\b/);
      expect(sentence).not.toMatch(/\blicensed\b/);
    }
  });

  it("never uses urgency theatre or emoji", () => {
    for (const level of ALL_LEVELS) {
      const sentence = enforcementSentence(level);
      // No exclamation marks, no all-caps shouting words, no emoji.
      expect(sentence).not.toContain("!");
      expect(sentence).not.toMatch(/\b(URGENT|WARNING|ACT NOW|IMMEDIATELY)\b/);
      // No emoji (any non-ASCII pictographic).
      expect(/\p{Extended_Pictographic}/u.test(sentence)).toBe(false);
    }
  });

  it("states the on-air promise for the hardest rung", () => {
    expect(enforcementSentence("block-new-instance").toLowerCase()).toContain("on air");
  });

  it("is honest about a source build that compiled the heartbeat out", () => {
    expect(enforcementSentence("unlicensed-build").toLowerCase()).toContain("source build");
  });

  it("resolves the SAME copy through the catalog hook (no drifting second set)", () => {
    const { result } = renderHook(() => useEnforcementSentence(), {
      wrapper: TestProviders,
    });
    for (const level of ALL_LEVELS) {
      expect(result.current(level)).toBe(enforcementSentence(level));
    }
  });
});

describe("isLadderBannerLevel", () => {
  it("does not raise the banner when active (no false alarm)", () => {
    expect(isLadderBannerLevel("active")).toBe(false);
  });

  it("raises the banner at warning or worse", () => {
    for (const level of ["warning", "config-locked", "watermark", "block-new-instance"] as const) {
      expect(isLadderBannerLevel(level)).toBe(true);
    }
  });
});

describe("EnforcementBadge", () => {
  it("renders a label with text (never colour alone)", () => {
    renderWithProviders(<EnforcementBadge level="config-locked" />);
    // The badge carries readable text, not just a hue.
    expect(screen.getByText(/config-locked/i)).toBeInTheDocument();
  });

  it("renders the active level with its own label", () => {
    renderWithProviders(<EnforcementBadge level="active" />);
    expect(screen.getByText(/active/i)).toBeInTheDocument();
  });
});
