# ADR-W012: i18n — Lingui v5 + ECMAScript Intl, client-localized errors

- **Status:** Proposed
- **Area:** Accessibility & Internationalization
- **Date:** 2026-06-02
- **Source:** [web/accessibility.md](../web/accessibility.md)

## Decision

Adopt Lingui v5 (@lingui/core/react runtime; cli + vite-plugin + swc-plugin dev) as the i18n framework, authoring all copy as ICU MessageFormat (plural/select via CLDR) with auto-generated content-hash IDs and <Trans> rich-text. The i18n library owns catalogs + lookup; the ECMAScript Intl API owns ALL value formatting (DateTimeFormat/NumberFormat/RelativeTimeFormat/PluralRules), memoized per (locale, options). Locale and timezone are independent axes — the multi-timezone clock instantiates one cached Intl.DateTimeFormat({timeZone}) per displayed zone; timecode structure stays app-controlled. RTL = dir on <html> (feature-detected getTextInfo() + static fallback) + CSS logical properties + Tailwind logical utilities, with the konva canvas mirrored by an explicit mirroredX = stageWidth-(x+width) transform. Locale is negotiated from navigator.languages (RFC 4647 lookup) with a persisted override and sent as Accept-Language. API errors are RFC 9457 problem+json localized client-side from a stable machine code/type, with server title/detail as fallback only. Localize UI chrome + value formatting only; never localize user/operator content (source names, overlay text, dB/fps/Mbps, timecode, IDs).

## Rationale

Lingui has the smallest runtime (~3 KB gz vs ~14 KB react-intl vs ~23 KB react-i18next) and compiles ICU to JS at build time (zero runtime parse) — material for a console re-rendering tiles/meters at high frequency — plus the best TS DX. Intl is CLDR-backed and native; hardcoded count===1?... and split('-') break in real locales. Client-side error localization keeps wording consistent with the UI, works offline, and allows interpolation.

## Alternatives considered

react-intl (runner-up: native ICU + portable JSON, but ~14 KB and runtime ICU parse — choose only if the org mandates portable ICU/translator familiarity); react-i18next (rejected: weakest TS ergonomics, ICU via plugin only); library-native formatters or hand-rolled date/number math (rejected: use Intl); server-rendered Accept-Language error text as primary (rejected: diverges from UI wording, breaks offline).

## Consequences

Greenfield setup: Vite/SWC macro plugin, extraction + pseudolocale in CI, TMS, lazy per-locale catalogs. dB/fps are not Intl units (format number + literal); getTextInfo() is not Baseline (Firefox) so needs a fallback map; the OpenAPI 3.1 error schema must expose a stable code/type (backend shape unverified). Bundle sizes and the Lingui/React 19/Vite 6 matrix must be pinned and smoke-tested.
