# ADR-W027: Config-file watch fingerprint folds a content hash (supersedes ADR-W020 §1's stat-only fingerprint)

- **Status:** Accepted
- **Area:** Web/API stack · config-as-code ↔ engine (invariant #11 live-apply classification)
- **Date:** 2026-07-11
- **Source:** cross-vendor review of PR #251 (task #7); supersedes the "Known limit (accepted)"
  of [ADR-W020](ADR-W020.md) §1

## Context

[ADR-W020](ADR-W020.md) §1 detects config-file changes by polling the path and fingerprinting
`(len, mtime, inode)`, acting when the same new fingerprint is observed on two consecutive polls.
It recorded a **"Known limit (accepted)"**: a rewrite that preserves all three axes — a
same-length in-place write landing within the filesystem's mtime granularity, or one whose mtime
is restored by tooling — is invisible to the watcher. It rejected hashing the content each poll
"as a per-second read of the whole file for a corner no real writer hits", on the premise that
editors and deployment tools always change the inode (write-temp + `rename(2)`) or advance mtime.

That premise does not hold for several **legitimate** writers:

* a shell redirect (`config > file`, `sed -i`) rewrites the file **in place** — same inode;
* a non-atomic editor that truncates-and-writes in place — same inode;
* a config-management tool that rewrites the file in place.

When such a write is the **same length** as the adopted content and lands within the filesystem's
coarse mtime granularity, all three stat axes alias the already-applied fingerprint. The
`watch_loop` "already applied" short-circuit then drops the edit **without reading its content** —
the new bytes are never applied, silently. This is not theoretical: on the project's devcontainer
`/tmp` it reproduces ~293/300 for a same-length in-place rewrite, and it surfaced as a flaky
control-plane test (`promote_does_not_retrigger_a_file_watch_apply` and its neighbours) whose
"flake" was the defect firing. A test-only workaround (routing the tests through atomic
temp+rename to mint a fresh inode) was **blocked by the cross-vendor review** as masking a real
correctness defect rather than fixing it.

## Decision

Fold a hash of the file's **bytes** into the watch `Fingerprint` (a fourth axis alongside `len`,
`mtime`, `inode`), and read the file **exactly once per poll from a single fd**. `probe()` opens the
path once, reads that fd — deriving both `len` and the content hash (`fingerprint_content_hash(&bytes)`,
`DefaultHasher` — not an adversarial boundary; whoever writes the config already controls it) from
that **single read** — and `fstat`s the SAME fd for `mtime`/`inode`, then returns a
`Probed { fingerprint, bytes }`. `len` + the content hash and the carried apply-bytes are ONE
coherent read; the fd pins one inode, so `mtime`/`inode` belong to the same file as those bytes — a
rename can never pair one file's metadata with another file's bytes. (This is not a point-in-time
snapshot of *every* axis: the `fstat` follows the read, so a same-inode in-place rewrite landing
between them can move `mtime` past the bytes just read. That is benign — the content hash, coherent
with the applied bytes, is the authoritative discriminator, not `mtime`.) A same-length in-place
content change is then a **distinct fingerprint**, so it falls through
to the existing validate → apply path; the three fingerprint comparison sites use the derived
`PartialEq`, so they are content-aware with no further change.

Crucially, the settling poll **applies the bytes `probe()` already carried** (UTF-8-decoded in
place), rather than doing a second `read_to_string`. So the fingerprint recorded as `applied` /
`rejected` is always the fingerprint of the content **actually applied**: `len` and the hash come
from the same read as the applied bytes (`mtime`/`inode` are that fd's `fstat`, pinned to the same
inode). There is no probe-then-reread window in which a concurrent writer could make the applied
content diverge from the recorded fingerprint (the TOCTOU the cross-vendor re-review flagged) — the
guarantee is deterministic detection *of the content actually applied*, not merely "a content change
is noticed".

