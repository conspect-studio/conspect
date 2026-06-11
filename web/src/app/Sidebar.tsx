// The left navigation rail. A <nav> landmark with NavLinks; the active route is
// marked with aria-current (not color alone).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { NavLink } from "react-router-dom";

import { cn } from "../lib/utils";
import { NAV_ITEMS } from "./navigation";

/** Props for {@link Sidebar}. */
export interface SidebarProps {
  /** Close the mobile drawer (no-op on desktop). */
  readonly onNavigate?: () => void;
}

/** The primary navigation rail. */
export function Sidebar({ onNavigate }: SidebarProps): JSX.Element {
  const { t } = useLingui();
  return (
    <nav aria-label={t`Primary`} className="flex h-full flex-col gap-1 p-3">
      <div className="px-2 pb-3 text-lg font-semibold tracking-tight">
        <Trans>Multiview</Trans>
      </div>
      <ul className="flex flex-col gap-1">
        {NAV_ITEMS.map(({ path, label, Icon }) => (
          <li key={path}>
            <NavLink
              to={path}
              end={path === "/"}
              onClick={onNavigate}
              className={({ isActive }): string =>
                cn(
                  "flex items-center gap-3 rounded-md px-3 py-2 text-sm font-medium transition-colors hover:bg-accent hover:text-accent-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                  isActive
                    ? "bg-accent text-accent-foreground"
                    : "text-muted-foreground",
                )
              }
            >
              {({ isActive }): JSX.Element => (
                <>
                  <Icon className="size-4 shrink-0" aria-hidden="true" />
                  <span>{label}</span>
                  {isActive ? <span className="sr-only">(current)</span> : null}
                </>
              )}
            </NavLink>
          </li>
        ))}
      </ul>
    </nav>
  );
}
