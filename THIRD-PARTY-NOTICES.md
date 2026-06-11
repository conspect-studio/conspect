# Third-Party Notices

Multiview's own source code is licensed under the **Multiview Source-Available
Non-Commercial License, Version 1.0** (see the root [`LICENSE`](LICENSE) file;
commercial licensing is described in [`LICENSE-COMMERCIAL.md`](LICENSE-COMMERCIAL.md)).
Multiview is **source-available**; it is **not** "open source" or "free software".

This file lists the **third-party components** that are distributed with, or linked
or loaded by, the Multiview software. Those components are **not** covered by
Multiview's license — each remains under **its own license**, and you must comply
with the terms of each component's license when you use, copy, or distribute
Multiview. Where a component carries its own copyright and license notices, those
notices are retained alongside the component (and where a component ships with
Multiview, its license text accompanies it).

Copyright © 2026 **Aperim Pty Ltd** (ABN 46 150 699 737), Suite 108, 9 Hutchinson
Street, St Peters, NSW 2044, Australia, for the Multiview software itself. Each
third-party component below is copyright its respective authors.

> The summaries below describe the **license families** of the load-bearing
> third-party components. They are a human-readable orientation, **not** a complete
> per-file manifest. The authoritative, exhaustive per-dependency listing is
> machine-generated — see [Generating the complete manifest](#generating-the-complete-manifest).

---

## 1. FFmpeg / libav\* — LGPL-2.1 (dynamically linked)

Multiview uses the **FFmpeg / libav\*** libraries (`libavcodec`, `libavformat`,
`libavutil`, `libavfilter`, `libswscale`, `libswresample`, and related) for
container demux/mux and for software/hardware decode and encode. In the default
build these libraries are used **under the GNU Lesser General Public License,
version 2.1 (LGPL-2.1)**, by **dynamic linking**; they are not statically bundled
into the Multiview binary and are not relicensed under Multiview's license.

Nothing in Multiview's license limits the rights the LGPL grants you with respect
to these libraries — including the right to modify the Multiview software for your
own use, to reverse-engineer it for debugging such modifications, and to relink it
against a modified version of the libraries, to the extent the LGPL requires (see
[`LICENSE`](LICENSE) §9).

- **Components:** FFmpeg / libav\* libraries.
- **License:** GNU LGPL-2.1 (or later), as built and distributed.
- **Linkage:** dynamic. The default Multiview build is kept LGPL-clean.
- **Upstream:** https://ffmpeg.org/ — license: https://ffmpeg.org/legal.html

### 1a. Optional `gpl-codecs` build — GPL-2.0-or-later (NOT under Multiview's license)

Multiview has an **optional, off-by-default** `gpl-codecs` build profile that links
GPL-licensed encoders such as **x264** and **x265**. Those GPL codec components are
**not licensed to you under Multiview's license** and are **not distributed by
Aperim Pty Ltd under Multiview's license**. If you choose to build Multiview with
`gpl-codecs`, the resulting **combined work is governed by the GNU GPL** (version 2
or later, as applicable to those components), and Multiview's Source-Available
Non-Commercial License does not apply to that combined work (see [`LICENSE`](LICENSE)
§9). This profile is opt-in only; the default build never links these components.

- **Components:** x264, x265 (and any other GPL-only codec enabled by the profile).
- **License:** GNU GPL-2.0-or-later.
- **Status:** opt-in; excluded from the default build and from Multiview's licence grant.

---

## 2. Bundled fonts — SIL Open Font License 1.1 (OFL)

Multiview embeds a small set of fonts (used for timecode, labels, and the
failure-path cards such as the "SIGNAL LOST" card) so that glyph metrics are
deterministic with no dependency on host-installed fonts. These fonts are **not**
covered by Multiview's license; they are redistributed under the **SIL Open Font
License, Version 1.1 (OFL-1.1)**. The full OFL-1.1 text ships alongside each font
file. See the root [`NOTICE`](NOTICE) file for the canonical attribution.

| Font | License | Copyright | File |
|------|---------|-----------|------|
| JetBrains Mono | OFL-1.1 | Copyright 2020 The JetBrains Mono Project Authors | `crates/multiview-compositor/assets/fonts/JetBrainsMono-Regular.ttf` (OFL: `JetBrainsMono-OFL.txt`) |
| Noto Sans | OFL-1.1 | Copyright 2022 The Noto Project Authors | `crates/multiview-compositor/assets/fonts/NotoSans-Regular.ttf` (OFL: `NotoSans-OFL.txt`) |

- **License:** SIL Open Font License, Version 1.1.
- **Upstreams:** https://github.com/JetBrains/JetBrainsMono ,
  https://github.com/notofonts/latin-greek-cyrillic
- **Reserved-name / acknowledgement requirements** under the OFL apply; the OFL text
  accompanying each font governs.

---

## 3. Rust crate dependencies — predominantly MIT and/or Apache-2.0

Multiview is built on the Rust crate ecosystem. The transitive dependency graph is
**predominantly dual-licensed `MIT OR Apache-2.0`**, with a minority under other
permissive licenses. The license allow-list enforced by `cargo deny` (see
[`deny.toml`](deny.toml)) for these dependencies is:

- **MIT**
- **Apache-2.0** (and **Apache-2.0 WITH LLVM-exception**)
- **BSD-2-Clause**, **BSD-3-Clause**
- **ISC**
- **Zlib**
- **MPL-2.0**
- **Unicode-3.0**, **Unicode-DFS-2016**
- **CC0-1.0**

Each crate remains under its own license(s); the corresponding license text and
copyright notices are carried in the crates' published sources and are reproduced in
the machine-generated manifest (see below). The set of dependencies — and therefore
the exact licenses present — varies with the enabled Cargo features. `cargo deny
check` is a CI gate that fails the build if any dependency resolves to a license
outside the allow-list above, so the default build stays free of copyleft-by-linking
surprises.

> Note: the LGPL applies to the **FFmpeg / libav\* C libraries** (§1), not to the
> Rust **bindings** that wrap them — those bindings are themselves MIT/Apache-2.0.

---

## 4. Optional, runtime-loaded proprietary SDKs — NOT vendored, your own licence required

Two proprietary, professional-AV SDKs are supported **only** through off-by-default,
license-isolating integrations. Neither SDK is **vendored, bundled, statically
linked, or distributed** with Multiview, and neither is required to build or run the
default product. Using these integrations requires **your own licence** for, and
your own copy of, the respective SDK/runtime, supplied by its vendor under that
vendor's terms.

### 4a. NDI® (Vizrt / NewTek) — proprietary, runtime-loaded

The optional `ndi` feature integrates **NDI®** for low-latency IP video in/out. The
NDI runtime library is **resolved at run time** via `NDIlib_v6_load` (dynamic
`dlopen` of the operator-provided runtime); **nothing NDI is linked or vendored at
build time**, so both the default build and the `ndi` build compile and link without
the SDK present. Use of NDI is **gated by, and subject to, the NDI SDK Licence
Agreement and the NDI brand/attribution requirements**, which you accept directly
with the SDK vendor.

- **Vendor:** NDI® is a registered trademark of Vizrt Group (formerly NewTek, Inc.).
- **License:** proprietary NDI SDK Licence Agreement (vendor-supplied; not granted by
  Multiview).
- **Distribution:** never vendored; runtime-loaded only. Mandatory NDI attribution
  applies when the feature is used.

### 4b. Audio over IP — AES67 / SMPTE ST 2110-30 (open standard, no proprietary SDK)

Multiview transports audio over IP using **AES67 / SMPTE ST 2110-30** — open,
royalty-free industry standards (RTP + L16/L24 PCM + SDP + SAP + PTP). This is
included in the public build with **no proprietary SDK**. It is also Audinate's own
licence-free bridge to Dante networks, so it interoperates with Dante devices that
support Dante's AES67 mode. **Native Dante integration is NOT supported** (per
[ADR-T010](docs/decisions/ADR-T010.md)); there is no Audinate SDK in any Multiview
build. References to Dante here are for interoperability and identification only.

