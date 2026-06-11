# Multiview licensing — source-available non-commercial

> **Status: ADOPTED for this branch (pending external legal review).** The operative license is the
> repo-root [`LICENSE`](../../LICENSE) (final, Version 1.0) with [`LICENSE-COMMERCIAL.md`](../../LICENSE-COMMERCIAL.md)
> as the commercial pointer. Treat the text as the live license, not a draft. The operator is having
> it reviewed by counsel separately; this package is the research/justification record behind it.
>
> A clear license in place is deliberate — it prevents the repo from ever sitting with **no** license
> during the transition off `MIT OR Apache-2.0`.

## What's in this package

| File | What |
|---|---|
| [`../../LICENSE`](../../LICENSE) | **The license** (final, Version 1.0, PolyForm-Noncommercial-derived) — commercial definition, the three free exceptions, the use-trigger and licensor-reservation clauses. Aperim Pty Ltd (ABN 46 150 699 737), NSW law. |
| [`../../LICENSE-COMMERCIAL.md`](../../LICENSE-COMMERCIAL.md) | Commercial-license pointer (contact, OEM/appliance tier, codec/NDI notes; native Dante is NOT supported — AES67/ST 2110-30 is the open Dante interop path). |
| [`relicense-advisory.md`](relicense-advisory.md) | The full legal/licensing advisory — feasibility, base instrument, the three exceptions, the GPL crux, tooling, the exact repo-change checklist, enforcement & trademark, open decisions. Multi-agent researched + adversarially reviewed. |
| [`build-and-distribution.md`](build-and-distribution.md) | Engineering companion: the **public-vs-private build/distribution matrix** (codecs / NDI; native Dante is not a planned feature, AES67/ST 2110-30 is public), LGPL container hygiene, and the **CI fan-out workstream** to generate & publish distinct public/non-public artifacts. |

## TL;DR

A **dual-license**: public code under the **source-available, non-commercial** `LICENSE` (free home
use; three free carve-outs); a **separately-sold commercial license** for everyone else (business,
**education, government**, productization, streamers/creators), with an OEM/appliance tier.

**Four hard conditions** (per the advisory): (1) own 100% of the copyright — a **CLA** is required
before any outside contribution (DCO does not grant relicensing rights); (2) relicense **before** the
first public tag; (3) the **`gpl-codecs` (x264/x265) GPL build is hard-incompatible** with a
non-commercial license and must never ship in a distributed artifact; (4) counsel reviews the bespoke
definitions. And: it is **source-available, not "open source"** (fails OSD §§5–6 / FSF Freedom 0).

## Operator refinements baked into the LICENSE

1. **Use — not just modification/distribution — is the trigger.** Running the software *as supplied or
   as modified, obtained directly or from a fork*, commercially is itself the licensed (paid) act
   (`LICENSE` §4).
2. **Fork/derivative reach.** A fork or derivative is licensed to its recipients only on the same
   terms; commercial use of it (or of the unmodified software) is Commercial Use (`LICENSE` §4).
   *(Honest boundary: copyright reaches copying/derivatives, not abstract functionality — a clean-room
   reimplementation isn't reachable; trademark is the complementary lever. Advisory §9.)*
3. **Aperim is the sole licensor and is not bound by its own license** — explicit Reservation of the
   Licensor's rights (`LICENSE` §10.1). This is *why* the **CLA** matters for keeping dual-licensing.
4. **Multiple builds; encumbered artifacts stay private** — see `build-and-distribution.md`. Encumbered:
   **`libx264`/`libx265` (GPL), NDI**; native Dante is NOT supported (ADR-T010); everything else (incl.
   all *hardware* H.264/H.265 and the **AES67/ST 2110-30** route to Dante audio, the clean public path)
   is public.

## Rollout — three coordinated workstreams

The `LICENSE` is in place. To make the repo's **declared** license consistent with it, two more
workstreams follow (they touch many files and partly need coordination with other in-flight work):

1. **License finalization — DONE on this branch.** `LICENSE` + `LICENSE-COMMERCIAL.md` landed; counsel
   review to follow.
2. **Doc/metadata refactor.** A single coordinated pass per the **exact checklist in advisory §8** —
   Cargo.toml `license` → `license-file` + the mandatory `license-file.workspace = true` key-rename
   across **all 20 members**; remove root `LICENSE-MIT`/`LICENSE-APACHE`; `web/package.json`; the
   OpenAPI **generator macro** + AsyncAPI; `conventions.md` §7 first; `CONTRIBUTING.md` DCO→CLA; new
   **ADR-0043** superseding ADR-0012; and the **"open source" → "source-available"** sweep. Plus a CLA
   doc + `THIRD-PARTY-NOTICES`.
3. **CI/release split (fan-out).** Generate & publish **distinct public/non-public** binaries and
   containers, with a hard "no-leak" gate on the public lane. Teams A–D + gating in
   `build-and-distribution.md` §6. Additionally blocked on the NDI feature existing (native Dante is NOT a planned feature — ADR-T010).

## Open decisions (full list in advisory §10)

- **Commercial price/tiers** (incl. OEM/appliance tier; evaluation window).
- **Trademark name** — coin a distinctive product name under the **Aperim** house mark ("Multiview"/
  "multiviewer" is generic → a bare word mark is likely refused/weak).
- **Optional secondary revenue gate** on the Content Creator Exception (omitted from `LICENSE` v1.0 for
  cleanliness; counsel can add a USD threshold if wanted).
- **Bare-license vs contract** characterisation (affects the infringement remedy — for counsel).
- **CLA timing** — live before the first external PR.
- **crates.io** — recommend keep `publish = false`.

## Provenance

Produced by a 9-agent research workflow (5 parallel research tracks → synthesis → 2 adversarial
reviewers → final edit; 21 corrections applied), then extended by the operator's refinements
(use-trigger, reservation-of-rights, build/distribution split, three-workstream rollout)
and finalized with the registered entity details. Worked entirely in the `docs/license-noncommercial`
worktree branch; the root working tree was not touched.
