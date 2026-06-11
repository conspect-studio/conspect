# Multiview — management web app

> [!NOTE]
> **Early scaffold — implementation in progress.** This will be the real Multiview management app; the
> full design is in [`../docs/web/management-app.md`](../docs/web/management-app.md). Right now it is
> a minimal compiling skeleton being built out against that design.

The polished management console for Multiview. This is a **scaffold**: a minimal
React 19 + TypeScript + Vite app that builds today. The full stack described in
[`../docs/web/management-app.md`](../docs/web/management-app.md) is layered in
during implementation:

- **UI/design:** shadcn/ui (Radix + Tailwind v4), TanStack Query + TanStack Table
- **Layout editor:** react-konva (free-form canvas) + dnd-kit (accessible DnD)
- **API client:** generated from the OpenAPI 3.1 spec (`openapi-typescript` + `openapi-fetch`)
- **Realtime:** WebSocket client for live state + WHEP for preview (see [`../docs/api/realtime.md`](../docs/api/realtime.md))

## Develop

```sh
npm install
npm run dev      # http://localhost:5173 (proxies /api -> http://localhost:8080)
```

## Build

```sh
npm run build    # type-check + production build into web/dist
```

In production the built assets are embedded into the `multiview` binary via
`rust-embed` and served by `multiview-control` (single deployable). See
[`../docs/architecture/conventions.md`](../docs/architecture/conventions.md) §8.
