// Which accepted commands the server correlates to a realtime outcome.
//
// KEEP IN SYNC WITH `CorrKey::for_command` in
// crates/multiview-control/src/realtime.rs. Only the commands that function maps
// to `Some(..)` get an envelope `corr` echoing the 202's `operation_id`; every
// other command (apply-layout, swap/route, tally override, media load/seek,
// overlay/source upserts, device/cast/sync-group ops, ...) rides the stream
// UNCORRELATED, so waiting for a `corr` would only ever time out. The 202 body's
// `kind` is `Command::kind()` (crates/multiview-control/src/command.rs), so the
// set below is keyed by those kind strings:
//
//   Start / Stop                         -> "start" / "stop"
//   ArmSalvo / TakeSalvo / CancelSalvo   -> "arm_salvo" / "take_salvo" / "cancel_salvo"
//                                           (the salvo routes always name the salvo)
//   MediaTransport Play/Pause/Stop/Cue   -> "media_play" / "media_pause" / "media_stop" / "media_cue"
//   ArmMediaExit / TakeMediaExit /
//   CancelMediaExit                      -> "arm_media_exit" / "take_media_exit" / "cancel_media_exit"
//
// Deliberately absent: "media_load", "media_seek" (ambiguous outcome), "apply_layout",
// "swap_source", "route_*", "set_tally_override", overlay/source upserts.

/** `Command::kind()` strings for which `CorrKey::for_command` returns `Some`. */
export const CORRELATED_COMMAND_KINDS: ReadonlySet<string> = new Set([
  'start',
  'stop',
  'arm_salvo',
  'take_salvo',
  'cancel_salvo',
  'media_play',
  'media_pause',
  'media_stop',
  'media_cue',
  'arm_media_exit',
  'take_media_exit',
  'cancel_media_exit',
]);

/** The `kind` an Idempotency-Key replay acceptance carries (original kind is not echoed). */
export const REPLAY_KIND = 'replay';

/** True when an accepted command of this `kind` will get a `corr`-stamped outcome. */
export function isCorrelatedKind(kind: string): boolean {
  return CORRELATED_COMMAND_KINDS.has(kind);
}
