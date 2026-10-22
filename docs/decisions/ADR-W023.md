# ADR-W023: SPA WebRTC preview player and management UI

- **Status:** Proposed
- **Area:** Web/API stack · SPA preview transport, source/output management, in-app docs
- **Date:** 2026-06-10
- **Source:** [webrtc](../research/webrtc.md) §7.3–§7.4; consumes ADR-0048 (transport endpoint),
  ADR-T014 (WHIP ingest), ADR-0049 (WebRTC outputs), ADR-P006 (WHEP preview completion); builds on
  ADR-W015 (typed forms + round-trip rules) and ADR-W016 (in-app docs system)

## Context

The SPA's only live-pixel path is a ~1 fps JPEG poll: a local `usePreviewUrl` hook in
`web/src/pages/MonitoringPage.tsx` fetches `/api/v1/preview/program.jpg` and
`/preview/inputs/{id}.jpg` with the stored bearer (an `<img>` cannot send `Authorization`) and
swaps object URLs. Those endpoints are **not in the OpenAPI document**, so the fallback path is
hand-written and untyped — against the generated-client rule (conventions §8). With ADR-P006 the
control plane serves real sub-second WHEP previews (program/input/output scopes, program audio,
honest fidelity labels), and ADR-T014/ADR-0049 add three resource kinds the UI cannot yet author:
the `webrtc` source and the `webrtc`/`whip_push` outputs. The SPA must consume all of it without
regressing on builds where WebRTC is compiled out (`webrtc-native` absent) or the encoder ladder is
empty — the UI must *know*, not guess, which transport a given deployment offers.

## Decision

1. **`<WhepPlayer>` — a small, fully testable WHEP client** (new `web/src/preview/` module; no
   third-party WHEP library). Design points, each load-bearing:
   - **Injected `RTCPeerConnection` factory.** The component takes a `pcFactory` prop (default
     `(cfg) => new RTCPeerConnection(cfg)`). jsdom has no `RTCPeerConnection`; injection lets
     vitest drive a scripted fake through the full negotiate/connect/fail state space without a
     browser, and keeps the component free of module-level globals.
   - **`recvonly` transceivers** for video **and** audio added before `createOffer()`, so the
     offer always carries both m-lines (the server answers `inactive` for audio-less scopes).
   - **`muted` + `playsInline` set on the `<video>` element before `play()`** — required for
     autoplay policies (Safari in particular rejects unmuted autoplay and fullscreens non-inline
     video on iOS). Audio is opt-in via an explicit unmute control, never automatic.
   - **POST on ICE-gathering-complete OR a 2 s timeout**, whichever first. The endpoint is
     vanilla-ICE (ADR-0048: no trickle, PATCH is `405`), so the client must send a complete offer;
     with no `iceServers` configured host gathering completes in milliseconds, and the timeout is
     a belt-and-braces cap, not the expected path. The offer is POSTed as `application/sdp` with
     the stored control-plane bearer.
   - **Relative `Location` resolved against the post-redirect URL**:
     `new URL(location, response.url)` — never against the page origin or the pre-redirect
     request URL (RFC 9725 servers commonly return relative session URIs, and the POST may have
     been redirected).
   - **Teardown DELETEs the session URL with `fetch(url, { method: 'DELETE', keepalive: true })`**
     on unmount **and** on `pagehide`, attaching the same `Authorization` header the
     session-creating POST used — session DELETE requires the same credential class as its POST.
     `fetch` with `keepalive` *can* carry headers; `sendBeacon` cannot, which is exactly why
     keepalive fetch is the teardown mechanism. Tab closes and navigations thus release server
     sessions promptly instead of waiting for the ADR-0048 idle GC.
2. **Transport selection: capabilities probe, then a WHEP→JPEG fallback ladder.** A
   `<PreviewSurface>` wrapper owns the ladder; mounts render it, not `<WhepPlayer>` directly.
   - **Probe first.** `GET /api/v1/preview/capabilities` (ADR-P006 move 6: `{ webrtc, scopes: {...},
     fallback: "jpeg" }`), fetched once via TanStack Query and cached. `webrtc: false` (or the
     scope absent) → straight to the JPEG poll, **no badge** — JPEG is then the deployment's
     honest primary, not a degradation.
   - **Fallback triggers** when WHEP is attempted: (a) non-2xx session POST (including `503`
     capacity, whose problem body carries a fallback hint); (b)
     `connectionState === "failed"`; (c) a `getStats()` watchdog — `inbound-rtp`
     `bytesReceived` unchanged for ~6 s after connect ⇒ media-path stall (catches the
     silent-blackness failure mode that `connectionState` never reports).
   - **Honest fallback badge.** When the ladder degrades, the surface shows a visible
     "Fallback — still preview" badge (brief §7.4 honesty rule: never present a 1 fps poll as the
     live player), alongside the existing fidelity labelling from ADR-P006
     (`RealEncodedOutput` vs `PreEncodeCanvasApprox`).
   - **Backoff retry.** A failed WHEP attempt is retried with exponential backoff on the next
     mount or explicit user action (a "retry live preview" affordance on the badge) — never a
     hot reconnect loop against a struggling server.
   - **The JPEG rung is the existing poll, extracted.** `usePreviewUrl` moves out of
     `MonitoringPage.tsx` into a reusable `useJpegPreview` hook in `web/src/preview/`, unchanged
     in behaviour (bearer fetch → object URL → revoke), consumed by the ladder and by any
     surface on a no-WebRTC build.
   - **LL-HLS is explicitly NOT a rung in v1.** The ladder ships WHEP→JPEG; an LL-HLS middle
     rung (hls.js, ~3 s latency, proxy-friendly) is a separate work-schedule item, recorded
     there — adding it later extends `<PreviewSurface>` without changing this contract.
