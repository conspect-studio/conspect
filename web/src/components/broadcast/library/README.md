# broadcast/library/ — media-library UI components

Production React/TS components for the media-**library** workspace
(M screen 6), distilled from the approved `broadcast-ui/library` design
prototype. **UX reference only**: the prototype's simulated store,
`localStorage` snapshot, fake jobs ledger and success toasts are NOT
carried over — nothing here fetches, persists, times, or owns state.

**Scope:** presentation-only, fully controlled, typed props. Every value
is caller-owned (page + realtime ledger); every callback is an **INTENT**
— firing one asserts nothing about acceptance or application. Outcomes
arrive back as props (`pendingOperation`, gates, data). No new
dependencies; every user string goes through Lingui.

**Status — read before wiring:** these components are **not integrated**.
They await (a) the real backend contract from the dedicated library
workstream (asset listing/promotion/unload — API shapes not yet shipped)
and (b) a page/route adapter. Nothing here may be pointed at invented
endpoints; the honest unavailable states below are the correct rendering
until those contracts exist. The evidence audit backing every honesty
rule in this folder is `.worktrees/.scratch/ui-design/library-evidence.md`.

## Exports (barrel: `./index.ts`)

| Export | Kind | Purpose |
|---|---|---|
| `LibraryBrowser` | component | Controlled asset browser: search/filter/sort/selection surfaces + capability-gated row & batch actions. |
| `VTReferenceSummary` | component | One VT slot's compact reference line: explicit empty/temporary/library states, on-air unload guard, save intents. |
| `MetadataInspector` | component | Per-asset metadata: measured (read-only) vs declared (gated intents) vs provenance/usage records. |
| `TranscriptRangeEditor` | component | Controlled word-range selection with exact boundary readouts, 48px endpoint steppers, keyboard, readonly mode. |
| `AssetAuditionControls` | component | Presentational local-audition surface: transport intents, channel/mode selection, real-supplied analysis only. |
| `RightsChip`, `TranscriptStateChip`, `OnAirPill`, `InUseChip`, `AssetKindChip` | components | Honest-state chips (rights unknown is never rendered as cleared). |
| `formatBoundaryLabel`, `describeBoundaryUnit`, `boundaryAtGridIndex`, `stepBoundary`, `isSameGrid`, `compareBoundaries` | pure functions | Exact grid-boundary math (integer index + rational pair / sample rate; never divided float fps). |

Types live in `./types.ts` (+ per-component files); boundary types reuse
`../duration.ts` (`RationalBoundary`, `DurationGrid`,
`AssetDurationSource`) — no divided float rate is ever stored.

## The honesty contracts (pinned by colocated tests)

### Capability gates — `ActionGate { supported, unavailableReason }`

Every action surface takes an explicit gate. `supported: false` renders
the control **disabled with the visible reason** (never a mystery
disable, never a fake "working" state). This is the required rendering
for every missing backend contract today:

- **Asset listing** (`LibraryBrowser.listingGate`): no listing API
  exists → the honest unavailable panel with the reason; search/sort
  disabled.
- **Save / unload** (`VTReferenceSummary.saveGate`/`unloadGate`): no
  promotion/unload verb exists (`MediaTransportVerb` stops at
  Load|Cue|Play|Pause|Stop|Seek) → disabled + reason until the backend
  owner ships the slice.
- **Metadata edits** (`MetadataInspector.editGate`): no metadata API →
  declared family read-only with the reason.
- **Audition** (`AssetAuditionControls.auditionGate`, per-mode gates):
  no per-asset audition/scrub/peaks/spectrogram service exists →
  unavailable panel; the spectrogram mode stays disabled with its reason
  even when waveform is offered.
- **Transcript** (`TranscriptRangeEditor.readOnly` +
  `readOnlyReason`): no STT backend exists → words render static with
  the capability explanation.

### On-air guard (VTReferenceSummary)

`onAir` is **derived desk truth passed in** — never a second hardcoded
state. While on air, unload-style actions (Save & unload, Unload) are
disabled with the reason on the control. **Save alone stays available**
(if its gate supports it): protecting bytes is always safe.

### Intents, never success

Callbacks are intents (`onSaveIntent`, `onUnloadIntent`,
`onRowAction`, `onBatchAction`, `onSelectionChange`, …). Components
never render applied/saved/working on their own; the accepted-but-not-
applied state is passed in as `pendingOperation` and renders as
"accepted — waiting for the applied event" — the applied event is the
only truth (same wording family as `../OperationFeedback`).

