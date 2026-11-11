# Current-main switcher backend curation

## Provenance and boundaries

Base: current fetched `origin/main` at `49697655`, which contains the requested
`9e842a6b` FFmpeg 9 mainline. Source checkpoint: `52137295`.

Curated backend delta: `2fc52b8b^1..2fc52b8b`, corresponding to the reviewed
`a4abb03f..ca10192e..1b839a9e..a44912b7` first slice. No historical release merge,
old output pipeline, weather, IPTV catalogue, graphics library or webpage UI
was restored. The older release-integration note describes its historical merge,
not this branch's ancestry.

Required dependencies absent from current main:

- Reviewed engine demand manager and its `RequestedSize: Hash` metadata support.
- Source catalogue and bounded rail driver over current main's real ingest paths.
- Canonical optional source activation and demand-manager system limits.
- Control demand provider DTOs and bounded sustained preview lease owner.
- Reviewed authenticated principal/method/path idempotency namespace, 4096-key
  non-evicting capacity, typed 503 and exhaustive existing callers.

The legacy default remains always-on. The switcher remains an explicit opt-in
and admits only one opaque full-canvas source cell. No host config changes.

## Runtime safety and remaining gates

This is R3 work: the original switcher design and lease-recovery notes apply.
Current main's FFmpeg wrappers, output loop, headers, audio encoder, timestamps,
and output routing remain the integration baseline, not historical replacements.

Live source edits in an opted-in switcher invalidate its catalogue entry and
retire the old producer instead of admitting stale source pixels. A restart is
required to attach the edited source. Non-switcher legacy edits retain their
existing behavior.

Audio follow must use the source of the actual composed item, not a global
latest switcher snapshot: an encode backlog must not apply tomorrow's selection
to yesterday's video. Warm source leases run off-clock; gain selection uses the
existing off-clock program audio bus. Failed takes retain the old item selection.
The native two-source audio proof uses a real decoded MPEG-2/AAC 400 Hz file
and the existing 1 kHz bars producer. Three completed CUTs change both video and
the exact pre-encode PCM; spectral checks prove that Preview does not mix into
Program. A two-tone unit test retains the selected tone across rejected-take
metadata, and a separate test preserves authored gain and live mute.

## Verification record

- Source formatter and whitespace checks pass.
- Native-free focused config/core/control tests: 47 pass, including nine lease
  chaos cases and actual HTTP route tests.
- Focused engine tests: 39 pass. Idempotency unit tests: seven pass.
- Canonical OpenAPI/AsyncAPI and TypeScript generation completed on this backend.
- FFmpeg 9 SDK ABI 63/63/61 native tests: three switcher tests pass, including
  actual HTTP cold-preview/CUT/distinct JPEGs and correlated terminal outcome.
- Native audio/video test: one pass across three completed CUTs. Native focused
  audio/retirement units: three pass. Cold-source real startup: one pass.
- Backend library strict Clippy passes. Native CLI strict Clippy is a separate gate.
- Adversarial review and CI remain incomplete. This note is not a ship claim.

No deployment, host opt-in, full test suite or main/root edit was performed.
