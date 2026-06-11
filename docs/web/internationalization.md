# Internationalization & Localization (i18n / l10n)

The Multiview management web app is **greenfield** for i18n: `/web` is a bare React 19 + Vite 6 + TS
scaffold (no i18n, Tailwind, shadcn, or konva installed yet), so this is designed in from the start
rather than retrofitted. This document fixes the library, formatting, workflow, multi-timezone +
timecode handling, RTL (including the canvas editor), locale negotiation, API error policy, and the
exact **what-is / what-isn't localized** boundary.

## Library: Lingui v5

**Adopt Lingui v5** (`@lingui/core` + `@lingui/react` runtime; `@lingui/cli`,
`@lingui/vite-plugin`, `@lingui/swc-plugin` dev). Wire the SWC macro plugin into
`@vitejs/plugin-react`, add `lingui()` to `vite.config.ts`, wrap the app in `<I18nProvider>`.

**Rationale (best fit for this realtime stack):**

- **Smallest runtime.** Lingui runtime ≈**3 KB gz** (`@lingui/core` ~2.05 KB + `@lingui/react`
  ~1.41 KB on the current v6 line) vs react-intl ≈**14 KB gz** vs react-i18next+i18next ≈**23 KB
  gz**. Sources agree on the ranking; exact KB are version-dependent — **confirm with a real bundle
  analysis** before quoting in a decision.
- **Compile-time ICU.** Lingui compiles ICU MessageFormat catalogs to **plain JS functions at build
  time** (zero runtime parse). react-intl / react-i18next parse ICU **at runtime** — a per-render
  CPU cost that matters for a console re-rendering tiles/meters at high frequency.
- **Best TS DX.** Macros (`t`, `<Trans>`, `useLingui`) with **auto-generated content-hash IDs**
  (engineers never hand-write keys) and native rich-text `<Trans>` tags (translators get whole
  sentences, not fragments).

**Runner-up:** FormatJS / react-intl (native ICU, `@formatjs/cli` `[contenthash:5]` auto IDs, AST
pre-compile) **only if** the org mandates portable ICU JSON + translator ICU familiarity. **Avoid
react-i18next** here (weakest TS ergonomics; ICU only via an optional plugin).

> **Hard rule.** The i18n library owns **catalogs + lookup**. The **ECMAScript `Intl` API** owns
> **all value formatting** (dates, numbers, relative time, plurals) — CLDR-backed and native. Never
> format values with a library-specific formatter or hand-rolled math.

## ICU MessageFormat (plurals & select)

Author every count/category-dependent string as ICU — never string concatenation:

```
{count, plural, one {# alarm} other {# alarms}}
{severity, select, critical {Critical} warning {Warning} other {Info}}
```

CLDR defines **six plural categories** (`zero/one/two/few/many/other`), used selectively per
language; a hardcoded `count === 1 ? singular : plural` is a **bug** in Polish/Russian/Arabic. The
library picks the form via CLDR rules (`Intl.PluralRules`). Use `selectordinal` for ordinals. Give
translators whole sentences via `<Trans>` rich-text tags rather than splitting around interpolated
values.

## Extraction & translation workflow

- `lingui extract` scans `t`/`<Trans>` macros → per-locale catalogs (PO default; JSON supported).
  `lingui compile` emits optimized JS catalogs. Content-hash IDs mean no hand-written keys.
- **Pseudolocalization** (`pseudoLocale`) in CI/preview surfaces untranslated strings, hardcoded
  copy, and text-expansion/truncation layout breakage **before** translation.
- **Lazy-load** the active locale's compiled catalog via dynamic import
  (`i18n.loadAndActivate`); switching locale code-splits in the new catalog. Only the active
  locale's catalog ships.
- Connect a **TMS** (Crowdin / Translation.io). Run extraction **in CI** and **fail on unexpected
  new untranslated keys**.

## Date/time formatting — `Intl.DateTimeFormat`

Each `Intl.DateTimeFormat` handles exactly **one `timeZone` per instance** — there is no multi-zone
formatter. **Locale and timezone are independent state axes**: locale governs style (12/24h,
separators, month names); timezone governs *which* wall-clock a given display shows.

```ts
new Intl.DateTimeFormat(locale, {
  timeZone: 'America/Los_Angeles',
  hour: '2-digit', minute: '2-digit', second: '2-digit',
  timeZoneName: 'short',
});
```

- **Memoize** one formatter **per displayed zone** — construction is costly and clocks tick every
  second.
- Validate zone IDs via `Intl.supportedValuesOf('timeZone')`.
- `timeZoneName: 'short' | 'shortOffset' | 'longGeneric'` for labels.

### Multi-timezone clock wall

Iterate the configured zones; each gets its **own cached** `Intl.DateTimeFormat({ timeZone })`. The
user locale formats all of them; the per-zone option chooses the wall-clock.

### Timecode overlays (HH:MM:SS:FF, drop-frame)

Format numerals/separators via `Intl` (locale-aware), but the **SMPTE structure is
app-controlled** — timecode is engineering data, not localized. The `:`/`;` drop-frame delimiter and
frame field stay app-defined.

## Number/level/bitrate formatting — `Intl.NumberFormat`

```ts
// bitrate — compound unit via -per-, SINGULAR ("bit-per-second", not "bits-")
new Intl.NumberFormat(locale, { style: 'unit', unit: 'kilobit-per-second', unitDisplay: 'short' });
```

- `notation: 'compact'` for bitrate chips; `style: 'percent'` for level meters; `signDisplay` for
  gain offsets; `maximumFractionDigits` for precision.
