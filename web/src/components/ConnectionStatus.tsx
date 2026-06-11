// Header connection indicator. Status is conveyed by TEXT + a GLYPH (icon),
// never color alone (accessibility.md §1.4.1); transitions announce politely.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import { CheckCircle2, Loader2, PlugZap, XCircle } from "lucide-react";

import type { RealtimeStatus } from "../realtime/connection";
import { Badge } from "./ui/badge";
import type { BadgeProps } from "./ui/badge";

/** Props for {@link ConnectionStatus}. */
export interface ConnectionStatusProps {
  /** The live realtime connection status. */
  readonly status: RealtimeStatus;
}

interface Presentation {
  readonly variant: NonNullable<BadgeProps["variant"]>;
  readonly icon: JSX.Element;
  readonly label: JSX.Element;
}

function present(status: RealtimeStatus): Presentation {
  switch (status) {
    case "open":
      return {
        variant: "live",
        icon: <CheckCircle2 className="size-3.5" aria-hidden="true" />,
        label: <Trans>Connected</Trans>,
      };
    case "connecting":
      return {
        variant: "stale",
        icon: <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />,
        label: <Trans>Connecting</Trans>,
      };
    case "reconnecting":
      return {
        variant: "reconnecting",
        icon: <PlugZap className="size-3.5" aria-hidden="true" />,
        label: <Trans>Reconnecting</Trans>,
      };
    case "closed":
      return {
        variant: "nosignal",
        icon: <XCircle className="size-3.5" aria-hidden="true" />,
        label: <Trans>Disconnected</Trans>,
      };
  }
}

/** A compact, accessible engine-connection badge for the app header. */
export function ConnectionStatus({ status }: ConnectionStatusProps): JSX.Element {
  const { variant, icon, label } = present(status);
  return (
    <Badge variant={variant} role="status" aria-live="polite">
      {icon}
      <span>{label}</span>
    </Badge>
  );
}
