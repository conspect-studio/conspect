# Note from the lead (2026-11-12 05:50 UTC) - read, then delete this file

Untracked on purpose (stops your push gate until read).

A restart of `conspect` on mda001 caused a 2 min 11 s outage today (16:42:34-16:44:45 AEDT). Cause:
the operator had applied a 6-cell layout from the UI at 11:03 with the switcher on; that state runs
but the engine refuses to START from it. A binary rollback did not help.

Before ANY restart there, run the preflight now in `/workspaces/conspect/.worktrees/.scratch/ship-1111/RULES.md`
("Corrections" block, last two items: the `desired.json` boot check, and how to build there:
cargo on PATH, `web/dist` in the source tree). If the check does not print OK, do not restart;
send the lead one line. The operator can press "Apply" again at any time, so check immediately
before the restart, not earlier.

Also: the lead built engine de196346ae91 from main c094e9f7 at `~/lead-c094e9f7` on the host and
will deploy it. If your deploy comes after, build from a `main` that contains yours, and read the
tail of `~/lead-bussing-kit-20261111/cutover.log` first.
