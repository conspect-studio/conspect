# Accessibility (WCAG 2.2 AA)

The Multiview management web app targets **WCAG 2.1 and 2.2 Level AA**. This document is the
conformance plan: the per-area approach, the hard surfaces (the react-konva layout editor and the
realtime multiviewer), and the testing/CI gate that holds the bar.

> **Stack note.** `/web` is currently a React 19 + Vite + TS scaffold; shadcn/ui (Radix), Tailwind
> v4, TanStack, react-konva and dnd-kit are layered in during implementation
> ([management-app.md](management-app.md)). All defaults quoted below (dnd-kit, Radix) must be
> re-verified against the **pinned versions** once a lockfile exists — the figures here reflect
> current upstream behaviour, not an installed dependency.

## Commitment

- New UI ships **conformant or it does not ship**: the CI a11y gate (below) blocks merges on
  detectable regressions.
- WCAG 2.2 over 2.1 adds AA SC **2.4.11 Focus Not Obscured (Minimum)**, **2.5.7 Dragging
  Movements**, **2.5.8 Target Size (Minimum)**, **3.3.8 Accessible Authentication (Minimum)**, plus
  A SC 3.2.6 and 3.3.7, and removes 4.1.1 Parsing. We treat all of these as in-scope.
- Automated tooling catches roughly **57% of issues *by volume*** (Deque, weighted by real-world
  frequency — **not** 57% of success criteria; criterion-level automatability is far lower).
  Manual screen-reader testing is therefore mandatory, not optional.

## Per-area plan

### Navigation, landmarks, structure

| SC | How we meet it |
|----|----------------|
| 1.3.1 Info & Relationships (A) | Semantic landmarks (`header`/`nav`/`main`/`aside`), one `h1` per route, ordered headings. |
| 2.4.1 Bypass Blocks (A) | Skip-to-content link as the first focusable element; named landmark regions. |
| 2.4.3 Focus Order (A) | DOM order matches reading order; route changes move focus to the new `h1`/main and announce the view. |
| 2.4.7 Focus Visible (AA) | Visible focus ring on every interactive element (never `outline:none` without a replacement); ≥3:1 ring contrast. |
| 2.4.11 Focus Not Obscured (AA) | Sticky header/toolbars use `scroll-margin-top` = header height so a focused element is never hidden behind them. |
| 3.2.3 Consistent Navigation (AA) | Shared shell (nav, command bar) identical across routes. |

### Forms (shadcn Form + Radix)

| SC | How we meet it |
|----|----------------|
| 1.3.1 / 4.1.2 (A) | Every control has a programmatic `<label>`; groups use `fieldset`/`legend` or `role=group` + `aria-labelledby`. |
| 3.3.1 Error Identification (A) | Errors surfaced in text, tied via `aria-describedby`, field marked `aria-invalid`. |
| 3.3.2 Labels or Instructions (A) | Persistent labels (no placeholder-as-label); format hints in described-by text. |
| 3.3.3 Error Suggestion (AA) | Validation messages say how to fix, not just that it failed. |
| 3.3.7 Redundant Entry (A) | Previously entered values pre-filled/selectable, not re-typed. |
| 3.3.8 Accessible Authentication (AA) | No cognitive-function test without an alternative; allow password-manager paste/autofill. |

Use shadcn's `Form` wrapper (RHF + `FormMessage`) so label/description/error wiring is consistent.
**Do not strip Radix's `aria-*`/`role` props when restyling** — shadcn copies Radix source in, so
careless edits silently remove the accessibility layer.

### Data tables (TanStack Table)

| SC | How we meet it |
|----|----------------|
| 1.3.1 (A) | Real `<table>` semantics; `<th scope="col|row">`; caption or `aria-label` naming the table. |
| 4.1.2 (A) | Sortable headers are `<button>`s inside `<th>`; **only the active column** carries `aria-sort` (TanStack does not toggle it — issue 2992 — so we set it ourselves). |
| 4.1.3 Status Messages (AA) | Sort/filter/page changes announced politely ("Sorted by Bitrate, descending; 42 rows"). |
| 2.5.8 Target Size (AA) | Row-action icon buttons ≥24×24 CSS px (or spacing exception). |

### Dialogs, menus, tooltips (Radix primitives)

