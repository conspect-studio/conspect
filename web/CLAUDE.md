# web/ — agent notes (the SPA)

The Multiview management UI: **React 19 + TypeScript + Vite**. Built and embedded into
`multiview-control` via rust-embed (`cargo xtask build-web`).

Stack (conventions §8 — canonical):
- **shadcn/ui** (Radix + Tailwind v4); **TanStack Query / Table**.
- **react-konva** + **dnd-kit** for the layout editor (drag/resize tiles on the canvas).
- **API client is GENERATED from the OpenAPI spec** (`openapi-typescript` + `openapi-fetch`) —
  do **not** hand-write fetch calls or types. Regenerate after the spec changes
  (`cargo xtask gen-openapi`).
- Accessibility: **WCAG 2.1 AA**.

The API it consumes (from `multiview-control`): base `/api/v1`; long-running ops → `202` + operation
id with the result on the realtime stream; RFC 9457 errors; `ETag`/`If-Match`; WebSocket at
`/api/v1/ws`, SSE at `/api/v1/events`. The UI is best-effort and must tolerate dropped/conflated
realtime events (engine isolation, inv #10).

Commands: `npm --prefix web ci` · `npm --prefix web run dev` · `... run build` · `... run lint`.

Read first: [web-api-stack](../docs/research/web-api-stack.md) ·
[realtime-api](../docs/research/realtime-api.md) ·
[conventions §8](../docs/architecture/conventions.md) · ADR-W001..W008.
Map: [codebase-map](../docs/development/codebase-map.md).
