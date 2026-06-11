// Lingui + locale-state provider (docs/web/internationalization.md).
//
// - Negotiates the active locale (explicit choice > navigator.languages > en).
// - Lazy-loads + activates the compiled catalog for the active locale.
// - Reflects locale + direction onto <html lang>/<html dir> (SC 3.1.1).
// - Exposes `useLocale()` for the switcher; `useActiveLocale()` for formatters.
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
} from "react";
import type { JSX, ReactNode } from "react";
import { i18n } from "@lingui/core";
import type { Messages } from "@lingui/core";
import { I18nProvider as LinguiProvider } from "@lingui/react";

import {
  DEFAULT_LOCALE,
  LOCALE_STORAGE_KEY,
  directionFor,
  negotiateLocale,
} from "./locales";
import type { Direction, Locale } from "./locales";
import { messages as enMessages } from "../locales/en/messages";

/** The locale context value exposed to the app. */
export interface LocaleContextValue {
  /** The currently active locale. */
  readonly locale: Locale;
  /** The active writing direction. */
  readonly direction: Direction;
  /** Switch the active locale and persist the choice. */
  readonly setLocale: (locale: Locale) => void;
}

const LocaleContext = createContext<LocaleContextValue | null>(null);

// Load the source (en) catalog synchronously and activate it so the very first
// paint is translated; other locales are code-split and imported on demand. The
// loader map keeps each import statically typed (no template-literal `any`).
const loadedLocales = new Set<Locale>(["en"]);
i18n.load("en", enMessages);
i18n.activate(DEFAULT_LOCALE);

// The source locale (`en`) is bundled + pre-loaded above, so it has no entry
// here; every other locale is code-split behind a dynamic import. Each loader is
// statically typed (no template-literal `any`).
type CatalogLoader = () => Promise<{ readonly messages: Messages }>;

const CATALOG_LOADERS: Readonly<Partial<Record<Locale, CatalogLoader>>> = {
  ar: () => import("../locales/ar/messages"),
};

async function activateLocale(locale: Locale): Promise<void> {
  if (!loadedLocales.has(locale)) {
    const loader = CATALOG_LOADERS[locale];
    if (loader !== undefined) {
      const mod = await loader();
      i18n.load(locale, mod.messages);
    }
    loadedLocales.add(locale);
  }
  i18n.activate(locale);
}

function readStored(): string | null {
  try {
    return window.localStorage.getItem(LOCALE_STORAGE_KEY);
  } catch {
    return null;
  }
}

function persist(locale: Locale): void {
  try {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, locale);
  } catch {
    // Persistence is best-effort; ignore storage failures (private mode, etc.).
  }
}

/** Provider that activates Lingui and manages locale + direction. */
export function I18nProvider({ children }: { readonly children: ReactNode }): JSX.Element {
  const initial = useMemo<Locale>(
    () => negotiateLocale(readStored(), [...navigator.languages]),
    [],
  );
  const [locale, setLocaleState] = useState<Locale>(initial);
  const direction = useMemo<Direction>(() => directionFor(locale), [locale]);

  useEffect(() => {
    let cancelled = false;
    void activateLocale(locale).then(() => {
      if (cancelled) {
        return;
      }
      // i18n.activate triggers a Lingui re-render via the provider key below.
    });
    return (): void => {
      cancelled = true;
    };
  }, [locale]);

  useEffect(() => {
    document.documentElement.lang = locale;
    document.documentElement.dir = direction;
  }, [locale, direction]);

  const setLocale = useCallback((next: Locale): void => {
    persist(next);
    setLocaleState(next);
  }, []);

  const value = useMemo<LocaleContextValue>(
    () => ({ locale, direction, setLocale }),
    [locale, direction, setLocale],
  );

  return (
    <LocaleContext.Provider value={value}>
      <LinguiProvider i18n={i18n}>{children}</LinguiProvider>
    </LocaleContext.Provider>
  );
}

/** Access the locale context (throws if used outside the provider). */
export function useLocale(): LocaleContextValue {
  const ctx = useContext(LocaleContext);
  if (ctx === null) {
    throw new Error("useLocale must be used within an I18nProvider");
  }
  return ctx;
}

/** Convenience hook returning just the active locale tag for formatters. */
export function useActiveLocale(): Locale {
  return useLocale().locale;
}
