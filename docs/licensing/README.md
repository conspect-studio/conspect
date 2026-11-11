# Conspect licensing — source-available, limited free use

## Current policy — Version 2.0

The operative [license](../../LICENSE) is Version 2.0. The original rollout record
below describes Version 1.0 and is historical where it differs from this policy.
Counsel review remains separate; engineering review is not legal sign-off.

- Free use is **personal, home, hobby and learning use**, plus the express First
  Nations Owned Broadcaster, Community Broadcaster and non-monetized Content Creator
  exceptions. Creators must have fewer than 55,000 aggregate audience members.
- **Religious use is never eligible**, including solo religious creators, regardless
  of size, charitable status or income.
- **Any monetization of content made in whole or part with Conspect requires paid
  licensing**, including ads, sponsored posts, subscriptions, affiliate income,
  content-linked tips/donations, and free goods or services supplied for promotion.
  No free exception overrides this rule; the old community sponsorship allowance
  is not a monetization allowance under Version 2.0.
- **Paid and metered features are excluded** from free use. Free use has **no
  warranty**, including fitness for use, subject to non-excludable legal rights.
- Source access permits private study and authorized use, not general copying,
  modification, redistribution or derivative reproduction. Asking an AI agent to
  reproduce Conspect from its code violates the license outside express grants.
  Third-party rights, mandatory legal rights, and independent implementations of
  general ideas remain unaffected.
- Earlier valid grants for earlier copies are not retroactively revoked.

## Historical Version 1.0 rollout record (not current eligibility guidance)

> **Historical status:** Version 1.0 was adopted on the original branch, pending
> external legal review. The repo-root license now contains Version 2.0; the original
> Version 1.0 is available in Git history. This record does not describe current
> eligibility, modification or distribution rights.
>
> A clear license in place is deliberate — it prevents the repo from ever sitting with **no** license
> during the transition off `MIT OR Apache-2.0`.

## What's in this package

| File | What |
|---|---|
| [`../../LICENSE`](../../LICENSE) | **Current license: Version 2.0** (the historical Version 1.0 was PolyForm-Noncommercial-derived) — commercial definition, the three free exceptions, the use-trigger and licensor-reservation clauses. Aperim Pty Ltd (ABN 46 150 699 737), NSW law. |
| [`../../LICENSE-COMMERCIAL.md`](../../LICENSE-COMMERCIAL.md) | Commercial-license pointer (contact, OEM/appliance tier, codec/NDI notes; native Dante is NOT supported — AES67/ST 2110-30 is the open Dante interop path). |
| [`relicense-advisory.md`](relicense-advisory.md) | The full legal/licensing advisory — feasibility, base instrument, the three exceptions, the GPL crux, tooling, the exact repo-change checklist, enforcement & trademark, open decisions. Multi-agent researched + adversarially reviewed. |
| [`build-and-distribution.md`](build-and-distribution.md) | Engineering companion: the **public-vs-private build/distribution matrix** (codecs / NDI; native Dante is not a planned feature, AES67/ST 2110-30 is public), LGPL container hygiene, and the **CI fan-out workstream** to generate & publish distinct public/non-public artifacts. |

## Historical Version 1.0 summary (not current eligibility)

A **dual-license**: public code under the **source-available, non-commercial** `LICENSE` (free home
use; three free carve-outs); a **separately-sold commercial license** for everyone else (business,
**education, government**, productization, streamers/creators), with an OEM/appliance tier.

**Four hard conditions** (per the advisory): (1) own 100% of the copyright — a **CLA** is required
before any outside contribution (DCO does not grant relicensing rights); (2) relicense **before** the
first public tag; (3) the **`gpl-codecs` (x264/x265) GPL build is hard-incompatible** with a
non-commercial license and must never ship in a distributed artifact; (4) counsel reviews the bespoke
definitions. And: it is **source-available, not "open source"** (fails OSD §§5–6 / FSF Freedom 0).

## Historical operator refinements in Version 1.0 (not Version 2.0 grants)

1. **Use — not just modification/distribution — is the trigger.** Running the software *as supplied or
   as modified, obtained directly or from a fork*, commercially is itself the licensed (paid) act
   (`LICENSE` §4).
2. **Historical fork/derivative reach.** Version 1.0 allowed recipients to receive
   forks on those same terms. Version 2.0 instead requires separate written
   permission for modification and distribution; this historical rule grants none.
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
- **Historical Version 1.0 question, now resolved:** the former optional revenue
  gate is superseded by Version 2.0's mandatory no-monetization rule. There is no
  minimum income threshold; promotional non-cash benefits also count.
- **Bare-license vs contract** characterisation (affects the infringement remedy — for counsel).
- **CLA timing** — live before the first external PR.
- **crates.io** — recommend keep `publish = false`.

## Provenance

Produced by a 9-agent research workflow (5 parallel research tracks → synthesis → 2 adversarial
reviewers → final edit; 21 corrections applied), then extended by the operator's refinements
(use-trigger, reservation-of-rights, build/distribution split, three-workstream rollout)
and finalized with the registered entity details. Worked entirely in the `docs/license-noncommercial`
worktree branch; the root working tree was not touched.
