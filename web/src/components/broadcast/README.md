# broadcast/ — broadcast UI component library

Production React/TS components distilled from the approved `broadcast-ui`
prototype (design round 4, correction round 4 — reviews:
`production-component-regressions.md`, `correction-4-review-comfort.md`,
`correction-4-review-safety.md`). The prototype's verified improvements
carry forward; its known residuals are corrected here, not copied.

**Scope of this library:** presentation-only, fully controlled, typed props.
No fetch, no `localStorage`, no timers of its own, no simulated data, no new
dependencies. Every state transition belongs to the caller's realtime ledger;
every string goes through Lingui (`Trans` / `t`).

## Exports (barrel: `src/components/broadcast/index.ts`)

| Export | Kind | Purpose |
|---|---|---|
| `TouchAction` | component | Minimum-48px action key with its own pending spinner and a visible disabled reason. |
| `OperationFeedback` | component | Reserved, stable feedback + Undo region for one correlated operation. |
| `ProducerStatus` | component | Output-producer line: state, real tick age, tick source; management chip separate. |
| `TaskWorkspace` | component | Responsive task card: title, always-visible compact status, primary content, collapsible details. |
| `AssetDurationLabel` | component | Per-asset duration + precision with honest unknown/unmeasured states. |
| `formatAssetDuration`, `describeGrid`, `describeDurationPrecision`, `groupThousands` | pure functions | Exact-rational duration math (audio samples, video NDF frames). |
| `deriveProducerDisplay` | pure function | The producer verdict derivation (tick-truth table below). |

## Contracts (pinned by the colocated tests)

### TouchAction — 48px, own spinner, disabled reason
- `h-12 min-w-12` (48×48 floor) on every variant.
- `pending`: button disabled, `aria-busy`, spinner painted INSIDE the
  button's own box (`absolute inset-0` child) — it can never cover a
  neighbouring key or inflate `scrollWidth` (comfort N7).
- `disabled` + `disabledReason`: the reason is visible text below the key,
  linked with `aria-describedby` (and `title`), so a blocked control is
  never a mystery.

