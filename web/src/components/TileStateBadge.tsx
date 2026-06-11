// Tile-state badge: color + glyph + text label, with the state in the
// accessible name (accessibility.md §1.4.1 — never color alone).
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import { CircleDot, CircleSlash, RefreshCw, TriangleAlert } from "lucide-react";

import type { TileState } from "../realtime/envelope";
import { Badge } from "./ui/badge";
import type { BadgeProps } from "./ui/badge";

/** Props for {@link TileStateBadge}. */
export interface TileStateBadgeProps {
  /** The tile lifecycle state. */
  readonly state: TileState;
}

interface Presentation {
  readonly variant: NonNullable<BadgeProps["variant"]>;
  readonly icon: JSX.Element;
  readonly label: JSX.Element;
}

function present(state: TileState): Presentation {
  switch (state) {
    case "LIVE":
      return {
        variant: "live",
        icon: <CircleDot className="size-3.5" aria-hidden="true" />,
        label: <Trans>Live</Trans>,
      };
    case "STALE":
      return {
        variant: "stale",
        icon: <TriangleAlert className="size-3.5" aria-hidden="true" />,
        label: <Trans>Stale</Trans>,
      };
    case "RECONNECTING":
      return {
        variant: "reconnecting",
        icon: <RefreshCw className="size-3.5" aria-hidden="true" />,
        label: <Trans>Reconnecting</Trans>,
      };
    case "NO_SIGNAL":
      return {
        variant: "nosignal",
        icon: <CircleSlash className="size-3.5" aria-hidden="true" />,
        label: <Trans>No signal</Trans>,
      };
  }
}

/** A tile-state pill carrying icon + text. */
export function TileStateBadge({ state }: TileStateBadgeProps): JSX.Element {
  const { variant, icon, label } = present(state);
  return (
    <Badge variant={variant}>
      {icon}
      <span>{label}</span>
    </Badge>
  );
}