### Exact time, never float

- Trims and transcript word times are `RationalBoundary` values
  (integer index + exact rational pair, or integer sample index + rate).
  Readouts show NDF timecode/elapsed clock **plus** the integer unit
  line (`FR 250 · 30000/1001`, `SMP 96,000 · 48 kHz`).
- Steppers propose adjacent indices, clamped to the measured length;
  trim proposals never cross `in < out` (config invariant,
  `media.rs:78`). A proposal is an intent; nothing applies inside.
- Absent/unusable data degrades to honest fallbacks (`Duration unknown`,
  `Not measured`, `Timecodes not aligned`) — never NaN/undefined.

### Rights

`unknown` is always visible as the warning chip and is **never**
rendered as cleared; only the operator's explicit select (an
`onRightsChange` intent) can declare a status. Rights are a DECLARED
metadata family.

## Data adapter requirements (for the captain / page wiring)

No API adapters are included — the backend contract is still being
delivered by the dedicated library workstream. When wiring, the page
must supply:

- **Listing**: rows already searched/filtered/sorted by the caller
  (`LibraryBrowser` only renders what it is given); facet groups with
  **counts computed over the caller's full set**, not the filtered page.
- **Durations**: per-asset measured grid + units via
  `AssetDurationSource` (`../duration.ts`); `parent` for derivative
  clips. Never a global 25 fps / 48 kHz assumption — per-asset rates.
- **Byte sizes**: exact, caller-formatted strings (`sizeLabel`) or exact
  integers (`byteSize`); a free-space query does not exist — do not
  fabricate one.
- **Provenance/usage**: read-only records; there is no lineage or
  reference-count query — do not invent reclaimed-size or
  deletion-safety claims.
- **Transcript**: words with exact per-word start/end boundaries on the
  asset's own grid; corrections are overlays in the caller's store (a
  text edit must never silently move a timecode); STT capability
  explanation for the readonly state.
- **Audition analysis**: REAL measured data only — waveform peaks
  normalized 0..1 for the active window (invalid/NaN values degrade to
  the floor); `null` when no service supplied it. The component never
  generates synthetic analysis.
- **On-air truth**: derive from the desk programme state; never from
  the slot's own fields.
- **Gates**: one `ActionGate` per action, with a localized
  `unavailableReason` whenever the backend contract is absent or the
  control connection blocks the mutation (distinguish internet loss ≠
  stale control plane ≠ read-only — audit §9.10).
- **Selection/sort/filter/filtersOpen/disclosure state**: caller-owned;
  a per-operator preference store does not exist yet — session state is
  the caller's choice, `localStorage` authority is not.

Every "supported: false" gate above is the correct production wiring
until the backend workstream ships its contract; flip a gate only when
the matching route actually exists.

## i18n

All copy goes through `t`/`<Trans>` (Lingui). New catalog entries are
**not** committed (locales are outside this folder's ownership): run
`npm --prefix web run i18n:extract -- --clean` then `... i18n:compile`,
and commit `src/locales/**` together with this library — the
`i18n catalog freshness (lingui)` CI gate fails until then.

## Tests

Colocated, jsdom, `renderWithProviders` (Lingui + Query providers).
They pin the DOM/interaction contract (controlled intents, keyboard
flow, gates, on-air guard, exact boundary rendering, no silent
mutation); they do not claim physical layout. Run scoped:

```sh
npm --prefix web run test -- src/components/broadcast/library
```

## Browser regression acceptance (jsdom cannot measure)

Physical checks for the captain, at 390×844 / 1024×768 / 1440×1000 at
100% and 200% font scale:

1. **Phone fold first (390):** browser search + sort + first results
   within the first fold; the facet wall stays behind the controlled
   Filters key on narrow and in flow from `md:` up.
2. **48px floors:** every action key, word option, chip toggle and
   stepper ≥ 48×48 at all widths.
3. **No chip collisions:** chips wrap with gaps at 390; the ON AIR pill
   never overlaps the asset label or the rights chips.
4. **Headers bury nothing:** the browser's `N shown` line and the
   VT summary rows keep content below them within one fold height.
5. **Long values wrap, never widen:** `document.scrollWidth ===
   clientWidth` with a long asset name, provenance URL and correlation
   reason at 390.