Radix implements the WAI-ARIA APG patterns (focus trap + restore for `Dialog`/`AlertDialog`, roving
tabindex for menus, `aria-expanded`/`aria-controls`), satisfying **2.1.2 No Keyboard Trap**,
**2.4.3**, **4.1.2**. Radix explicitly does **not** guarantee WCAG conformance on its own — we must
supply accessible names (`DialogTitle`, `aria-label` on icon-only triggers) and meaningful content.
`Escape` closes; focus returns to the trigger.

## The canvas layout/template editor (hardest surface)

A `<canvas>` produces **no accessibility tree** — react-konva pixels are invisible to AT and a
canvas has no native focus ring. Konva drag is **pointer-only** with zero built-in a11y. The
architecture is therefore **two models, one source of truth**:

> The Konva canvas is a *presentational view* of a layout model (ordered cells: `id`, `label`,
> `x`, `y`, `w`, `h`, `z`, `rotation`). A fully-equivalent **non-canvas DOM editing path** drives
> the *same* model. Every edit possible by dragging is possible without it.

**Accessible editing path** — an **APG Grid**-pattern Cells list plus a per-cell **Inspector**:

- **Cells list** — single tab stop into the group; arrow keys move the active cell (roving tabindex
  on the active cell; `document.activeElement` is **never null** — on add, focus the new cell; on
  delete, focus a sibling). Reorder via dnd-kit (below).
- **Inspector** — numeric `x/y/w/h/z/rotation` inputs, `+`/`−` step buttons, and explicit
  **Bring to front / Send to back / Forward / Backward** buttons.

### SC 2.5.7 Dragging Movements (AA) — single-pointer, independent of keyboard

> "All functionality that uses a dragging movement … can be achieved by a single pointer without
> dragging." Per W3C, **keyboard equivalence does NOT satisfy 2.5.7** unless the equivalent path is
> also clickable/tappable — touchscreen users may have no keyboard. The "essential" exception
> (freehand drawing) does **not** apply to positioning tiles, so the editor gets no exemption.

The single-pointer alternative *is* the Inspector: numeric fields (W3C-accepted "text input for
numerical values"), `+`/`−` steppers ("adjacent up/down controls"), and z-order buttons. Palette →
tile placement uses **tap-source-then-tap-cell** (W3C "multi-step pointer interaction"), never
drag-only.

### SC 2.1.1 Keyboard (A) — grab/move/drop/cancel

On a focused cell: `Enter`/`Space` to **grab**; arrow keys **nudge** (1 px fine; `Shift`+arrow =
grid step); modifier+arrow **resizes** an edge; `Enter` to **drop**; `Escape` to **cancel and
restore** the pre-grab position; `Home`/`End`/`PageUp`/`PageDown` jump to edges; `F2`/`Enter` opens
the Inspector. We **draw a pseudo focus ring on the canvas** (canvas has none natively → 2.4.7). No
keyboard trap (2.1.2).

> **dnd-kit covers the DOM reorder/palette-drop path only**, not free-form canvas dragging.
> KeyboardSensor defaults (re-verify when pinned): activate `Space`/`Enter`, cancel `Escape`,
> end `Space`/`Enter`/`Tab`, **25 px** per arrow. 25 px is too coarse for broadcast placement — we
> supply a custom `coordinateGetter` (1 px fine / grid-step coarse). `useDraggable` sets
> `tabindex=0`, `role=button`, `aria-roledescription="draggable"`, `aria-describedby` → hidden
> instructions. We **translate** dnd-kit's `screenReaderInstructions` + `announcements` (ships
> English only) for i18n, and phrase by **position not index** ("position 2 of 9").

### Announcements (SC 4.1.3, AA)

Move/resize/reorder are status changes (no focus move) → **`role=status` / `aria-live=polite`**,
**throttled, announced on settle** (never per pixel): "Camera 3 moved to x 480, y 120; position 2
of 9." Reserve `role=alert`/assertive for genuine editor errors (overlap, out-of-bounds). Each
cell's accessible **name carries identity + state in text** (4.1.2; never color alone). Honour
`prefers-reduced-motion` on DragOverlay drop animation and canvas transitions.

## Realtime status, tally, alarms, meters

### Never color alone — SC 1.4.1 Use of Color (A)

Tally (red/green/amber) and alarm severity are a colorblindness trap. **Triple-encode every state:
color + shape/icon + text label**, and put the state in the element's accessible name. Baseline
hues use the CVD-safe **Wong/Okabe-Ito** palette (e.g. `#0072B2`, `#E69F00`, `#D55E00`,
`#009E73`) — but that is hue separation only; each swatch must still be **contrast-tested against
both dark and light backgrounds** (Wong yellow `#F0E442` fails on white).

