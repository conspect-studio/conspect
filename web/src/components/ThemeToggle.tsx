// Light/dark/system theme toggle. Icon-only buttons carry an accessible name.
import type { JSX } from "react";
import { useLingui } from "@lingui/react/macro";
import { Monitor, Moon, Sun } from "lucide-react";

import { useTheme } from "../theme/ThemeProvider";
import type { ThemePreference } from "../theme/ThemeProvider";
import { Button } from "./ui/button";

const ORDER: readonly ThemePreference[] = ["light", "dark", "system"];

/** Cycles light -> dark -> system; announces the current choice via the label. */
export function ThemeToggle(): JSX.Element {
  const { preference, setPreference } = useTheme();
  const { t } = useLingui();

  const next = ORDER[(ORDER.indexOf(preference) + 1) % ORDER.length] ?? "system";

  const label =
    preference === "light"
      ? t`Theme: light. Switch to dark.`
      : preference === "dark"
        ? t`Theme: dark. Switch to system.`
        : t`Theme: system. Switch to light.`;

  return (
    <Button
      variant="ghost"
      size="icon"
      aria-label={label}
      title={label}
      onClick={(): void => {
        setPreference(next);
      }}
    >
      {preference === "light" ? (
        <Sun aria-hidden="true" />
      ) : preference === "dark" ? (
        <Moon aria-hidden="true" />
      ) : (
        <Monitor aria-hidden="true" />
      )}
    </Button>
  );
}