3. **JPEG endpoints enter the OpenAPI document; CORS exposes the headers the player reads.**
   The untyped snapshot routes (`/api/v1/preview/program.jpg`, `/preview/inputs`,
   `/preview/inputs/{id}.jpg` in `crates/multiview-control/src/routes/preview.rs`) gain utoipa
   registrations (triple registration + the route-completeness assertion in
   `crates/multiview-control/src/openapi.rs` — the `ApiDoc::rest_routes` vs mounted-router
   parity test in `tests/openapi.rs`), so the fallback rung and the new capabilities
   probe are part of the typed, generated client surface like everything else. The fallback
   token is the single literal `"jpeg"` (capabilities document and the `503` problem-body hint
   alike); the shipped `ws-jpeg` literal in `routes/preview.rs` is renamed to match in this
   push — no consumer exists yet, so the rename is safe and the route tests update with it.
   The router's configurable `CorsLayer` (brief §7.4; `webrtc.cors_allow_origins`, default
   `"*"`, applied only to the media-signaling routes — WHIP/WHEP/preview-WHEP/capabilities)
   sets `Access-Control-Allow-Headers: authorization, content-type` and
   `Access-Control-Expose-Headers: location, link` — without the exposed `location` a
   cross-origin player (Vite dev server, external dashboards) cannot read the WHEP session
   `Location` and silently leaks sessions. Preflight `OPTIONS` is unauthenticated by browser
   construction.
4. **Mount points.** Four surfaces, all via `<PreviewSurface>`:
   - **MonitoringPage program card** (replacing the bare `<img>` program path);
   - **input click-to-focus dialog** — input thumbnails stay JPEG (cheap, many), clicking opens
     a dialog with the live WHEP player for that input scope (FocusGate-capped server-side,
     ADR-P003/P006);
   - a **new outputs preview section** on MonitoringPage for `webrtc` outputs and
     WebRTC-compatible renditions (the `RealEncodedOutput` path, ADR-P006);
   - a **LayoutEditorPage live-preview toggle** — off by default (the editor is a Konva canvas;
     the live program behind it is an aid, not the default editing surface).
5. **Forms for the three new kinds, per the ADR-W015 round-trip rules.**
   - `SOURCE_KINDS`/`OutputKind`/`OUTPUT_KINDS` in `web/src/resources/types.ts` gain `webrtc`
     (source) and `webrtc` + `whip-push` (outputs; display kind `whip-push` ↔ wire tag
     `whip_push` via `outputWireKind` in `web/src/resources/forms.ts`, the `ll-hls`/`ll_hls`
     precedent).
   - The `webrtc` **source** form (`sourceFormToBody`/`sourceFormFromRecord`) manages
     `token` and `audio` (ADR-T014 schema) and — because the source is *published to*, not
     *pulled from* — displays the **derived WHIP URL** (`POST /api/v1/whip/{source_id}`,
     read-only, with a copy button) and the token (masked, copy button) so an operator can
     configure a publisher without reading docs. There is no `url` field to author; the
     locator column shows the derived endpoint. Tokens are plaintext `token` config values in
     v1, following the existing config-secret posture (returned to authorized readers, present
     in config export); they migrate together with url-embedded stream keys if/when a
     `secret_ref` indirection lands.
   - The `webrtc` and `whip_push` **output** forms manage the ADR-0049 fields
     (`max_viewers`/`token`/`codec`/`audio`/`gpu_pin` for `webrtc`; `url`/`token`/`codec`/
     `audio`/`gpu_pin` for `whip_push`); the `webrtc` output likewise displays its derived WHEP
     URL (`POST /api/v1/whep/{output_id}`). The `webrtc` output form states the token
     semantics explicitly: with no `token`, viewing requires an API key with View scope (View
     suffices — the session is read-shaped), never anonymous; with a `token`, that bearer
     alone admits a viewer. All forms follow the managed-keys rule: unmanaged
     body fields are preserved verbatim across an edit, `parseSourceFormKind`/
     `parseOutputFormKind` refuse unknown kinds (`editable: false` renders read-only), and
     validation mirrors the server's 422 schema (ADR-W015) — including surfacing the ADR-0049
     config-time rule that a `webrtc` output forces B-frames-off H.264 on its rendition.
