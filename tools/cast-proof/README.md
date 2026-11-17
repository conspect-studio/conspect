# tools/cast-proof — Google Cast playback proof

Automated proof that the engine's cast output (ADR-0111, `docs/runbooks/cast.md`) plays the way a Chromecast
plays it: the real engine, the real Cast receiver SDK (CAF 3.x, loaded from Google), Shaka Player 4.15.56 with the
SDK's own defaults, and the receiver app the engine embeds (`crates/multiview-control/src/cast_receiver/`, read
from there; no second copy exists), all in headless Chrome on a fake Cast platform (`fake-cast-platform.mjs`
answers the SDK's `ws://localhost:8008/v2/ipc`).

## Run

```sh
cargo build -p multiview-cli --features gpl-codecs,overlay  # libx264 (H.264 encoder); overlay wires caption sources
cargo build -p multiview-output --example cast_validate
npm ci --prefix tools/cast-proof
tools/cast-proof/run.sh                                     # [--size 1920x1080] [--cores N] [--seconds 60]
                                                            # [--engine BIN] [--validator BIN] [--out DIR]
                                                            # [--serial | --parallel]
tools/cast-proof/run.sh --reference                         # ffmpeg reference stream + fault tests (run-all.sh)
```

Browser: `CHROME_PATH`, else the newest Playwright Chromium in `~/.cache/ms-playwright`. `check-browser.mjs` runs
first and stops the run (exit 93) when the browser cannot play H.264 + AAC through MediaSource. Needs Node 20+,
`openssl`, `curl`, `python3`, outbound HTTPS to `www.gstatic.com` and `ajax.googleapis.com`. One run per host (the
SDK hard-codes port 8008). `ffmpeg` (with libx264) writes the caption source of check (h). Exit code = number of failed required checks; `out/summary.json` holds every number,
`out/engine.log` the engine's output.

## What `run.sh` does

It generates a config (canvas `--size` at 30000/1001, two `bars` sources with a 1 kHz tone via `--program-audio`,
one HLS output, one cast output, `[control] listen` on a free `[::1]` port; for (h), #418: a `kind = "file"` source
`cc` written by `gen-captions.sh` (testsrc + an 880 Hz AAC tone + a subrip stream with one cue `CAP <n> T=<2n>` per
2 s, 25 min long because a file source plays once) with `[sources.captions] mode = "auto"`, shown in no cell; the
cast output gets `[[outputs.slots.audio]]` x2 (the program, en, NAME "Program"; `track = "commentary"`, de, from an
`[[audio.routes]]` of `cc` with `target_track = "commentary"`, title "Commentary") and one `[[outputs.slots.subtitle]]`
"English"/en with `source = "cc"`), starts `multiview run`, puts
`proxy.mjs` (TLS, self-signed, transparent: only `Host` is rewritten) in front of the control listener because a
receiver page must be HTTPS, creates a stream with `POST /api/v1/cast/streams {feed: program}` and waits for a full
10-segment window. Every URL the browser loads in (a)-(g) is an engine route.

| check | proves |
|---|---|
| a `cast_validate` | the cast directory is a valid CMAF HLS stream (playlists, init, segment timing, PDT) |
| b Shaka, SDK defaults, 60 s | the stream plays with the configuration a Default-Media-Receiver device uses: start, continuity, latency, buffer, frames, audio |
| b' Shaka, receiver config | the same with the playback config our receiver app sets (read from receiver.js) |
| c receiver SDK + engine receiver, 60 s | the receiver's probe reaches the engine's ladder route and is stored, the ladder offers `hls`, `first_frame_ms` and a tick-to-present latency by `frame` reach the engine (ADR-0114 slice 2), MEDIA_STATUS PLAYING within 15 s, no BUFFERING > 1 s, latency 6-10 s, overlay, custom-namespace reports, reports reach the ENGINE (`last_report`, `fetches`, `cast_device: false`), CSP clean, token never logged |
| d Default Media Receiver equivalent, 30 s | a bare SDK page on another origin plays the stream: the engine's CORS headers work for real |
| e takes during playback | `POST /api/v1/routing/output/take` to source `b` and back: no IDLE, BUFFERING < 3 s in total, no reload, the validator still passes, at most 2 discontinuities |
| f revoke | `DELETE` → master 404, the receiver shows "ended" and stops requesting, the cast directory empties after the linger, a second stream works (new token, new init files) |
| h tracks (#418), `test-engine-tracks.mjs` | h1: `cast_validate --json` passes with 2 audio + 1 subtitle rendition, a `.vtt` for every video segment, cues > 0. h2: Shaka 4.15.56 with SDK defaults lists audio en/"Program" + de/"Commentary" and text en/"English"; switching to de (`selectVariantTrack`, keeping the buffer) has no stall > 1 s, no skipped media, audio keeps decoding and the de rendition is fetched; the text track shows a `CAP <n> T=<s>` cue, every cue on one timeline (+-0.5 s). h3: real CAF SDK + engine receiver: MEDIA_STATUS 2 AUDIO + 1 TEXT with names/languages, `EDIT_TRACKS_INFO` [Commentary, English] switches to de (no BUFFERING or stall > 1 s) and shows a cue, and the engine's `last_report` carries the tracks with the new choice. Informational h2i: a CAF-style switch (clearBuffer) under CAF's default rebufferingGoal 10 s |
| probe-unit (`test-probe-unit.mjs`, no browser) | the receiver's `probe.js` (ADR-0114 slice 2): clock offset and its error bound, an epoch step discards samples, latency windows and the 0-60 s sanity rule, `playhead` without rVFC, `unavailable` without a clock, the probe body, and that `probe.js` and `receiver.js` are ES5 syntax only. Also a gating step of the CI job |
| g authz | one changed token character → 404 with the unknown-file body; wrong bearer → 401 |
| HLS comparison (informational) | Shaka with SDK defaults on the existing `/hls/` output; expected to miss (3 s seek range), never a failure |

`stream-health.mjs` measures how far the stream runs behind real time (PDT vs wall clock) before and after: a late
stream means the host starved the encoder, so latency failures then describe the host, not the player.

`--reference` (`run-all.sh`) keeps the earlier mode: an ffmpeg reference stream behind the harness's own server
(`server.mjs`, which applies the engine's CSP), needed by `test-receiver-faults.mjs` for faults the engine cannot
be told to produce (held segments, corrupt segments, an unreachable server, generator restart).

## Tracks (#418): `test-receiver-tracks.mjs`

Run by `run-all.sh` (skip with `--skip-tracks`) or alone: `node test-receiver-tracks.mjs [--cases a,b,c,d,e,f]
[--offset 100000]`. Its stream comes from `gen-tracks-stream.mjs`: ffmpeg writes video + two AAC renditions
("Program"/en 440 Hz, "Commentary"/de 880 Hz) as CMAF fMP4 with the media timeline (`tfdt`) starting at `--offset`
seconds (default 100000 s, past the 33-bit MPEG-TS wrap at 95443.7 s, like an engine that has run for a day); the
script adds a segmented WebVTT rendition ("Captions"/en, one `.vtt` per video segment, same sequence numbers,
EXTINF and PDT, one cue `CUE <seq> T=<tfdt s> PDT=<ms>` at 0-1.5 s of the segment) and the master.

| case | proves |
|---|---|
| a | CAF publishes `media.tracks` (2 AUDIO + 1 TEXT, names/languages from the manifest) and `activeTrackIds` [Program] |
| b | `EDIT_TRACKS_INFO` [Commentary, Captions]: Shaka's active variant becomes de, no BUFFERING or stall > 1 s; the text track is shown, every cue starts at its segment's media time (+-0.5 s, timed by PDT, not by X-TIMESTAMP-MAP) and a known cue is active at its time; reports carry `tracks` |
| c | LOAD `customData.audioLanguage: 'de'` starts on Commentary (the first audio segment fetched is Commentary's) |
| d | the choice survives the receiver's own reload (segments held 36 s -> the 20 s BUFFERING watchdog reloads) |
| e | LOAD `activeTrackIds` is honoured; a sender's `media.tracks` (with a text track URL) is dropped and the URL never fetched; `EDIT_TRACKS_INFO` with an unknown id is refused by CAF and changes nothing |
| f | `customData.textLanguage: 'en'` starts with subtitles, `textLanguage: null` without |

X-TIMESTAMP-MAP with Shaka 4.15.56 (measured with this stream; source `lib/text/vtt_text_parser.js`
`computeHlsOffset_`, `lib/media/media_source_engine.js` `appendBuffer`): the text timestamp offset is the VIDEO
SourceBuffer's offset (playlist time - video `tfdt`), so `MPEGTS` is the video `tfdt` of the segment start in
90 kHz, with `LOCAL:00:00:00.000`. Shaka undoes the 33-bit wrap itself (it adds 2^33 while the segment's media time
is at least 95443.7 s), so `MPEGTS` must be written modulo 2^33: with `tfdt` at 100000 s the wrapped value puts every
cue on time (0.008 s), the unwrapped value puts them 95443.7 s late (never shown).

## What it cannot prove

- A real device: certificate trust of the Let's Encrypt chain on Chromecast firmware, the device's hardware
  decoder, the sender's Cast picker and the device registration of the app id. Only the operator's device test
  proves these.
- The SDK runs in desktop Chrome on a fake platform; `setvolume` and app close go to the fake, not to a TV.
- CI size: the CI job (`cast-proof` in `.github/workflows/ci.yml`) runs on a 4 vCPU GitHub runner at 960x540 with
  a release engine, `--serial`. No size passes 5 of 5 on 4 cores (table below), so there the playback step is
  reported (warning + artifact), not gating; the cast pipeline + chaos tests in the same job are a gate. CI therefore
  does NOT prove 1080p decode pacing, nor the 6-10 s latency window on a small host. The full-size proof is
  `run.sh` on a host with 8 or more cores. A debug engine cannot hold the 29.97 fps cadence on 4 cores at all (it
  stops itself: "output fell 12 ticks behind the clock"), so use release binaries for `--cores` runs.
- `--serial` (default with `--cores` or `CI`) runs the browsers one at a time: on a small host, two software Chrome
  decoders beside libx264 starve the encoder.

## Measured on the dev host (dated numbers, not a standing claim)

Host: 56 cores, shared with other lanes (load average 10 to 55 during the runs), release engine at
`7a5eb120`'s tree, Chromium 156 (Playwright), 14 Nov 2026.

Full size, all cores, parallel (`run.sh`), three runs in a row, all required checks passed:

| run | (c) receiver: PLAYING after LOAD | latency | buffer min | stalls | dropped | (b) Shaka SDK defaults: start / latency | (d) DMR equivalent: PLAYING | (a)/(e) validator, discontinuities after the takes | (e) BUFFERING across takes |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 411 ms | 8.5 s | 6.4 s | 0 | 0.11 % | 6558 ms / 13.6 s | 766 ms | pass, 0 | 0 ms |
| 2 | 431 ms | 9.4 s | 6.5 s | 0 | 0.11 % | 6391 ms / 14.1 s | 6388 ms | pass, 0 | 0 ms |
| 3 | 389 ms | 8.6 s | 6.2 s | 0 | 0.11 % | 6390 ms / 13.6 s | 776 ms | pass, 0 | 0 ms |

(f) in every run: DELETE then master 404, the "ended" screen within about 2 s, 0 requests afterwards, the cast
directory empty about 17 s after the DELETE (10 s linger), a second stream ready with a new token. (g) passed. The
HLS comparison (Shaka, SDK defaults, `/hls/`) misses as expected: seek range 3 s.

Check (h), #418 (release engine `92767a147` + overlay, `--size 960x540 --serial`, all cores, load average 27 to 42,
three runs in a row, every required check of (a)-(h) passed): validator 2 audio + 1 subtitle rendition, 10 of 10
subtitle segments, 20 cues; Shaka (SDK defaults) audio switch to de in 3 to 4 ms, 0 s stall over 15 s, no skipped
media, 8 de / 0 en audio segments fetched after it, cues on one timeline within 0.28 s; receiver `EDIT_TRACKS_INFO`
switch 8 to 14 ms, BUFFERING 213 to 276 ms, 0 s stall, the engine's `last_report` carries Commentary + English active.
Informational h2i (clearBuffer switch under CAF's rebufferingGoal 10): lag 0.26 to 1.16 s. Known engine limit (not a
cast fault): an in-container text cue's end is not honoured (each cue is held until the next, 4 s at most), so the
`.vtt` cues here last 2 s instead of 1.5 s; only cue starts are checked.

Pinned to 4 cores (`--cores 4`, release engine), all required checks:

| size | mode | passed | what failed |
|---|---|---|---|
| 1920x1080 | parallel | 0 of 5 | the engine stopped itself (encoder behind the clock) in all 5 |
| 1920x1080 | serial | 0 of 3 | engine stopped (2 runs); one run Shaka waiting 910 ms |
| 1280x720 | parallel | 1 of 5 | engine stopped (2); receiver latency 10.7 s (2) |
| 1280x720 | serial | 1 of 3 | receiver latency 10.5 s; DMR BUFFERING 2.5 s |
| 960x540 | parallel | 2 of 5 | receiver latency 10.4 to 10.9 s (3 runs; the window is 6 to 10 s) |
| 960x540 | serial | 0 of 1 | Shaka (SDK defaults) waiting 1024 ms; receiver latency 10.4 to 10.7 s |

The 4-core misses are at most about 1 s outside a criterion, with the stream itself on time (`stream_health` lag
under 2.1 s); the criteria were not widened. Cast pipeline tests on 4 cores
(`taskset -c 8-11 cargo test -p multiview-cli --features gpl-codecs --lib -- cast_output binding_delivery`): 38
passed, 3 of 3, 61 s each.
