# docs/runbooks — operational runbooks

One runbook per provisioned resource/procedure, written **in the same commit** that provisions or changes the resource (a database, queue, bucket, worker, scoped token, DNS/route, CI secret, local dev service).
Each runbook: what it is and why, the exact command/API used to create/change it, its id/name/binding, how to verify it, how to rotate/recreate/restore/roll back.
Present tense, executable, kept current — never aspirational.
`docs/operations/` holds narrative how-to guides for the whole system; `docs/runbooks/` holds resource-scoped runbooks. Link a runbook from the relevant guide instead of duplicating when it grows into general guidance.
