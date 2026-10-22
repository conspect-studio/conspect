# web/ — agent notes (the SPA)

React 19 + TypeScript + Vite; embedded into `multiview-control` via rust-embed (`cargo xtask build-web`).
Stack (conventions §8 — canonical): shadcn/ui (Radix + Tailwind v4), TanStack Query/Table, react-konva + dnd-kit.
API client is **generated** from the OpenAPI spec (`openapi-typescript`/`openapi-fetch`) — never hand-write fetch calls/types. Regenerate: `cargo xtask gen-openapi` then `npm --prefix web run generate:api`; realtime types: `cargo xtask gen-asyncapi` then `npm --prefix web run generate:events`. CI checks specs are fresh but never regenerates TS — commit both halves together.
User-facing strings go through Lingui; `i18n catalog freshness (lingui)` CI job fails on drift — run `npm --prefix web run i18n:extract -- --clean` then `... i18n:compile`, commit `src/locales/**` with the strings. Accessibility: WCAG 2.1 AA.
Commands: `npm --prefix web ci` · `... run dev` · `... run build` · `... run lint`.
Read first: [conventions §8](../docs/architecture/conventions.md).
