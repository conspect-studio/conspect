# Conspect Version 2 license clarity and product naming

- Status: Accepted for PR and merge delivery by the operator on 2026-11-11; deployment remains separate (R3).
- Date: 2026-11-10
- Scope: license policy and documentation/UI copy; no enforcement or media-path changes.
- Supersedes: ADR-0043 eligibility and source-reuse guidance for copies supplied under Version 2.0 only.

## Approved policy and implementation plan

The operator requests Conspect branding and a narrow free-use license. Preserve the
First Nations and Community Broadcaster exceptions and the creator exception below
55,000 aggregate audience members, but exclude religious use of every size and any
monetization of content produced wholly or partly with Conspect. Ads, sponsorships,
promotional gifts and other commercial benefits count. Unrelated income does not
by itself disqualify genuinely personal use. Free use excludes paid/metered features
and provides no warranty, subject to non-excludable legal rights.

The operator explicitly selected a new, narrower source grant: authorized product
use and private study, including necessary authorized-use copies, without a general
modification, redistribution or reproduction grant. The README gives the requested
AI-agent reproduction example. Ideas, independent implementations, third-party
licenses and mandatory rights remain outside the restriction.

1. Audit current product documents and embedded UI; preserve generic multiview
   feature terminology, compatibility identifiers, paths and historical records.
2. Revise the operative license as Version 2.0 and align current summaries/notices.
3. Coordinate the separate website repository through a new remote session.
4. Regenerate translations and run focused UI checks; review three R3 lenses.
5. Deliver reviewed PRs, resolve CI and merge. The operator explicitly directs end-to-end delivery without further PR-permission prompts; deployment remains separate.

## Migration, alternatives and recovery

Retaining the old broad copy/modify/distribute grant would contradict the operator's
selected policy. Retroactively revoking earlier grants is not an option. Version
2.0 applies to copies supplied under it; older copies keep their applicable grants.
The eventual merge/release commit identifies the new terms' cut point in Git history.
No claim is made that all existing installations are covered by the new terms.

Preserve existing CLA grants over contributors' own contributions. The CLA does not
grant permission to modify the rest of Conspect. Contributions that need modification
or redistribution require separate written permission; a future contribution policy
must resolve that workflow explicitly rather than invent permission here.

Before publication, recovery is a normal revert of this proposed change. After
publication, reverting text cannot revoke rights already granted or erase versions
already supplied. Counsel review is separate from engineering review. No chaos/soak
test applies: this task changes no output-clock or isolation behaviour.

## Review evidence

- xAI `x-ai/grok-4.7`: policy consistency, eligibility examples and branding/compatibility lenses.
- Anthropic `claude-sonnet-5-5`: migration, prior grants, third-party rights and contribution lens.
- Authors used OpenAI/DeepSeek routes. Reviews used fresh contexts, not author forks.
- Accepted fixes include GPL-feature auto-relicensing claims, stale historical summaries,
  DCO wording, source-of-truth priority and notices naming the old license version.
- Rejected review suggestions: remove necessary authorized copies; narrow legally
  required LGPL rights; prohibit income from unrelated content; revoke prior CLA grants.
  These contradict the approved copy allowance, third-party protections or migration boundary.
- These are engineering consistency reviews, not legal sign-off. Final commit/check/PR
  evidence belongs in the PR. The operator confirmed end-to-end PR/merge delivery on
  2026-11-11; remote CI and final website reconciliation remain delivery gates.
