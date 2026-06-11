# Clock & timer synthetic sources — dual analogue+digital clock, multi-zone metadata, and countdown/countup timers

> **Status:** Design brief (verification-hardened). Extends ADR-0027 (synthetic
> sources are first-class `SourceKind`s). Decision recorded in
> [ADR-0047](../decisions/ADR-0047.md). Backlog: `SYN-CLOCK-*` / `SYN-TIMER-*`
> in [work-schedule.md](../development/work-schedule.md).
>
> **Source of truth for names/flags:** [conventions.md](../architecture/conventions.md).
> Where this brief and the as-built Rust disagree, the **code wins** and this
> brief is updated.

## 0. Why this brief exists

ADR-0027 made colour **bars**, a **solid** slate, and a full-frame **clock**
first-class, in-process `SourceKind`s that flow through the one uniform
ingest→`TileStore`→compositor path. The clock it shipped is deliberately minimal:
one face at a time (analog **or** digital), a single fixed `tz_offset_minutes`,
no label, no countdown. The operator has asked for two concrete extensions:

1. **A dual analogue+digital clock as a single source**, plus richer per-clock
   metadata: a **location/label**, an **IANA timezone** (e.g.
   `Australia/Sydney`), and a visible **UTC-offset** indicator — so several
   clocks can sit in a layout, each labelled with its zone.
2. **A digital countdown / countup to a target**: count **down** to a target or
   **up** from it, where the target is either a **time-of-day** (e.g. `14:30:00`,
   optionally recurring daily) or a **specific date+time** (an absolute instant,
   with a timezone). Behaviour **at** and **after** the target is configurable
   (hold at zero, or roll into the opposite direction), with a configurable
   display format and overrun styling.

Both are pure-pixel, LGPL-clean, in-process generators — they need no libav and
no GPL codec, exactly like the existing synthetic sources. This brief grounds the
design in the as-built code and specifies the schema, rendering, timing/timezone
correctness, and the test strategy so an implementer can build it directly.

## 1. As-built code this design builds on (cite-checked)

