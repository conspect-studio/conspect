# LEAD NOTE (read, then delete this file; an untracked file blocks your push gate)

main f8c3f370 (PR 355) fails clippy::large_futures at crates/multiview-control/src/patch.rs:28 in PR CI (fmt+clippy and the feature-gated clippy legs). When you merge origin/main: if PR 349 is not merged yet, apply the same one-line fix it uses: `runtime.spawn(Box::pin(persist_relay(state, control)))`. Once PR 349 is on main, merging main fixes it.
