// Account-side global chrome (brief §13.7): the header licence chip, the ladder
// banner, the pending-action strip, and the config-lock interceptor hook.
//
// All of these render the SAME enforcement data the engine and the portals read
// (ADR-0050 §6). The chrome is a courtesy surface — the API is the real gate
// (a config-locked write returns 409 config_locked regardless of the UI). The
// tile watermark is an ENGINE concern (S3), not a web one; the SPA only explains
// it (see the Licence screen). These components are additive and isolation-safe
// (invariant #10): they only read.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import { ShieldAlert } from "lucide-react";
import { Link } from "react-router-dom";

import { useLicence, usePendingActions } from "../../api/conspectQueries";
import { Badge } from "../ui/badge";
import { useConfigLock } from "./config-lock";
import {
  isLadderBannerLevel,
  useEnforcementSentence,
} from "./enforcement-copy";

/**
 * The header licence chip. Unclaimed → links to the Account screen so the
 * operator can claim the machine; claimed → links to the Licence screen and
 * shows the opaque tier (the chip is paired with text, never colour alone).
 */
export function LicenceChip(): JSX.Element | null {
  const licence = useLicence();
  if (licence.data === undefined) {
    return null;
  }
  const status = licence.data.licensed ? licence.data.status : null;
  if (status === null || status === undefined) {
    return (
      <Link
        to="/settings/account"
        className="inline-flex min-h-[44px] items-center rounded-md px-1"
      >
        <Badge variant="stale">
          <Trans>Unclaimed</Trans>
        </Badge>
      </Link>
    );
  }
  const variant = status.enforcement === "active" ? "live" : "stale";
  return (
    <Link
      to="/settings/licence"
      className="inline-flex min-h-[44px] items-center rounded-md px-1"
      aria-label={status.tier}
    >
      <Badge variant={variant}>{status.tier}</Badge>
    </Link>
  );
}

/**
 * The full-width ladder banner. Quiet (renders nothing) when the licence is
 * active or absent; an actionable callout at warning-or-worse, naming the
 * one-sentence reason + a link to the Licence screen for remediation. Pairs a
 * glyph with text (never colour alone — WCAG 1.4.1).
 */
export function LadderBanner(): JSX.Element | null {
  const licence = useLicence();
  const sentence = useEnforcementSentence();
  const level = licence.data?.status?.enforcement;
  if (level === undefined || !isLadderBannerLevel(level)) {
    return null;
  }
  return (
    <div
      data-testid="ladder-banner"
      role="alert"
      aria-live="polite"
      className="flex items-start gap-3 border-b border-s-4 border-amber-500 bg-card px-4 py-3"
    >
      <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
      <div className="min-w-0 flex-1 space-y-1">
        <p className="text-sm font-medium text-foreground">{sentence(level)}</p>
        <p className="text-sm text-muted-foreground">
          <Link to="/settings/licence" className="underline underline-offset-2">
            <Trans>Open the licence screen</Trans>
          </Link>
        </p>
      </div>
    </div>
  );
}

/**
 * The pending-action strip. Renders nothing when no actions are queued; when one
 * or more remote actions await the operator, a compact callout links to the
 * System Actions screen to review/cancel them.
 */
export function PendingActionStrip(): JSX.Element | null {
  const pending = usePendingActions();
  const queued = (pending.data ?? []).filter((a) => a.state === "pending");
  if (queued.length === 0) {
    return null;
  }
  return (
    <div
      data-testid="pending-action-strip"
      role="status"
      aria-live="polite"
      className="flex items-center justify-between gap-3 border-b bg-card px-4 py-2"
    >
      <p className="text-sm">
        <Trans>You have remote actions awaiting review.</Trans>
      </p>
      <Link
        to="/system/actions"
        className="inline-flex min-h-[44px] items-center text-sm underline underline-offset-2"
      >
        <Trans>Review actions</Trans>
      </Link>
    </div>
  );
}

/**
 * A full-width banner shown while the engine is config-locked by enforcement.
 * It explains why reconfiguration is disabled and links to the Licence screen.
 * Resource forms render read-only alongside it; the API also returns a
 * `409 config_locked` problem, so the lock holds even if the UI is bypassed.
 * Renders nothing when not locked.
 */
export function ConfigLockBanner(): JSX.Element | null {
  const { locked, level } = useConfigLock();
  const sentence = useEnforcementSentence();
  if (!locked) {
    return null;
  }
  return (
    <div
      data-testid="config-lock-banner"
      role="status"
      aria-live="polite"
      className="flex items-start gap-3 border-b border-s-4 border-amber-500 bg-card px-4 py-3"
    >
      <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
      <div className="min-w-0 flex-1 space-y-1">
        <p className="text-sm font-medium text-foreground">
          {level !== undefined ? (
            sentence(level)
          ) : (
            <Trans>Reconfiguration is locked by the licence state.</Trans>
          )}
        </p>
        <p className="text-sm text-muted-foreground">
          <Trans>
            Changes to sources, outputs, and layouts are read-only until the next
            licensing contact.
          </Trans>{" "}
          <Link to="/settings/licence" className="underline underline-offset-2">
            <Trans>Open the licence screen</Trans>
          </Link>
        </p>
      </div>
    </div>
  );
}