| Piece | Where | What it gives us |
|---|---|---|
| `SourceKind` (internally tagged `kind=`) with `Bars` / `Solid { color }` / `Clock { face, twelve_hour, tz_offset_minutes }` | `crates/multiview-config/src/schema.rs:214` | The serde union we extend additively. `ClockFaceConfig` is `analog`/`digital` at `schema.rs:197`. |
| `SyntheticKind` (resolved, copy-able render descriptor) + `render()` + `generator_loop()` | `crates/multiview-cli/src/synth.rs:45`, `:107`, `:307` | The render path: `from_source_kind` (`:72`) maps config → render descriptor; `render()` switches on kind; `generator_loop()` is the per-tick publish thread (peer of a decode thread), re-rendering a clock only when its displayed second changes (`render_key`, `:138`). |
| `render_clock()` — composes the overlay rasterizer onto a near-black slate | `crates/multiview-cli/src/synth.rs:150` | Analog branch builds `clock_face(...)` primitives; digital branch rasterizes a run via `TextEngine` and centres it. `overlay`-gated; returns `SynthError::OverlayRequired` without it (`:251`). |
| Clock **model** (pure, integer arithmetic) | `crates/multiview-overlay/src/clock.rs` | `WallTime` (whole Unix seconds, `:30`), `TimeZoneOffset` (fixed whole-minute offset, "DST resolved upstream to a concrete offset", `:57`), `LocalTime` (h/m/s, `:92`), `ClockFace`/`ClockModel` (`:127`/`:354`), `AnalogHands::for_dial` (12h vs 24h dial math, `:219`), `TimeRef`/`RefSource`/`RefStatus` (the a11y badge, `:329`/`:252`/`:281`). |
| Compositor 2D primitives (vector, CPU+GPU SDF) | `crates/multiview-compositor/src/overlay/subpass.rs` | `OverlayPrimitive::{Glyph, FilledRect, Line, Stroke (angled capsule), Ring (annulus), Image}` (`:117`). `clock_face(angles, style, hour_ticks)` builds a full analog face — bezel `Ring`, hour-tick `Stroke`s, three hand `Stroke`s, hub `FilledRect` (`:342`). `ClockFaceStyle::centred/at` (`:306`). `HandAngles` (`:292`). `OverlayColor` is **linear** RGBA (`:48`). |
| Text engine (pure-Rust) | `crates/multiview-compositor/src/overlay/text.rs` | `TextEngine::rasterize_run(text, FontFamily::{Mono,…}, size_px, rgba) -> RasterizedRun` of `RasterizedGlyph`s with `dest_x/dest_y/width/height` + premultiplied coverage — the cosmic-text/swash path. Used by `render_clock`'s digital branch (`synth.rs:204`). |
| Bake | `apply_overlays_to_nv12(&bg, &list, canvas)` | `crates/multiview-compositor/src/overlay/subpass.rs` | Blends an `OverlayDrawList` onto an NV12 image in linear light (inv #8) and returns NV12 (inv #5). |
| `Nv12Image::{solid_rgb, color_bars, solid, sample, y_plane}` | `crates/multiview-compositor/src/pipeline.rs` | The slate/bars constructors + the per-pixel sampler the golden tests use. |
| Reference-clock contract | [ADR-T012](../decisions/ADR-T012.md), `multiview-engine/src/{ptp.rs,sysref.rs}`, `multiview-cli/src/wallclock.rs` | The disciplined reference (`SelectedReference`, `ReferenceStatus`) is a **media-clock reference only, never a pacer** (inv #1). The clock face may *display* its lock badge; it must never gate output. |

**Key as-built facts that constrain the design:**

- **The clock display value derives from WALL CLOCK sampled at render — not from
  the media tick counter** (inv #1: `out_pts=f(tick)`; the tick paces output, the
  wall clock is *sampled*). `generator_loop` (`synth.rs:322`) reads
  `unix_now_seconds()` each iteration and stamps the published frame's `MediaTime`
  from the **monotonic elapsed** of the generator (`synth.rs:339`), not from the
  displayed time. We preserve this split exactly.
- **The model is integer-only**; the only floats are hand angles, from a single
  division (`clock.rs:17`). Carrying time as `i64` seconds / `chrono` types (never
  float fps) is the standing rule (inv #3, safety rule #6).
- **`TimeZoneOffset` is a *fixed* whole-minute shift** and explicitly delegates
  DST to "resolved upstream to a concrete offset" (`clock.rs:57`). That upstream
  resolver does not exist yet — this brief adds it (the `chrono-tz` seam, §5).

## 2. Feature 1 — dual analogue+digital clock + multi-zone metadata

### 2.1 UX / functionality

A `clock` source can render, in one source frame:

- an **analogue** face (the existing `clock_face` bezel/ticks/hands), **or**
- a **digital** readout (the existing centred `TextEngine` run), **or**
- **both** ("dual"): an analogue face with a digital readout beneath it, plus an
  optional metadata strip.

Per-clock metadata, drawn as a small text block (label line + zone/offset line):

- **`label`** — a free-text location/title, e.g. `Sydney`, `Studio A`, `NY DESK`.
- **`timezone`** — an **IANA zone id**, e.g. `Australia/Sydney`,
  `America/New_York`, `UTC`. This is the *authoritative* zone; DST is resolved
  from it (§5).
- **`show_offset`** — when true, render the **resolved UTC offset** as a badge,
  e.g. `UTC+10:00` / `UTC-04:00`, computed for the *displayed instant* (so it is
  correct across a DST boundary).

The operator can place several `clock` sources in a grid — `Sydney` /
`London` / `New York` / `Los Angeles` — each labelled, each with its own zone,
each showing its offset. This is the "world clock wall" pattern.

### 2.2 Layout of the dual face

For a `dual` clock in a `W×H` tile:

```
┌───────────────────────────┐
│         ╭───────╮         │   analogue face centred in the
│        (  ◷ 10  )         │   upper region (square, radius ≈
│         ╰───────╯         │   0.40·min(W, upper_h))
│       14:32:07            │   digital readout, mono, centred
│   Sydney      UTC+10:00   │   metadata strip: label · offset
└───────────────────────────┘
```

- Reserve the bottom `~18%` of `H` for the metadata strip when `label` or
  `show_offset` is set; the analogue face + digital readout share the rest.
- `analogue` and `digital` modes use the full tile as today (the metadata strip
  still draws at the bottom if requested, shrinking the face/readout region).
- All sizing is integer-derived from `W`/`H` (no magic px); the existing
  `ClockFaceStyle::at(cx, cy, radius)` (`subpass.rs:328`) lets us place the bezel
  anywhere, so `dual` is purely a *placement* change — **no new renderer**.

### 2.3 Why this is mostly composition, not new drawing

Every visual element already exists:

- analogue face → `clock_face(HandAngles, ClockFaceStyle::at(...), hour_ticks)`,
- digital readout → `TextEngine::rasterize_run` → centred `Glyph` primitives,
- label / offset badge → two more `rasterize_run` calls placed in the strip.

The work is (a) a `dual` placement function that calls both with the right
`ClockFaceStyle` + text origins, and (b) feeding the resolved offset + label into
the text. No new `OverlayPrimitive`, no new shader.

## 3. Feature 2 — countdown / countup to a target

### 3.1 UX / functionality

A new **`timer`** source renders a digital duration that counts toward (or away
from) a target instant:

- **`direction`** — `down` (target − now) or `up` (now − target).
- **target** — one of:
  - **`time_of_day`** `{ at = "14:30:00", tz, recur_daily }` — the next (or, with
    `up`, the most recent) occurrence of that wall-clock time in zone `tz`. With
    `recur_daily = true` the timer re-arms every day (a daily "back in N minutes"
    / "show starts at" countdown); with `false` it is the single next occurrence
    from arm time.
  - **`datetime`** `{ at = "2026-07-01T09:00:00", tz }` — an absolute instant
    (`tz` resolves the offset, so `at` is unambiguous across DST).
- **at-target behaviour** (`on_target`) — what happens when the remaining duration
  hits zero:
  - **`hold`** — freeze the display at `00:00:00` (default for `down`).
  - **`continue`** — roll past zero in the same `direction` and keep going
    (a count-up that crosses its target keeps incrementing; a count-down that
    reaches zero starts counting **up** — i.e. "elapsed since target", styled as
    overrun).
  - **`zero_then_up`** — explicit "countdown reaches zero, then counts up the
    overrun" (the common live-production "we're N over" case).
  - **`recur`** — only valid with a `time_of_day` `recur_daily` target: re-arm to
    the next occurrence (a perpetual daily countdown).
- **display format** (`format`) — `d_hh_mm_ss` (default, drops the day field when
  zero), `hh_mm_ss`, `mm_ss`, `hh_mm_ss_ff` (add a frames field derived from the
  canvas cadence), or `auto` (drop leading zero units: `5:00`, `1:05:00`,
  `2d 01:05:00`). Sub-second is **frames**, never float seconds (the cadence is a
  `Rational`; frames = `floor(subsecond_ns · num / (den · 1e9))`).
- **overrun styling** (`overrun`) — when the timer is past its target and styled
  as overrun (`continue`/`zero_then_up`), an optional `prefix` (default `+` /
  `-`) and an a11y badge word (`OVER` / `ELAPSED`) so the state reads without
  colour (the same accessibility stance as `RefStatus`, `clock.rs:277`).

### 3.2 Behaviour at and after the target (worked)

For `direction = down`, target `T`, now `N`, `remaining = T − N` (seconds):

| `on_target` | `remaining > 0` | `remaining == 0` | `remaining < 0` (past target) |
|---|---|---|---|
| `hold` | show `remaining` | show `00:00:00` | show `00:00:00` (frozen) |
| `continue` / `zero_then_up` | show `remaining`, prefix `−`/none | show `00:00:00` | show `|remaining|`, prefix `+`, badge `OVER` |
| `recur` (time_of_day+recur_daily) | show `remaining` | re-arm to next day's `T`; show new `remaining` | (never reached) |

For `direction = up`, target `T`, `elapsed = N − T`: symmetric (`hold` freezes at
the cap if a `cap` is set; default up has no cap and just keeps counting).

### 3.3 Display value math (integer, deterministic)

```
displayed_seconds = match direction {
    Down => (target_unix - now_unix),        // may be negative (overrun)
    Up   => (now_unix - target_unix),
};
// at_target clamping per on_target:
let shown = clamp_for_policy(displayed_seconds, on_target);   // i64 seconds
let neg   = shown < 0;
let abs   = shown.unsigned_abs();                              // u64
let days  = abs / 86_400; let rem = abs % 86_400;
let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
```

`frames` (for `hh_mm_ss_ff`) come from the **sub-second** part of the
*generator's* sampled wall instant against the cadence `Rational` — but to stay
deterministic and cheap, a timer re-renders at most once per **field** it shows
(per second for `hh_mm_ss`, per frame for `hh_mm_ss_ff`). The `render_key`
(`synth.rs:138`) generalises: for a frame-resolution timer the key is the frame
index, not the second.

## 4. Rendering approach (analogue face + digital text)

### 4.1 Analogue face — already a solved primitive

The analogue face is **drawn with vector primitives, not a bitmap** — this is the
deliberate as-built design (`subpass.rs` module docs: "meters are geometry, not
pictures"; the same applies to the clock). `clock_face()` (`subpass.rs:342`)
emits, back-to-front:

1. a bezel **`Ring`** (anti-aliased annulus, closed-form SDF, identical CPU/GPU),
2. `hour_ticks` radial **`Stroke`** ticks (12 for a 12-hour dial, 24 for 24-hour),
3. three hand **`Stroke`**s (angled capsules with round caps) — hour short+thick,
   minute longer+thinner, second longest+thinnest (red),
4. a hub **`FilledRect`** with `corner_radius` = a round dot.

Hand angles come from `AnalogHands::for_dial(local, twelve_hour)` (`clock.rs:219`)
— pure integer→angle math, the only float. **No new drawing code is required for
the analogue face**; `dual` only changes the `ClockFaceStyle` (centre + radius)
so the face sits in the upper region.

Optional enrichment (small, additive, still vector): 12 **hour numerals** as
text runs placed at tick positions for `dual`/`analogue` when `numerals = true`.
This reuses `rasterize_run` + the unit-vector placement already in `clock_face`.

### 4.2 Digital readout + metadata — `TextEngine` runs

The digital readout reuses the exact centring logic in `render_clock`'s digital
branch (`synth.rs:204-244`): `rasterize_run(text, FontFamily::Mono, size_px,
rgba)`, measure the glyph extent, offset every glyph to centre. For `dual`, the
origin is the lower region instead of the whole tile.

The metadata strip is two more runs:

- **label line**: `rasterize_run(label, FontFamily::SansSerif, …)` left-aligned in
  the strip,
- **offset badge**: `rasterize_run("UTC+10:00", FontFamily::Mono, …)`
  right-aligned in the strip.

The timer is one centred mono run of the formatted duration, plus an optional
overrun-badge run (`OVER`/`ELAPSED`) — symmetry with the clock's a11y badge.

### 4.3 The bake stays NV12 + linear (inv #5, #8)

All primitives go into one `OverlayDrawList`; `apply_overlays_to_nv12(&slate,
&list, canvas)` blends them in linear light onto the near-black slate and returns
NV12 — unchanged from today. `OverlayColor` is linear (`subpass.rs:48`); we never
materialise RGBA per tile.

### 4.4 Cost (efficiency pass, inv #9)

A clock/timer re-renders only when its displayed **field** changes (once a second
for `hh:mm:ss`; once a *frame* only when `ff` is shown) — `render_key`
generalises to a field index, so an animated clock costs one bake per visible-unit
change, not one per tick (the existing `generator_loop` caching, `synth.rs:319`).
The analogue second hand *does* move every second, so an analogue/dual clock bakes
≈1/s; a `mm_ss` timer bakes ≈1/s; an `hh_mm_ss_ff` timer bakes at the canvas
cadence (this is the one expensive mode and is opt-in). Each bake is a handful of
SDF primitives + a few short text runs over one tile — bounded and cheap, and the
degradation loop (inv #9) sheds it cheapest-impact-first like any tile.

## 5. Timing & timezone correctness

### 5.1 The split: tick paces, wall clock is sampled (inv #1)

Unchanged and load-bearing: the **output clock paces** (`out_pts=f(tick)`); the
**displayed time is wall-clock sampled at render** and stamped onto the frame as
the generator's monotonic elapsed (`synth.rs:339`). A clock/timer source can never
pace or stall the engine — a slow bake just republishes last-good (inv #2,
`synth.rs:332`). The timer's *target* is an absolute instant; "now" is the sampled
wall clock; neither feeds the encoder PTS (safety rule #6).

### 5.2 IANA timezone → concrete offset (`chrono-tz`)

The as-built `TimeZoneOffset` is a *fixed* whole-minute shift and delegates DST to
an upstream resolver (`clock.rs:57`). This brief **adds that resolver**: a config
`timezone = "Australia/Sydney"` is parsed to a `chrono_tz::Tz`, and at each render
the **resolved offset for the displayed instant** is computed
(`tz.offset_from_utc_datetime(utc).fix()`), then handed to the existing model as a
`TimeZoneOffset::from_minutes(resolved_minutes)`. This is DST-correct: a Sydney
clock shows `UTC+11:00` in January and `UTC+10:00` in July automatically, and the
`show_offset` badge reflects the resolved value.

- **Backward compatibility:** `tz_offset_minutes` stays valid (fixed offset, no
  DST) — `timezone` (IANA) is the new, preferred field. If both are present,
  `timezone` wins and `tz_offset_minutes` is ignored with a config warning.
- **Where the resolver lives:** a pure helper in `multiview-config` (or a small
  `multiview-overlay` helper) `resolve_offset(tz: Tz, at: WallTime) ->
  TimeZoneOffset` so it is unit-testable with injected instants and reused by the
  control plane (to render the offset badge in the WebUI form preview).
- **Validation:** an unknown IANA id is a `ConfigError` at load (typed, no panic);
  the WebUI zone picker is populated from `chrono_tz::TZ_VARIANTS`.

### 5.3 Optional lock to the disciplined reference (ADR-T012)

A clock face may **display** the reference badge (`TimeRef`/`RefStatus`,
`clock.rs:329`) when the source opts in (`show_reference = true`): the badge reads
`PTP locked` / `SYS freerun` with its glyph, sampled from the engine's
`SelectedReference` (ADR-T012). This is **display only** — the reference never
paces the clock or the output (inv #1; ADR-T012's "media-clock reference, never a
pacer"). Default off (the standalone clock today draws no badge, `synth.rs:194`).

### 5.4 Determinism for tests

Every value derives from an **injected `WallTime`** (and, for the timer, an
injected target instant + cadence). `render()` already takes `now: WallTime`
(`synth.rs:112`); the timer render takes `now` + the resolved target so a test can
assert `14:29:55 → 00:00:05` exactly, the at-zero transition, and the overrun.
The `chrono-tz` resolution is a pure function of `(Tz, instant)` — golden-testable
at known DST boundaries.

## 6. Schema additions (the exact serde shape)

All additive, internally tagged, **`schema_version` stays `1`** (ADR-0027 set the
precedent: new variants/fields are additive). New enums mirror the model and use
`#[serde(tag = …, rename_all = "snake_case")]`, `#[non_exhaustive]`, never
`untagged` (conventions §5).

### 6.1 Extended `Clock` variant

```rust
/// The face a `Clock` source renders. Adds `Dual` to the existing analog/digital.
#[derive(…, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockFaceConfig {
    #[default]
    Analog,
    Digital,
    /// Both: an analogue face with a digital readout beneath it.
    Dual,
}

SourceKind::Clock {
    #[serde(default)]
    face: ClockFaceConfig,            // analog | digital | dual
    #[serde(default)]
    twelve_hour: bool,                // 12h vs 24h (both faces), as today

    /// IANA timezone id (e.g. "Australia/Sydney"). Preferred over
    /// `tz_offset_minutes`; DST-resolved per displayed instant. Absent ⇒ use
    /// `tz_offset_minutes` (fixed). If both present, `timezone` wins (warn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,

    /// Fixed UTC offset in minutes (legacy / no-DST). Retained for back-compat.
    #[serde(default)]
    tz_offset_minutes: i32,

    /// Operator location/label drawn on the face (e.g. "Sydney").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,

    /// Draw a "UTC±HH:MM" offset badge for the displayed instant.
    #[serde(default)]
    show_offset: bool,

    /// Draw the disciplined-reference badge (PTP/NTP/SYS lock). Display only —
    /// never paces (ADR-T012). Default off.
    #[serde(default)]
    show_reference: bool,

    /// Draw hour numerals on the analogue/dual face.
    #[serde(default)]
    numerals: bool,
}
```

### 6.2 New `Timer` variant

```rust
/// Count-down/up direction.
#[derive(…, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")] #[non_exhaustive]
pub enum TimerDirection { #[default] Down, Up }

/// What happens when the timer reaches its target.
#[derive(…, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")] #[non_exhaustive]
pub enum TimerOnTarget {
    #[default] Hold,        // freeze at 00:00:00
    Continue,               // roll past zero in the same direction
    ZeroThenUp,             // countdown → 00:00:00 → count the overrun up
    Recur,                  // re-arm to next occurrence (time_of_day+recur_daily)
}

/// The display format. `auto` drops leading zero units.
#[derive(…, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")] #[non_exhaustive]
pub enum TimerFormat {
    #[default] DHhMmSs,     // D:HH:MM:SS, day field dropped when zero
    HhMmSs,                 // HH:MM:SS
    MmSs,                   // MM:SS
    HhMmSsFf,               // HH:MM:SS:FF (frames from the canvas cadence)
    Auto,                   // drop leading zero units (5:00, 1:05:00, 2d 01:05:00)
}

/// The target instant, internally tagged by `target` (never untagged).
#[derive(…, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")] #[non_exhaustive]
pub enum TimerTarget {
    /// A wall-clock time-of-day in `tz`; next (or most-recent for `up`)
    /// occurrence. `recur_daily` re-arms each day.
    TimeOfDay {
        at: String,                    // "HH:MM:SS" (24h)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,      // IANA; absent ⇒ tz_offset_minutes
        #[serde(default)]
        tz_offset_minutes: i32,
        #[serde(default)]
        recur_daily: bool,
    },
    /// An absolute date+time; `timezone` resolves the offset (DST-correct).
    DateTime {
        at: String,                    // RFC3339 local "2026-07-01T09:00:00"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,      // IANA; absent ⇒ tz_offset_minutes
        #[serde(default)]
        tz_offset_minutes: i32,
    },
}

SourceKind::Timer {
    #[serde(flatten)]
    target: TimerTarget,               // time_of_day | datetime (tag: `target`)
    #[serde(default)]
    direction: TimerDirection,         // down (default) | up
    #[serde(default)]
    on_target: TimerOnTarget,          // hold | continue | zero_then_up | recur
    #[serde(default)]
    format: TimerFormat,               // d_hh_mm_ss (default) | …
    /// Operator label drawn above/below the count (e.g. "ON AIR IN").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// Overrun prefix override (default "-" pre-target / "+" post-target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overrun_prefix: Option<String>,
    /// Draw the overrun a11y badge ("OVER"/"ELAPSED") past the target.
    #[serde(default = "default_true")]
    overrun_badge: bool,
}
```

> **Tag collision note:** `SourceKind` is tagged on `kind`; `TimerTarget` is
> tagged on `target` and `#[serde(flatten)]`ed into the `Timer` variant, so a
> timer's TOML is `kind = "timer"`, `target = "time_of_day"`, `at = "14:30:00"`.
> Two distinct tag keys, no clash (the same pattern as `Source.kind` flattening
> `SourceKind`, `schema.rs:332`).

### 6.3 `SyntheticKind` (resolved render descriptor, `synth.rs:45`)

`from_source_kind` (`synth.rs:72`) gains `Clock { mode, twelve_hour, offset:
TimeZoneOffset, tz: Option<Tz>, label, show_offset, show_reference, numerals }`
and a new `Timer { direction, on_target, format, target_unix_resolver, label,
overrun_* }`. The `tz: Option<Tz>` is resolved-per-render (§5.2), not at config
load, so a long-running clock follows DST. `animated()` (`synth.rs:95`) returns
true for both. `render_key` (`synth.rs:138`) returns the field index (second, or
frame for `_ff`).

### 6.4 Example TOML

```toml
# Dual clock, Sydney, DST-correct, with label + offset badge
[[sources]]
id = "clk_syd"
kind = "clock"
face = "dual"
timezone = "Australia/Sydney"
twelve_hour = false
label = "Sydney"
show_offset = true

# Countdown to a daily 14:30 show start, then count the overrun up
[[sources]]
id = "tmr_show"
kind = "timer"
target = "time_of_day"
at = "14:30:00"
timezone = "Australia/Sydney"
recur_daily = true
direction = "down"
on_target = "zero_then_up"
format = "auto"
label = "ON AIR IN"

# Countdown to an absolute instant
[[sources]]
id = "tmr_launch"
kind = "timer"
target = "datetime"
at = "2026-07-01T09:00:00"
timezone = "UTC"
direction = "down"
on_target = "hold"
format = "d_hh_mm_ss"
```

## 7. Dependency choice — `chrono-tz` (deny-clean)

- **Chosen:** `chrono` + `chrono-tz`. `chrono-tz` is **dual-licensed
  `MIT OR Apache-2.0`** ([crates.io](https://crates.io/crates/chrono-tz),
  [LICENSE on docs.rs](https://docs.rs/crate/chrono-tz/latest/source/LICENSE)) —
  both already on the `deny.toml` `[licenses].allow` list (`deny.toml:65`). It
  bundles the **IANA tz database** at build time via a `parse-zoneinfo` build
  script ([lib.rs/chrono-tz](https://lib.rs/crates/chrono-tz)); the tz database
  itself is **public domain**, so there is **no GPL** and **no runtime network**.
  `chrono` is also `MIT OR Apache-2.0`. Build a `chrono-tz` with
  `default-features = false` + the `filter-by-regex`/`case-insensitive` knobs as
  needed to bound binary size; this is pure-Rust and lands in the **default
  build** (no `ffmpeg`, no `gpl-codecs`).
- **Alternatives considered:**
  - **`jiff`** (`MIT OR Apache-2.0`, excellent IANA support, reads the *system*
    tzdb on Unix) — rejected for v1 only because reading the host `/usr/share/
    zoneinfo` makes output host-dependent and the container may lack tzdata;
    `chrono-tz`'s *bundled* db is reproducible and self-contained (matches the
    "same config, same picture across builds" principle of ADR-0027). Revisit if
    we want live tzdata updates without a rebuild.
  - **`time` + `tzdb`/`tz-rs`** — workable and permissive, but `chrono` is already
    the ecosystem default and `chrono-tz` is the lowest-friction IANA layer on top
    of the integer `WallTime` model we already have.
  - **Keep fixed-offset only (no new dep)** — rejected: the operator explicitly
    asked for IANA zones with offset display, and a fixed offset is wrong across
    DST (a Sydney clock would read an hour off for half the year).
- **`cargo deny check` impact:** the whole `chrono`/`chrono-tz` closure resolves
  to `MIT`/`Apache-2.0`/`Unicode-3.0` (already allowed). The implementing PR runs
  `cargo deny check` and commits the updated `Cargo.lock`.

## 8. Testing strategy

Per the repo tiers: **golden-frame on CPU only**; GPU output uses SSIM/PSNR, never
bit-exact (compositor `CLAUDE.md`). All tests inject time — no wall-clock reads.

**Model / pure (no I/O, deterministic):**

- `resolve_offset(Tz, WallTime)` golden at DST boundaries: Sydney `2026-01-15` ⇒
  `+11:00`, `2026-07-15` ⇒ `+10:00`; New York spring-forward/fall-back instants.
- Timer math: `14:29:55` vs target `14:30:00` ⇒ `00:00:05` down; at-zero shows
  `00:00:00`; one second past with `zero_then_up` ⇒ `+00:00:01` + `OVER` badge;
  `up` symmetry; `recur_daily` re-arms to the next day. Property test
  (`proptest`): `format(decompose(n))` round-trips and is monotone in `n`;
  `down` and `up` are negatives of each other at the same instant.
- `TimerTarget::TimeOfDay` next-occurrence resolution across midnight and across a
  DST gap (a `02:30` target on a spring-forward day).

**Render / golden (CPU `overlay` feature):** extend the as-built content-aware
clock tests (`synth.rs:464-556` already assert "drew a face" + "animates" + "12h
vs 24h dial differ"):

- `dual` differs from both `analog` and `digital` at the same instant (it drew the
  extra readout/strip) and from the blank slate.
- a labelled clock differs from an unlabelled one (the label drew glyphs); a
  `show_offset` clock's strip differs between Sydney-in-January and Sydney-in-July
  (the offset badge text changed).
- timer goldens: `00:00:05` differs from `00:00:04`; the at-zero frame; an overrun
  frame differs from a pre-target frame (the `+`/badge drew). Sample-based
  (`Nv12Image::sample`/`y_plane`), content-aware — never an assertion-free hash.

**Config / serde:** round-trip TOML+JSON for every new field/variant; an unknown
IANA id is a typed `ConfigError` (no panic); `timezone` + `tz_offset_minutes`
both-present emits the documented warning and `timezone` wins; `examples/*.toml`
parse (the existing example-validation test).

**Mutation:** `cargo mutants --in-diff` must catch a flipped direction
(`Down`↔`Up`), a dropped at-target clamp, and an offset-sign error — the
format/clamp/resolve functions are the mutation targets.

**Invariants re-checked:** #1 (timer/clock never paces; tick still drives PTS), #2
(slow bake holds last-good), #5/#8 (NV12 + linear bake unchanged), #9 (bounded
per-field bake; degradation sheds it like any tile), #3/#6 (integer/`chrono`
time, never float fps), ADR-T012 (reference is display-only, never a pacer).

## 9. Scope boundary (what this brief does NOT cover)

- **Audio** — clock/timer sources are silent; the bars *tone* companion is AUD-5,
  separate.
- **The clock *overlay*** (drawn on top of a composited canvas) is unchanged; this
  brief is about clock/timer **sources** (a tile/canvas), per ADR-0027's
  source-vs-overlay split.
- **Live tzdata updates without a rebuild** — `chrono-tz` bundles the db at build
  time; a tz-rule change needs a dependency bump + rebuild (acceptable; revisit
  with `jiff`/system-tzdb if operationally needed).
- **WebUI** beyond the schema + zone-picker wiring is its own `SYN-CLOCK-UI`
  backlog item (the SPA source-kind picker + forms + the zone dropdown from
  `TZ_VARIANTS`).