6. **In-app docs (ADR-W016 system) and i18n.** The transports concept article
   (`web/src/pages/docs/concepts/TransportsPage.tsx`) gains WHIP-ingest and WebRTC-output/WHEP
   sections; the glossary (`GlossaryPage.tsx`) gains WHIP, WHEP, ICE, and SDP entries; the
   latency article (`LatencyPage.tsx`) gains a WebRTC row (sub-second, vs LL-HLS/HLS/JPEG). All
   register through `web/src/docs/registry.ts` (sections + keywords ⇒ searchable + linkable by
   construction), the docs nav (`web/src/pages/docs/docsNav.tsx`), the lazy route loaders, and —
   where an existing anchor is renamed — the registry's anchor-redirect map. New form fields
   carry `HelpLink`s to these anchors. All strings are Lingui-wrapped; `lingui extract` +
   `lingui compile` run for **en/ar/pseudo** and the compiled catalogues
   (`web/src/locales/*/messages.ts`, already tracked) are committed in the same push.
7. **Testing.** Three tiers, matching the repo's ladder (brief §10 / ADR-P006):
   - **vitest (jsdom):** `<WhepPlayer>` against the injected PC-factory fake (gathering states,
     offer/answer, `connectionState` transitions, stats-stall injection) with **MSW** handling
     the WHEP POST/DELETE (201 + `Location`, relative-Location resolution, 4xx/503, redirect);
     ladder tests assert probe-gated selection, each fallback trigger, the badge, and the
     teardown DELETE (including the `pagehide` path).
   - **Playwright e2e** (`web/e2e/`, real Chromium against the preview build): the fallback path
     end-to-end — capabilities up, WHEP route black-holed ⇒ JPEG rung + badge render; per the
     standing rule, browser-only behaviour is never trusted to jsdom.
   - **Hardware validation on the GPU test box (gpu-test-box):** real Chrome + Firefox against
     a `webrtc-native` build
     playing live program/input/output WHEP (Firefox explicitly, per the str0m 0.16.2 RTX note
     in ADR-0048), recorded with the usual evidence discipline.

## Rationale

A WHEP client is ~150 lines of protocol; a dependency would still need the factory-injection seam
for jsdom and would bury the four details that actually break in the field (gathering-complete
POST, relative `Location`, autoplay attributes, keepalive DELETE). The probe-first ladder keeps the
UI honest on every build profile: WebRTC-less builds get the JPEG path presented as primary, not as
a permanently-degraded player. Extracting (not rewriting) `usePreviewUrl` keeps the proven fallback
byte-identical in behaviour. Typing the JPEG endpoints closes the last hand-written fetch surface
in the preview path and is a precondition for the generated-client rule to hold. Forms reuse the
ADR-W015 machinery wholesale because the round-trip/preservation/unknown-kind rules are exactly the
properties that stop a UI from corrupting authored config.

## Alternatives considered

- **A WHEP client library** (rejected: the protocol surface is tiny, the libraries assume trickle
  ICE and `iceServers` we deliberately don't use, and none expose a PC-factory seam for jsdom).
- **LL-HLS as the v1 middle rung** (rejected for v1 — separate schedule item: it needs hls.js, the
  packager tap, and its own latency badge; the ladder is designed so it slots in without API
  change).
- **`<img>`-style token-in-query for WHEP** (rejected: WHEP is a `fetch` POST, so the bearer header
  works; query-string credentials would leak into logs for no gain).
- **Autoplay with audio** (rejected: blocked by every modern autoplay policy; muted-with-unmute is
  the only behaviour that works everywhere, Safari included).
- **WebSocket-pushed JPEG fallback** instead of polling (rejected for v1: the poll exists, is
  bounded, and is already isolation-proven; a push channel adds an engine-adjacent surface for a
  rung whose job is to be boring).

## Consequences

`web/src/preview/` becomes the single preview-transport module (`WhepPlayer`, `PreviewSurface`,
`useWhepSession`, `useJpegPreview`, capabilities query); `MonitoringPage.tsx` sheds its local hook.
The generated API types regenerate (capabilities + JPEG + WHIP/WHEP session routes), so
`cargo xtask gen-openapi` ordering matters in the push. Operators on no-WebRTC builds see no
behaviour change. The outputs page gains kinds that are *served by* Multiview (`webrtc`) next to
kinds that *push* (`whip_push`) — the forms' derived-URL display carries that distinction. The
docs registry, nav, and redirect maps grow (enforced by the existing registry unit tests); the
i18n catalogues grow for three locales. Playwright e2e gains its first network-failure-injection
spec, a pattern future transport work reuses. Session hygiene depends on `keepalive` DELETE being
best-effort only — the server-side idle GC (ADR-0048) remains the authoritative reaper.