| SC | How we meet it |
|----|----------------|
| 1.4.1 Use of Color (A) | color + icon/shape + text on every tally/alarm/meter state. |
| 1.4.11 Non-text Contrast (AA) | Tally borders, severity icons, focus rings, meter fill vs track all ≥3:1, both themes. |
| 1.4.3 Contrast Minimum (AA) | Status label text ≥4.5:1 (≥3:1 large), both themes. |

### aria-live strategy — SC 4.1.3 Status Messages (AA)

- **Pre-mount empty** `role=status` (polite) and `role=alert` (assertive) regions at startup and
  inject text after a tick — a region created/populated at the same moment usually does **not**
  announce. Drive them through one global announcer/message-bus, never recreating the node.
- **Polite for ~90%** of updates; **assertive only** for critical action-required alarm raises
  (overusing assertive implicates 2.2.4 Interruptions). Never move focus to a live region.
- **Debounce/coalesce** per-tile state (start ~1–2 s/tile — a *tunable*, not a WCAG figure; validate
  per screen reader) and batch storms into one summary; `aria-busy=true` while batching.
- **Alarm history** = `role=log` (polite, `aria-atomic=false`) so only new entries announce.
- Live announcements are transient — critical alarm state **also stays persistently visible** (card
  + log + accessible name).
- Connection lifecycle ("Reconnecting…", "Restored") announced politely; only critical loss
  assertive.

### Audio meters — silent gauges, not live streams

A high-rate meter must **not** stream over `aria-live` (that floods AT). Use native `<meter>` (best
support) or `role=meter` with `aria-valuemin/max/now` + `aria-valuetext` + an accessible name, on a
**focusable wrapper** so the value can be read on demand. `role=meter` is a structure role, **not** a
live region. Announce only **thresholds**: clip / over-0-dBFS **assertive**, prolonged silence /
audio-loss **polite**.

### Motion — SC 2.3.3 (AAA) honoured + SC 2.3.1 (A)

`prefers-reduced-motion: reduce` (via `matchMedia`/CSS) gates meter peak animation, alarm pulsing,
and tile transitions → static state. Pulse/blink is **never** the sole alarm signal. Nothing flashes
>3×/second (2.3.1, Level A, required). (Note: 2.3.3 is AAA — strongly recommended, but we do not
claim reduced-motion is *required* for AA.)

## Target size & contrast (global)

- **2.5.8 Target Size (AA):** pointer targets ≥**24×24 CSS px**, or the 24 px-circle **spacing
  exception**; dense multiviewer tile overlays rely on the spacing/essential exceptions with an
  equivalent full-size control in the Inspector.
- **1.4.3 / 1.4.11:** all text ≥4.5:1, UI/graphics ≥3:1, **verified in both dark and light themes**.
- **1.4.4 Resize Text / 1.4.10 Reflow (AA):** usable at 200% zoom and 320 CSS px width without loss.

## Testing plan & CI gate

| Layer | Tool | Catches |
|-------|------|---------|
| Lint | `eslint-plugin-jsx-a11y` | Static JSX a11y mistakes at author time. |
| Unit/component | `jest-axe` / `vitest-axe` on rendered components | Role/name/contrast violations per component. |
| E2E | Playwright + `@axe-core/playwright` | Page-level scans on real routes/flows. |
| Keyboard E2E | Playwright keyboard-only journeys | Editor grab/move/drop, dialog focus trap+restore, skip link, sort. |
| Manual SR | Screen-reader matrix (below) | The ~43% automation misses. |

**Manual screen-reader matrix** (the editor, multiviewer alarms/tally, meters, tables, dialogs):

| Screen reader | Browser | OS |
|---------------|---------|----|
| NVDA | Firefox + Chrome | Windows |
| JAWS | Chrome | Windows |
| VoiceOver | Safari | macOS |
| VoiceOver | Safari | iOS (touch — validates 2.5.7) |

**CI gate:** lint + axe component/E2E scans run on every PR and **fail the build** on new
violations; manual SR matrix runs per milestone and before release. Known-uncertain items to
validate live: `aria-busy` reliability (only JAWS suppresses busy content; NVDA/VoiceOver ignore),
debounce intervals, and `role=log` "new entries only" behaviour — these vary by SR and must be
tuned against real NVDA/JAWS/VoiceOver, not assumed.

## See also

- [Management app design](management-app.md)
- [Internationalization](internationalization.md)
- Decisions: [ADR index](../decisions/README.md)