### OperationFeedback — accepted ≠ applied; timeout ≠ failed
- The region is **reserved**: idle, pending and applied all keep the same
  `min-h-24` slot with no `overflow-hidden` and no fixed height, so the
  feedback can never clip its own Undo key (the prototype's 68px-slot defect)
  and can never shift the controls around it when a phase appears.
- `pending` copy says *accepted — waiting for the applied event*; `applied`
  says *applied*. Never both.
- `unknown` says *timed out — outcome unknown* and is **not dismissable**
  here — only the caller's reconciliation may clear it. The component holds
  **no timer**, so a pending operation can never age itself into a false
  "failed" (the obsolete round-2 behaviour).
- `failed` / `cancelled` say *nothing was changed* and persist until
  dismissed (`onDismiss` renders the Dismiss key).
- The `correlationId` is displayed on every phase that has one.
- Undo (`onUndo`) renders a 48px `TouchAction` whose label wraps
  (`max-w-full whitespace-normal`) instead of clipping at 320–390px;
  `undoPending` shows the undo's own spinner.
- Long feedback lines wrap (`break-words`, `min-w-0`): the pending line can
  never widen a 320px document (comfort §"320 navigation").

### ProducerStatus — ticks are the only truth
Derivation (`deriveProducerDisplay`, default `staleAfterMs` 3000):
1. `failed` / `stopped` report their own state.
2. No observed tick → **Unknown** ("No output tick observed") — even if the
   state enum claims `producing`.
3. A tick in the future → **Unknown** (clock skew; age unverifiable).
4. Tick age ≥ `staleAfterMs` → **Frozen / stale** (with the real age).
5. Fresh tick + `producing` → **Producing** (with the real age).
6. Fresh tick + unreported state → **Unknown** ("Producer state not
   reported") — a tick alone is not a state claim.

The management connection renders as its own chip (`managementLabel`) and
**never** feeds the verdict — a connected management plane cannot make a
stopped, failed or unknown producer read as producing (safety criteria 7).
Age is measured against the caller's `now` (injectable; default is the shared
ticking clock in `producerNow.ts`, which keeps renders pure per
`react-hooks/purity` — same pattern as `src/devices/lastSeen.tsx`).
The line wraps (`min-w-0 break-words`): the round-3 S2 clip at 390px cannot
recur.

### TaskWorkspace — task first, detail behind a disclosure
- Title, compact `status` and primary `children` are always in flow — status
  is never swallowed by the disclosure.
- With `details`: on narrow viewports (<768px) the detail hides behind a
  48px Details key (`aria-expanded`/`aria-controls`); on wide viewports the
  detail is in flow and the key is hidden (`hidden md:block` / `md:hidden`).
- Fully controlled: `detailsOpen` + `onDetailsToggle` are the caller's state.

### Duration — audio never INVALID, video exactly rational
`formatAssetDuration(asset, parent?)`:
- Audio grids format through the sample grid: elapsed clock `HH:MM:SS.mmm`
  (floored), boundary `{kind:"samples", index, sampleRateHz}`. A valid
  48 kHz asset can never render INVALID (comfort §N8).
- Video grids are exact rationals; NDF timecode `HH:MM:SS:FF` counts
  `ceil(n/d)` frames per second; the boundary keeps the integer index plus
  the rational pair `{kind:"frames", index, numerator, denominator}` — never
  a divided float fps (safety criteria 3).
- No usable grid → `unknown` (a precision grid is never invented); valid grid
  without a measured length → `unmeasured` (distinct); a derivative with no
  own grid falls back to the parent's and reports `gridOrigin:"inherited"`
  (marked "inherited from source" in `AssetDurationLabel`).
- Invalid inputs (0/negative/fractional/NaN rates, negative unit counts)
  degrade to the honest states — never throw, never NaN.

## Integration points (for the captain / page wiring)

No API adapters are included — the audit is still underway. The library takes
plain data; wire it at the page/ledger layer:

- **Operation phases**: map the realtime operation journal to
  `OperationPhase`. Accepted-but-not-yet-applied → `pending`; the applied
  event → `applied`; a report timeout → `unknown` (then keep it until
  reconciliation reports applied/failed); server failure → `failed` with a
  localized `failureReason`.
- **Correlation ids**: pass the ledger's correlation through `correlationId`.
- **Producer feed**: pass the observed last tick epoch ms + source from the
  output-tick lane (not from management health), plus the management chip
  label (e.g. the existing `ConnectionStatus` word).
- **Actions**: use `TouchAction` for every mutation key; when a connection
  state blocks mutation, pass `disabled` + a localized `disabledReason`.
- **Duration**: feed `AssetDurationLabel` / `formatAssetDuration` with each
  asset's measured grid + length; pass `parent` for derivative clips.
- **Feedback placement**: render `OperationFeedback` inline under the action
  it reports (the reserved slot pattern replaces page-top toasts).

## i18n

All copy goes through `Trans`/`t`. New catalog entries are **not** committed
(locales are outside this folder's ownership): run
`npm --prefix web run i18n:extract -- --clean` then `npm --prefix web run
i18n:compile`, and commit `src/locales/**` together with this library — the
`i18n catalog freshness (lingui)` CI gate fails until then.

## Tests

Colocated, jsdom, `renderWithProviders` (Lingui + Query providers). Run:

```sh
npm --prefix web run test -- src/components/broadcast
```

DOM tests pin the DOM contract (classes, aria wiring, wording, event
behaviour); they deliberately do **not** claim physical layout.

## Browser regression acceptance (for the captain — jsdom cannot measure)

These are the physical checks the regression doc requires, keyed to the
prototype defects. Verify at 320×700, 390×844, 1024×768, 1440×1000, at
100% and 200% font scale:

1. **Undo region, no clipping (regression 1):** with an applied operation +
   Undo rendered, the Undo button's bounding rect is fully inside the
   region's rect (all four corners hit the button); the full label
   ("Undo <operation>") is visible with no `text-overflow` cut at any
   width/font scale. Repeated pending→applied cycles leave the invoking
   control's y position unchanged (reserved slot, no reflow).
2. **No transient widening (regression 4/5):** while a pending line with a
   long correlation id is displayed, `document.documentElement.scrollWidth
   === clientWidth` at 320 and 390; the line wraps instead.
3. **48px targets:** every `TouchAction` bounding box ≥ 48×48 at all widths.
4. **Own-button spinner (N7):** during pending, `elementFromPoint` at the
   centre of the adjacent control returns that control, not the spinner.
5. **Producer line at 390 (S2):** the producer line wraps to multiple lines
   rather than clipping; state, age and source remain fully visible; the
   management chip renders as a separate pill.
6. **Task fold (regression 3):** at 390×844, a `TaskWorkspace`'s title,
   status and primary content sit within the first fold; the Details key is
   hidden at ≥768px with the detail in flow.
