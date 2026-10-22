## Class

<!-- Run `scripts/classify.sh` and paste its output here. The class is a floor you may
     raise, never lower; the gates it owes are in docs/standards/engineering.md Part A.
     CI's "change class" job prints the same thing, advisory. -->

## What and why

<!-- What this changes and why. Link the issue, ADR or brief it comes from. -->

## Evidence

<!-- Command + output + exit code for the gates this class owes beyond CI. Don't restate
     fmt/clippy/test/deny/link checks — CI proves those mechanically. Do report anything
     that skipped or failed; a green summary over a skipped suite is a defect. -->

## Risk notes

<!-- Trade-offs, what could break, follow-ups, and what a reviewer should attack first.
     At least one substantive risk statement — "no risk" is a yellow flag. -->

## Checklist

- [ ] Respects the canonical invariants (output-clock, isolation, NV12-throughout, color order) — see `docs/architecture/conventions.md`
- [ ] Inclusive language throughout (code, comments, docs, commits) — see `CODE_OF_CONDUCT.md`
- [ ] Commit messages signed off (`git commit -s`, DCO)
