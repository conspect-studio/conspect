// Light/dark theme provider. Persists an explicit choice and otherwise follows
// the OS preference; toggles the `.dark` class for Tailwind tokens (index.css).
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
} from "react";
import type { JSX, ReactNode } from "react";

/** The user-selectable theme preference. */
export type ThemePreference = "light" | "dark" | "system";

/** The theme context value. */
export interface ThemeContextValue {
  /** The stored preference (may be `system`). */
  readonly preference: ThemePreference;
  /** The effective resolved theme actually applied. */
  readonly resolved: "light" | "dark";
  /** Update the preference (persisted). */
  readonly setPreference: (preference: ThemePreference) => void;
}

const ThemeContext = createContext<ThemeContextValue | null>(null);

const THEME_STORAGE_KEY = "multiview.theme";

function readStored(): ThemePreference {
  try {
    const value = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (value === "light" || value === "dark" || value === "system") {
      return value;
    }
  } catch {
    // Ignore storage failures.
  }
  return "system";
}

function systemPrefersDark(): boolean {
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

/** Provider that applies the resolved theme to `<html>`. */
export function ThemeProvider({ children }: { readonly children: ReactNode }): JSX.Element {
  const [preference, setPreferenceState] = useState<ThemePreference>(readStored);
  const [systemDark, setSystemDark] = useState<boolean>(systemPrefersDark);

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (event: MediaQueryListEvent): void => {
      setSystemDark(event.matches);
    };
    media.addEventListener("change", onChange);
    return (): void => {
      media.removeEventListener("change", onChange);
    };
  }, []);

  const resolved: "light" | "dark" =
    preference === "system" ? (systemDark ? "dark" : "light") : preference;

  useEffect(() => {
    const root = document.documentElement;
    root.classList.toggle("dark", resolved === "dark");
  }, [resolved]);

  const setPreference = useCallback((next: ThemePreference): void => {
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, next);
    } catch {
      // Persistence is best-effort.
    }
    setPreferenceState(next);
  }, []);

  const value = useMemo<ThemeContextValue>(
    () => ({ preference, resolved, setPreference }),
    [preference, resolved, setPreference],
  );

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

/** Access the theme context (throws if used outside the provider). */
export function useTheme(): ThemeContextValue {
  const ctx = useContext(ThemeContext);
  if (ctx === null) {
    throw new Error("useTheme must be used within a ThemeProvider");
  }
  return ctx;
}
