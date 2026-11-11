# R3 causal chaos proof log (player adoption)

Command: `cargo test --locked -p multiview-cli --features ffmpeg --lib player_adoption_chaos_tests -- --test-threads=1` — 6 passed.

Tests live in `pipeline.rs` `player_adoption_chaos_tests`. Production change: none; only a `#[cfg(all(test, feature="ffmpeg"))]` per-runtime barrier (`seek_barrier`, keyed by the runtime's opener `Arc` address) called `#[cfg(test)]` before the native seek in the worker's Seek/Cue handlers. Seek stays on the worker; ordering unchanged.

| Test | Proves |
|---|---|
| output_clock_emits_exact_ticks_while_player_worker_is_blocked_before_native_seek | Real worker + real opener/audio rail commit A; worker parked before the actual `demuxer.seek`/flush; real `OutputClock` + production `CompositorDrive::compose` emit exactly 120 frames, tick index/PTS = `MediaTime::from_tick`, canvas centre luma = A (not NA), worker still blocked after; release then lets native seek proceed, no fault. Only wall-clock use: 20 s hang deadline (fails). |
| load_storm_through_real_opener_stays_bounded_and_clock_exact | 10 000 real `opener.request` against live opener+worker: retired_len <= 2, latest-wins, clock exact, picture never NA. |
| worker_panic_after_commit_isolated_from_output_clock | Injected worker panic: runtime unregistered, owners handed to opener, clock/compositor unaffected. |
| stuck_http_opener_never_stalls_clock_and_newer_load_cancels_it | Silent TCP peer blocks the real opener; 100 ticks exact; newer load cancels well before the 5 s deadline. |
| default_asset_probe_adopts_real_geometry | Boot sentinel (out_point 1) is legitimate; the probed default asset yields ack 25 frames, 25/1, 64x48, audio, 1 s, first image adopted. |
| killed_child_flock_recovery_requires_boot_quiescence | Ported from donor 81ce5510 to current paths: child SIGKILLed at precommit hook; live owner excludes a second boot; recovery stays pending until opener+audio quiescence, then `Retired`. |

## Limitations
- Blocked-seek test drives the compositor/OutputClock directly (the same drive `drive_streaming` builds), not the full `Pipeline::run_for` encode/mux path; manual time, not real-time pacing.
- Opener-retire stall and an FFI-destructor stall are not injected (would need production seams); retirement is covered by the storm bound and the panic handoff.
- The panic test asserts isolation and unregistration, not native destruction timing.
- The Cue seek path shares the barrier call but only Seek is exercised.
