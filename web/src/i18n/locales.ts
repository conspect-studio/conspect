// Supported locales and locale negotiation (docs/web/internationalization.md).
//
// Negotiation uses RFC 4647 "lookup": match the user's ordered preferences
// against supported locales with fallback truncation (en-AU -> en). Direction
// is derived via Intl.Locale.getTextInfo() with a static RTL fallback because
// that API is not Baseline.

/** The locales the app ships catalogs for. */
export const SUPPORTED_LOCALES = ["en", "ar"] as const;

/** A supported locale tag. */
export type Locale = (typeof SUPPORTED_LOCALES)[number];

/** The fallback/source locale. */
export const DEFAULT_LOCALE: Locale = "en";

/** The key used to persist an explicit user locale choice. */
export const LOCALE_STORAGE_KEY = "multiview.locale";

/** Human-readable, self-named labels for the locale switcher. */
export const LOCALE_LABELS: Readonly<Record<Locale, string>> = {
  en: "English",
  ar: "العربية",
};

/** Static RTL allow-list used when `Intl.Locale` text-info is unavailable. */
const RTL_LANGUAGES: ReadonlySet<string> = new Set([
  "ar",
  "he",
  "fa",
  "ur",
  "ps",
  "syr",
  "dv",
  "ckb",
]);

function isLocale(value: string): value is Locale {
  return (SUPPORTED_LOCALES as readonly string[]).includes(value);
}

/**
 * Resolve a requested BCP 47 tag to a supported locale using RFC 4647 lookup
 * (progressive truncation of subtags), or `undefined` if none match.
 */
function lookup(tag: string): Locale | undefined {
  let candidate = tag.toLowerCase();
  for (;;) {
    if (isLocale(candidate)) {
      return candidate;
    }
    const lastDash = candidate.lastIndexOf("-");
    if (lastDash === -1) {
      return undefined;
    }
    candidate = candidate.slice(0, lastDash);
  }
}

/**
 * Negotiate the active locale from an explicit stored choice, then the ordered
 * browser preferences, falling back to {@link DEFAULT_LOCALE}.
 */
export function negotiateLocale(
  stored: string | null,
  preferences: readonly string[],
): Locale {
  if (stored !== null) {
    const explicit = lookup(stored);
    if (explicit !== undefined) {
      return explicit;
    }
  }
  for (const pref of preferences) {
    const match = lookup(pref);
    if (match !== undefined) {
      return match;
    }
  }
  return DEFAULT_LOCALE;
}

/** The writing direction of a locale. */
export type Direction = "ltr" | "rtl";

/**
 * Determine the writing direction for a locale, preferring the standard
 * `Intl.Locale` text-info shapes and falling back to a static RTL set.
 */
export function directionFor(locale: Locale): Direction {
  try {
    // The method form (getTextInfo) and the accessor form (textInfo) both exist
    // across engines and neither is Baseline; probe reflectively so we never
    // assert a shape the runtime may not provide.
    const intlLocale: object = new Intl.Locale(locale);
    const direction = readTextDirection(intlLocale);
    if (direction !== undefined) {
      return direction;
    }
  } catch {
    // Fall through to the static set below.
  }
  return RTL_LANGUAGES.has(locale) ? "rtl" : "ltr";
}

/** Read `direction` from either `Intl.Locale` text-info shape, reflectively. */
function readTextDirection(intlLocale: object): Direction | undefined {
  const getter: unknown = Reflect.get(intlLocale, "getTextInfo");
  const info: unknown =
    typeof getter === "function"
      ? Reflect.apply(getter, intlLocale, [])
      : Reflect.get(intlLocale, "textInfo");
  if (typeof info !== "object" || info === null) {
    return undefined;
  }
  const direction: unknown = Reflect.get(info, "direction");
  return direction === "rtl" || direction === "ltr" ? direction : undefined;
}
