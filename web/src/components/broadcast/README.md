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
| `OperationFeedback` | component | Inline feedback + Undo region; position-critical actions must precede it. |
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
- The region has a `min-h-24` empty-state floor, **not** a fixed-size slot.
  Long translated copy, correlation ids, failure reasons and Undo labels
  increase its height across phases. Place all position-critical controls
  **before** the feedback region; controls after it can move. The region has
  no `overflow-hidden` or fixed height: never clip essential details or Undo
  to simulate stable placement. This contract applies to each integrating page.
- `pending` copy says *accepted — waiting for the applied event*; `applied`
  says *applied*. Never both.
- `unknown` says *outcome unknown*, with an optional caller-verified,
  localized `unknownReason` (such as a confirmed timeout). It is **not
  dismissable** here: only caller reconciliation clears it. The component
  holds **no timer** and cannot infer a timeout or failure by itself.
- `failed` / `cancelled` report only their phase. Neither promises that no
  partial effects occurred or that the server rolled anything back. They
  persist until dismissed (`onDismiss` renders the Dismiss key).
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
3. Non-safe-integer tick/clock/threshold, nonpositive threshold, or unsafe age
   → **Unknown** (visible invalid-observation explanation; no numeric age or green badge).
4. A tick in the future → **Unknown** (clock skew; age unverifiable).
5. Tick age ≥ `staleAfterMs` → **Frozen / stale** (with the real age).
6. Fresh tick + `producing` → **Producing** (with the real age).
7. Fresh tick + unreported state → **Unknown** ("Producer state not
   reported") — a tick alone is not a state claim.

The management connection renders as its own chip (`managementLabel`) and
**never** feeds the verdict — a connected management plane cannot make a
stopped, failed or unknown producer read as producing (safety criteria 7).
Age is measured against the caller's `now` (injectable; default is the shared
ticking clock in `producerNow.ts`, which keeps renders pure per
`react-hooks/purity` — first subscription refreshes the cached time after
an idle module lifetime; the shared interval stops after the last unmount).
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
- Counts and rate members must be safe integers; unsafe, non-finite, zero or
  fractional rates yield `unknown`, and unsafe/negative/fractional counts
  yield `unmeasured`. Valid sample and frame arithmetic uses integer division
  (BigInt internally) to preserve exact boundaries without malformed labels.

## Integration points (for the captain / page wiring)

No API adapters are included — the audit is still underway. The library takes
plain data; wire it at the page/ledger layer:

- **Operation phases**: map the realtime operation journal to
  `OperationPhase`. Accepted-but-not-yet-applied → `pending`; the applied
  event → `applied`; an unobserved outcome → `unknown` (supply a localized
  `unknownReason` only if the cause is verified, and keep the phase until
  reconciliation); server failure → `failed` with a localized `failureReason`.
- **Correlation ids**: pass the ledger's correlation through `correlationId`.
- **Producer feed**: pass the observed last tick epoch ms + source from the
  output-tick lane (not from management health), plus the management chip
  label (e.g. the existing `ConnectionStatus` word).
- **Actions**: use `TouchAction` for every mutation key; when a connection
  state blocks mutation, pass `disabled` + a localized `disabledReason`.
- **Duration**: feed `AssetDurationLabel` / `formatAssetDuration` with each
  asset's measured grid + length; pass `parent` for derivative clips.
- **Feedback placement**: render `OperationFeedback` inline after its action
  and after every other position-critical control. Do not place a key action
  below its variable-height region and expect a fixed vertical position.

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
   width/font scale. Measure actual page controls on both sides of the
   region: controls before it retain position; controls after it can shift
   when the content grows. Minimum height is not a stability guarantee.
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
