import type { LinguiConfig } from "@lingui/conf";
import { formatter } from "@lingui/format-po";

// Lingui catalog config. Compile-time ICU: `lingui extract` scans `t`/`<Trans>`
// macros into per-locale PO catalogs; `lingui compile` emits JS catalogs that the
// app lazy-loads. `ar` is a stub locale that also exercises the RTL path.
// See docs/web/internationalization.md.
const config: LinguiConfig = {
  locales: ["en", "ar", "pseudo"],
  sourceLocale: "en",
  pseudoLocale: "pseudo",
  fallbackLocales: {
    default: "en",
  },
  catalogs: [
    {
      path: "<rootDir>/src/locales/{locale}/messages",
      include: ["src"],
    },
  ],
  // Lingui 6 removed the `format: "po"` string shorthand — the PO formatter is
  // now a separate package (`@lingui/format-po`) supplied via `formatter()`.
  // `lineNumbers: true` keeps the `#: src/...:NN` origin comments the committed
  // catalogs already carry, so `lingui extract` stays byte-stable.
  format: formatter({ lineNumbers: true }),
  // Emit compiled catalogs as TypeScript (`messages.ts`) keyed by the
  // content-hash message IDs the macro generates at runtime. The app imports
  // these compiled `.ts` catalogs directly (see src/i18n/I18nProvider.tsx);
  // they must be regenerated via `lingui compile` whenever the `.po` sources
  // change. The production build runs `lingui compile` before `vite build`
  // (see package.json) because the prod macro strips the source-string
  // fallback, so an empty/missing catalog would render the hash IDs.
  compileNamespace: "ts",
};

export default config;