> Dante® and Audinate® are registered trademarks of Audinate Pty Ltd; other
> Dante-family product names are trademarks of Audinate Pty Ltd. Multiview is an
> independent project of Aperim Pty Ltd, not affiliated with, endorsed by, or
> sponsored by Audinate; the marks are used nominatively, for identification only.

See [`LICENSE-COMMERCIAL.md`](LICENSE-COMMERCIAL.md) for how the remaining proprietary
components (NDI) relate to a commercial deployment.

---

## 5. Web SPA dependencies — predominantly MIT (npm)

The Multiview web UI (`web/`, a React + TypeScript SPA) depends on npm packages that
are **predominantly MIT**, with a minority under Apache-2.0, BSD, and ISC. As with
the Rust crates, each package remains under its own license; the authoritative
listing is produced by the SPDX/npm tooling described below.

---

## Generating the complete manifest

The summaries above are an orientation, not a substitute for the full,
machine-generated record. A complete, accurate, **per-dependency** notices manifest
(every crate/package, its version, its license(s), and its retained copyright/license
text) is produced by tooling and is the authoritative attribution artifact:

- **Rust crates:**

  ```bash
  cargo about generate about.hbs > THIRD-PARTY-RUST.html
  # or any cargo-about template/format you prefer
  ```

  `cargo about` walks the full resolved dependency graph (per the enabled feature
  set) and emits each crate's license text. Run it once per shipped feature profile
  (e.g. default, `nvidia`, `apple`, `linux-vaapi`, `full`) so the manifest matches
  what is actually distributed.

- **Web SPA (npm):** generate an SPDX SBOM / license report for `web/`, e.g. with an
  SPDX-capable tool (`npm sbom --sbom-format spdx`, or a license-report/SPDX
  generator), and include the per-package license text.

These generated manifests should be **appended to, or published alongside,** this
file as part of CI for each release, so that every distributed build carries a
complete and current third-party attribution record.

---

*This file must be kept intact and passed on with any copy or distribution of the
Multiview software (see [`LICENSE`](LICENSE) §8).*
