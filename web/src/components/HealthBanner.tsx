// HealthBanner — the SA-0 global health-warning banner (ADR-0035).
//
// Subscribes to the engine's `alerts` realtime lane (via useHealth) and renders
// every ACTIVE health warning as an actionable callout: a severity GLYPH + TEXT
// (never colour alone — WCAG 1.4.1), the human-readable message, the monospace
// `code`, and the concrete `remediation` (the `graphics` / libvulkan fix). It
// renders NOTHING when there are no active warnings (no false alarm on a clean
// host). Mounted globally in the app shell, under <header>.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import { CircleAlert, ShieldAlert, TriangleAlert } from "lucide-react";

import type { HealthWarning, WarningSeverity } from "../realtime/useHealth";
import { useHealth } from "../realtime/useHealth";

interface SeverityPresentation {
  /** A short, translatable severity label (text carries the meaning). */
  readonly label: JSX.Element;
  /** A severity glyph (decorative; the label carries the meaning). */
  readonly icon: JSX.Element;
  /** A non-colour-dependent border accent (text + icon remain the signal). */
  readonly accent: string;
}

function severityPresentation(severity: WarningSeverity): SeverityPresentation {
  switch (severity) {
    case "critical":
      return {
        label: <Trans>Critical</Trans>,
        icon: <ShieldAlert className="size-4 shrink-0" aria-hidden="true" />,
        accent: "border-destructive",
      };
    case "warning":
      return {
        label: <Trans>Warning</Trans>,
        icon: <TriangleAlert className="size-4 shrink-0" aria-hidden="true" />,
        accent: "border-amber-500",
      };
    case "info":
      return {
        label: <Trans>Notice</Trans>,
        icon: <CircleAlert className="size-4 shrink-0" aria-hidden="true" />,
        accent: "border-sky-500",
      };
  }
}

/** One warning row: severity label + glyph, message, code, and remediation. */
function WarningItem({ warning }: { warning: HealthWarning }): JSX.Element {
  const present = severityPresentation(warning.severity);
  return (
    <div
      className={`flex items-start gap-3 border-s-4 bg-card px-4 py-3 ${present.accent}`}
    >
      <span className="mt-0.5 flex items-center gap-1.5 font-medium">
        {present.icon}
        <span>{present.label}</span>
      </span>
      <div className="min-w-0 flex-1 space-y-1">
        <p className="text-sm font-medium text-foreground">{warning.message}</p>
        <p className="text-sm text-muted-foreground">
          <span className="font-medium">
            <Trans>How to fix:</Trans>
          </span>{" "}
          {warning.remediation}
        </p>
        <p className="text-xs text-muted-foreground">
          <Trans>Code:</Trans>{" "}
          <code className="rounded bg-muted px-1 py-0.5 font-mono">
            {warning.code}
          </code>{" "}
          · <Trans>Subsystem:</Trans> {warning.subsystem}
        </p>
      </div>
    </div>
  );
}

/**
 * The global health-warning banner. Renders an alert region for every active
 * warning, or `null` when there are none (a clean host shows nothing).
 */
export function HealthBanner(): JSX.Element | null {
  const { warnings } = useHealth();
  if (warnings.length === 0) {
    return null;
  }
  return (
    <div role="alert" aria-live="polite" className="border-b">
      {warnings.map((warning) => (
        <WarningItem key={warning.code} warning={warning} />
      ))}
    </div>
  );
}