- **`Intl.RelativeTimeFormat(locale, { numeric: 'auto' })`** for "last seen 5 minutes ago" health
  strings.
- **dB is NOT a CLDR/Intl unit.** Format the number with `Intl` (`style: 'decimal'`, fixed fraction
  digits, `signDisplay: 'exceptZero'`) and append a **non-translated `dB` literal**. Same for
  `fps`/`frames`. Do **not** attempt `style:'unit', unit:'decibel'`.
- Validate `bit`/`byte` and the compound `*-per-second` identifiers against target engines
  (`Intl.supportedValuesOf('unit')`) — the simple-unit set is implementation-defined.
- Never localize numeric IDs, versions, or handles.

## RTL (right-to-left)

- Set `dir` on `<html>` from the active locale's script direction. Derive via
  `Intl.Locale.prototype.getTextInfo()` **with a fallback**: it is **not Baseline** (Firefox
  reported unsupported; older engines expose a `textInfo` *accessor* instead of the method).
  Feature-detect both shapes, then fall back to a static `locale → dir` RTL set
  (`ar/he/fa/ur/…`) or the `intl-locale-textinfo-polyfill`.
- **CSS logical properties** everywhere: `margin-inline-start/end`, `padding-inline-*`,
  `inset-inline-*`, `border-inline-*`, `text-align: start/end`. In Tailwind v4 prefer
  `ms-/me-/ps-/pe-/start-/end-` and the `rtl:`/`ltr:` variants over `ml-/mr-`.
- **Mirror** nav, back/forward, progress, chevrons. **Do NOT mirror** media transport controls,
  video tiles, numbers, timecode, or logos (mirroring these is a regression).
- Test under `dir=rtl` with an Arabic/Hebrew pseudolocale.

### The react-konva editor — mirror EXPLICITLY

Canvas does **not** inherit `dir=rtl` or CSS logical properties, and Konva `Text` does not run a full
Unicode bidi algorithm. For RTL, mirror cell x with `mirroredX = stageWidth - (x + width)` so labels
render upright — **preferable to `scaleX:-1`**, which flips glyphs. Prefer rendering chrome labels in
the **accessible DOM overlay** rather than inside canvas for bidi/Arabic text. The accessible
non-canvas editing path must itself be RTL-correct via logical properties — but the underlying cell
coordinates **stay in the LTR coordinate system** (do not localize coordinate semantics).

## Locale negotiation

- On first load, match ordered `navigator.languages` (BCP 47, most-preferred first) against
  supported locales using **RFC 4647 "lookup"** (single best tag with fallback truncation,
  `en-AU → en`).
- Parse region/script with **`Intl.Locale`**, never `locale.split('-')[1]`.
- **Persist an explicit user choice** that overrides the browser; keep it in `<html lang>` and
  update on change (SC 3.1.1 Language of Page, A).
- Send the resolved locale as **`Accept-Language`** on API requests, mirroring q-value ordering
  (`en-US,en;q=0.9`).
- Wrap embedded foreign-language phrases with `lang` on a span (SC 3.1.2 Language of Parts, AA).
  User content of indeterminate language uses `lang`/`dir="auto"` so AT pronounces/orders it
  correctly.

## API error localization — RFC 9457, client-side from a stable code

Model errors as **RFC 9457 problem+json**. The server **must** return a **stable machine
identifier** — a registered `type` URI and/or an extension `code`
(e.g. `"code":"OUTPUT_TRANSCODER_UNAVAILABLE"`). The **client maps that code to a localized ICU
message**. This keeps wording consistent with the rest of the UI, works offline/cached, and allows
interpolation. Human-readable `title`/`detail` (`Accept-Language` negotiated, server echoes
`Content-Language`) are a **fallback only**, for unknown codes. **Never parse `detail` for logic.**
For `about:blank`, `title` matches the HTTP status phrase. Since the client is generated from
**OpenAPI 3.1**, ensure the error schema exposes `code`/`type` so it is **typed**. (Whether the Rust
backend already emits this shape is unverified — the schema must be confirmed/shaped to provide a
stable identifier.)

## What IS / ISN'T localized

**LOCALIZE — UI chrome + value formatting:**

- Buttons, menus, labels, tooltips, help text, empty states.
- Validation + error messages (mapped from machine codes).
- **Status terms** — the *textual label* paired with a tally color ("Live", "Fault").
- Units/separators and **all** date / number / relative-time formatting.
- Clock & timecode **formatting** (numerals, separators, 12/24h) — even though the *values* are
  operator data.

**DO NOT LOCALIZE — user / operator content:**

- Source names, channel/output names, template names, hostnames.
- **Operator-authored overlay text.**
- Technical symbols/data: `dB`, `fps`, `Mbps`, **timecode structure**, version strings, IDs.

Render user content **verbatim** with `lang`/`dir="auto"` so AT and bidi handle it without
translating. **Enforce the boundary** with lint guidance + a reviewer checklist: user content is
**never** wrapped in `t`/`<Trans>`, and UI chrome is **never** hardcoded — both are the common review
failures.

## Pinning note

Lingui v5 + React 19 + Vite 6 + the SWC macro plugin is the documented modern path, but the precise
version-compatibility matrix should be **pinned and smoke-tested**; specifics drift between minor
releases. Bundle-size figures above are point-in-time — re-measure before they enter a decision doc.

## See also

- [Management app design](management-app.md)
- [Accessibility (WCAG 2.2 AA)](accessibility.md)
- Decisions: [ADR index](../decisions/README.md)
