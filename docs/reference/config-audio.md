# Audio routes and output slots

This is a configuration reference, not a commissioning procedure. It describes
validation in `multiview-config`; transport support and runtime availability can
impose additional restrictions.

## Audio routing

`[audio]` is optional. `sample_rate_hz` is a required positive integer in Hz;
`routes` defaults to an empty list. Each `[[audio.routes]]` entry has these fields:

| Field | Meaning and validation |
| --- | --- |
| `input_id` | Required, non-empty managed source ID. It must name a declared source and occur at most once in `audio.routes`. |
| `channels` | Required tagged table. Its `kind` is `mono`, `stereo`, or `five_point_one`. |
| `target_track` | Optional discrete track name. If present it must be non-empty, unique among routes, and not `prog`, which is reserved for the program bus. |
| `language` | Optional language tag for the discrete track. If present it must not be empty; this validator does not enforce ISO-639 syntax. |
| `title` | Optional track title. If present it must not be empty. |
| `include_in_program_bus` | Boolean, default `false`. Includes the input in the program mix. If any route requests inclusion, at least one included route must be unmuted. |
| `gain_db` | Floating-point gain for the program contribution, default `0.0` dB (unity). It must be finite. |
| `mute` | Boolean, default `false`. A muted route contributes silence to the program bus; its discrete track remains declared. |

Unknown fields in an audio route or the `[audio]` block are rejected. When
`[audio]` is absent, the selectable track universe contains only the implicit
program track `prog`: a selection naming a discrete track is rejected. This
schema validation does not by itself prove that an audio source is producing
signal.

## Output audio

An output may use its `audio` selection table or `[[outputs.slots.audio]]`, but
not both when the slot list is non-empty. An empty slot list behaves as absent.
The first slot is the program mix and must not name `track`. Every later slot
must name a distinct declared `target_track`. Optional slot `channels`,
`sample_rate`, and `codec`, when supplied, must be `2`, `48000`, and `aac`
(case-insensitive), respectively. `language` is optional; the slot validator
shown in `output_format.rs` does not impose a language syntax rule. `name` is
an optional menu label (1–64 characters, no control characters or double
quote, no leading/trailing space) used as the `EXT-X-MEDIA` `NAME` of that slot's HLS/Cast audio
rendition; absent, the track's title (or `Track N`) / `Program` is used as
before. A program-only plain HLS output muxes audio into the video (no `EXT-X-MEDIA`), so the label shows only where separate audio renditions exist (Cast, or HLS with tracks); the same label also titles the stream in other mux outputs. Names within one master are made unique deterministically (a repeat
gets ` (n)`, n being its one-based position) and quotes/newlines are stripped.

The separate `outputs.audio` table has `mode = "program"`, `"auto"`, or
`"tracks"`. `program` carries the program bus; `tracks` requires a non-empty,
unique list of declared names (including `prog` if selected). `auto` resolves
the program plus declared discrete tracks, subject to transport capacity.
Output medium limits apply independently: for example WebRTC, WHIP, NDI,
display and AES67 allow one audio slot, while Cast allows up to 16. A selected
track unsupported by the output capability is rejected.

## Output subtitle slots

`[[outputs.slots.subtitle]]` declares one subtitle rendition. Fields are:

- `format`: `webvtt` (default), `cea608`, or `dvb`. The currently built
  combination is WebVTT on plain HLS or Cast, up to eight slots. LL-HLS
  WebVTT is not built. Other medium/format combinations are rejected as
  unsupported or mismatched.
- `language`: optional; if present, 1–35 ASCII letters, digits or hyphens.
- `name`: optional menu name; at most 64 characters, with no control characters
  or double quote. The effective label is `name`, otherwise `language`,
  otherwise `Subtitles N` (one-based); labels must be distinct per output.
- `source`: optional declared source ID whose captions this slot carries. An
  unknown ID is rejected. Absent means unpatched and the rendition stays empty.

Source: `crates/multiview-config/src/audio.rs`, `output_format.rs`, and
`lib.rs` (`validate_audio`). Configuration examples and runtime behaviour are
not verified by this reference.