* **Raw bytes, not `&str`:** the content axis hashes the raw bytes, so a non-UTF-8 file still
  fingerprints as a change; the settled apply then UTF-8-decodes the carried bytes and, on failure,
  takes the "not valid UTF-8" reject (the UTF-8 gate, formerly done by the settled `read_to_string`).
* **Unreadable-but-present, typed:** the content axis is a typed
  `ContentFingerprint { Readable(u64), Unreadable }`, **not** an in-band sentinel value. If the
  bytes cannot be read this poll — the open is denied (permissions), or the fd opens but reads fail
  (e.g. the path is a directory, `EISDIR`) — `probe()` yields `Unreadable` and carries the read/open
  error in place of bytes, so two such polls settle to the "cannot be read" reject (with that errno)
  rather than aliasing unchanged content, and a real file's `Readable` hash can never collide with
  "unreadable" (there is no reserved hash value). A path whose open AND `stat` both fail is instead
  reported missing (`None`), unchanged from ADR-W020.
* **Collision:** a `DefaultHasher` `u64` collision is ~2⁻⁶⁴ and, on a **non-adversarial** input
  (the writer already controls the file), cannot be constructed to matter; a widened digest buys
  nothing here.
* **Cost, re-evaluated:** the config file is small and the single read rides the blocking pool at
  the 1 s cadence, so it is negligible — the "per-second whole-file read" objection in ADR-W020 §1
  is reversed. Each poll now **opens the path once** and reads that fd (one path resolution) where
  the stat-only design did a path `stat` plus, on a settled change, a second path read to apply (two
  resolutions); even a ~1 MB config hashes in well under a millisecond and is served from the page
  cache.

## Consequences

* A legitimate same-length in-place rewrite now hot-applies like any other edit; detection no
  longer depends on filesystem `stat` granularity or on the writer changing the inode/mtime.
* The content the watcher applies always matches the fingerprint it records as `applied` (the
  content axis and the applied bytes come from ONE read), so a settled apply can never leave
  `applied` pointing at content that was never applied — closing the class of silent drop a
  probe-then-reread would reopen.
* **Resume / self-write suppression are unperturbed** (ADR-W020 §5–§7): an atomic rename carrying
  identical bytes still adopts-without-applying via the `last_observed == text` arm (inode differs,
  hash matches); a `touch` (mtime moves, bytes unchanged) stays a no-op; an expected server-side
  write (promote) still adopts via its content-paired token.
* ADR-W020 §1's "Known limit (accepted)" is **resolved**; that ADR's Status is annotated to point
  here. The rest of ADR-W020 (invalid-file handling, per-section diff/apply, store follow, restart
  surface, status endpoint) stands unchanged.
* Regression coverage: a deterministic test forces the worst case (an in-place same-length write
  with mtime pinned to the adopted write's and the inode reused) and asserts the edit applies; the
  atomic-rename edits are kept as additional coverage. A second test interposes a concurrent write
  in the probe→apply window (a deterministic `with_post_probe_interpose` seam) and asserts the
  applied content matches the recorded fingerprint (the TOCTOU regression); a third characterises
  the typed `Unreadable` reject and its recovery (a present-but-unreadable path, modelled root-
  independently via a directory swap → `EISDIR`).

## Alternatives considered

* **Keep stat-only, document the limit louder / tell operators to `touch`** — rejected: silently
  dropping a legitimate operator edit is a correctness defect (inv #1, "bad inputs are the
  purpose"), and a cross-vendor panel blocked the equivalent test-only workaround.
* **Read content only when the stat matches the applied fingerprint** — no cost saving (the steady
  state IS the stat-match case, so it would read every poll anyway) and it would not fix the same
  latent aliasing in the rejected-latch; folding the hash into the fingerprint fixes all comparison
  sites uniformly.
* **Switch to `notify` (inotify/FSEvents)** — orthogonal and still rejected per ADR-W020 (a new
  cross-platform dependency for latency we do not need); the 1 s poll + content hash is simpler and
  robust across renames and network/overlay mounts.
