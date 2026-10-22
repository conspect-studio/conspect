//! Release-artifact feature guard (task #109 — PR #7 cross-vendor panel
//! follow-up B(2)).
//!
//! A test-only Cargo feature — `multiview-control`'s `_test-seams`, which arms
//! the config-watch post-probe interpose hook that WRITES the watched file —
//! must NEVER be compiled into a shipped/release artifact. It exists solely for
//! `multiview-control`'s own integration tests (enabled there via a self
//! dev-dependency) and is inert in production, but it is an ordinary Cargo
//! feature that an explicit `--features _test-seams` (or a workspace
//! `--all-features` build) can still turn on. `cargo deny` cannot gate a
//! feature, and there is no reliable "release" `cfg`, so this guard closes the
//! gap: it resolves the effective feature set of every shipped release preset
//! and fails if any internal/test-only seam feature is enabled.
//!
//! Resolution uses `cargo tree` — dependency/feature RESOLUTION only, never a
//! compile. `-e no-dev` excludes dev-dependency edges so the resolved set
//! matches a non-test `cargo build --features <spec>` (a transitive
//! dependency's own dev-dependencies never enter a release graph, which is
//! precisely why `_test-seams` stays out).
//!
//! The guard flags any feature in the `_test-seams` family — a name containing
//! `test-seam` (case-insensitive) — on any crate. It deliberately does NOT flag
//! every leading-underscore feature: third-party crates carry unrelated
//! internal `_`/`__`-prefixed features (`reqwest/__rustls`,
//! `dimpl/_crypto-common`) that legitimately resolve in a release build.
//! `_test-seams` is the only such feature in this workspace today, so the rule
//! is exactly "no shipped build enables `_test-seams`", while staying
//! future-proof for any new `*test-seam*` seam.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// The feature specs the guard resolves: the four umbrella presets
/// (`crates/multiview-cli/Cargo.toml`) plus the EXACT `--features` strings every
/// shipping source builds. The `release_features_drift` test derives the
/// concrete strings from [`SHIPPING_SOURCES`] and asserts this list covers every
/// one (order-insensitive), so it cannot silently fall behind what ships.
///
/// * `.github/workflows/release.yml` binaries → `ffmpeg,linux-vaapi`, `ffmpeg,apple`
/// * `.github/workflows/docker.yml` GHCR images → `ffmpeg,linux-vaapi,web`,
///   `ffmpeg,nvidia,web`
/// * `deploy/Dockerfile{,.nvidia}` `CARGO_FEATURES` defaults →
///   `ffmpeg,linux-vaapi,web,ntp`, `ffmpeg,nvidia,web`; the documented GPL image
///   adds `gpl-codecs`.
pub const RELEASE_FEATURE_SPECS: &[&str] = &[
    // Umbrella presets (crates/multiview-cli/Cargo.toml `[features]`).
    "nvidia",
    "apple",
    "linux-vaapi",
    "full",
    // Exact strings the shipped release binaries build with (release.yml).
    "ffmpeg,linux-vaapi",
    "ffmpeg,apple",
    // Exact strings the shipped GHCR images build with (docker.yml → the
    // CARGO_FEATURES build-arg); `ffmpeg,nvidia,web` also = Dockerfile.nvidia's
    // ARG default.
    "ffmpeg,linux-vaapi,web",
    "ffmpeg,nvidia,web",
    // deploy/Dockerfile CARGO_FEATURES default + the documented GPL image variant.
    "ffmpeg,linux-vaapi,web,ntp",
    "ffmpeg,linux-vaapi,web,ntp,gpl-codecs",
];

/// How a [`ShippingSource`] file names its shipped `--features` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShippingSourceKind {
    /// A GitHub Actions workflow — the build matrix's `features:` scalars.
    GithubWorkflow,
    /// A Dockerfile — the `ARG CARGO_FEATURES=` default(s).
    Dockerfile,
}

/// A canonical repository source that pins a shipped artifact's cargo
/// `--features` string. The drift guard parses each so `RELEASE_FEATURE_SPECS`
/// cannot silently fall behind what actually ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShippingSource {
    /// Repo-root-relative path to the source file.
    pub path: &'static str,
    /// How to extract the `--features` strings from it.
    pub kind: ShippingSourceKind,
}

/// Every canonical source that defines a shipped artifact's feature set. The
/// drift guard (see the `release_features_drift` test) parses each and asserts
/// `RELEASE_FEATURE_SPECS` covers every resulting combo, so adding a shipped
/// preset/combo to these files without listing it here fails CI.
pub const SHIPPING_SOURCES: &[ShippingSource] = &[
    // Release binaries — release.yml build matrix (ffmpeg,linux-vaapi / ffmpeg,apple).
    ShippingSource {
        path: ".github/workflows/release.yml",
        kind: ShippingSourceKind::GithubWorkflow,
    },
    // GHCR container images — docker.yml build matrix, threaded into the
    // CARGO_FEATURES build-arg (ffmpeg,linux-vaapi,web / ffmpeg,nvidia,web).
    ShippingSource {
        path: ".github/workflows/docker.yml",
        kind: ShippingSourceKind::GithubWorkflow,
    },
    // Dockerfile CARGO_FEATURES defaults (used for a manual `docker build`
    // without the docker.yml build-arg override).
    ShippingSource {
        path: "deploy/Dockerfile",
        kind: ShippingSourceKind::Dockerfile,
    },
    ShippingSource {
        path: "deploy/Dockerfile.nvidia",
        kind: ShippingSourceKind::Dockerfile,
    },
];

/// Extract the shipped `--features` strings declared in one source file's text,
/// dispatching on its [`ShippingSourceKind`].
#[must_use]
pub fn extract_shipped_specs(kind: ShippingSourceKind, text: &str) -> Vec<String> {
    match kind {
        ShippingSourceKind::GithubWorkflow => feature_specs_in_workflow(text),
        ShippingSourceKind::Dockerfile => cargo_features_in_dockerfile(text),
    }
}

/// Extract the build-matrix `features:` scalars from a GitHub Actions workflow
/// (e.g. `features: "ffmpeg,nvidia,web"`). Surrounding quotes are stripped;
/// templated values (`${{ … }}`) and non-`features:` lines (e.g. the
/// `--features "${{ matrix.features }}"` build step) are skipped.
#[must_use]
pub fn feature_specs_in_workflow(workflow_yaml: &str) -> Vec<String> {
    workflow_yaml
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("features:")?;
            let spec = rest.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if spec.is_empty() || spec.contains("${{") {
                None
            } else {
                Some(spec.to_owned())
            }
        })
        .collect()
}

/// Extract the `ARG CARGO_FEATURES=<value>` default(s) from a Dockerfile's text.
/// A `--build-arg CARGO_FEATURES=…` in a comment is not an `ARG` line, so only
/// the real default is captured; templated values are skipped.
#[must_use]
pub fn cargo_features_in_dockerfile(dockerfile: &str) -> Vec<String> {
    dockerfile
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("ARG CARGO_FEATURES=")?;
            let spec = rest.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if spec.is_empty() || spec.contains("${") {
                None
            } else {
                Some(spec.to_owned())
            }
        })
        .collect()
}

/// The set of features a `--features` spec names (comma-split, trimmed, deduped),
/// so coverage comparison is order- and duplicate-insensitive.
fn feature_set(spec: &str) -> BTreeSet<String> {
    spec.split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Every `derived` shipped spec whose feature SET equals no `covered` entry's
/// (i.e. is not checked by the guard), deduplicated in input order. Empty ⇒
/// every shipped combo is covered. Order-insensitive: `ffmpeg,linux-vaapi` ≡
/// `linux-vaapi,ffmpeg`.
#[must_use]
pub fn uncovered_specs(derived: &[String], covered: &[&str]) -> Vec<String> {
    let covered_sets: Vec<BTreeSet<String>> = covered.iter().map(|s| feature_set(s)).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut uncovered = Vec::new();
    for spec in derived {
        if !covered_sets.contains(&feature_set(spec)) && seen.insert(spec.clone()) {
            uncovered.push(spec.clone());
        }
    }
    uncovered
}

/// Does `feature` name a test-only Cargo seam (the `_test-seams` family) that
/// must never be enabled in a shipped release build?
///
/// Matches any feature whose name contains `test-seam` (case-insensitive) — e.g.
/// `multiview-control`'s `_test-seams` (task #109). The match is scoped to the
/// test-seam name rather than the leading underscore because unrelated
/// third-party crates carry internal `_`/`__`-prefixed features
/// (`reqwest/__rustls`, `dimpl/_crypto-common`) that legitimately resolve in a
/// release build.
#[must_use]
pub fn is_test_seam_feature(feature: &str) -> bool {
    feature.to_ascii_lowercase().contains("test-seam")
}

/// A single `(crate, feature)` activation flagged by the guard: an
/// internal/test-only seam feature that a release preset resolved.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeamActivation {
    /// The crate whose feature was activated (e.g. `multiview-control`).
    pub krate: String,
    /// The offending feature name (e.g. `_test-seams`).
    pub feature: String,
}

/// Scan `cargo tree --format "{p}|{f}"` output for internal/test-only seam
/// features. Returns every offending `(crate, feature)` activation, deduplicated
/// and sorted. Pure: it parses text and performs no I/O.
///
/// Each input line is `<name> v<version> [(<path>)]|<f1,f2,...>` with an
/// optional trailing ` (*)` dedupe marker cargo appends to repeated nodes.
#[must_use]
pub fn find_seam_activations(tree_output: &str) -> Vec<SeamActivation> {
    let mut found: BTreeSet<SeamActivation> = BTreeSet::new();
    for line in tree_output.lines() {
        // Split package (`{p}`) from its feature list (`{f}`). Features never
        // contain `|`, so the last `|` separates them even if a path contained
        // one.
        let Some((pkg, feats)) = line.rsplit_once('|') else {
            continue;
        };
        let Some(krate) = pkg.split_whitespace().next() else {
            continue;
        };
        // Drop the trailing ` (*)` dedupe marker cargo appends to repeated nodes.
        let feats = feats.strip_suffix(" (*)").unwrap_or(feats);
        for feature in feats.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            if is_test_seam_feature(feature) {
                found.insert(SeamActivation {
                    krate: krate.to_owned(),
                    feature: feature.to_owned(),
                });
            }
        }
    }
    found.into_iter().collect()
}

/// The guard result for one release feature spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetOutcome {
    /// The `--features` spec that was resolved (e.g. `full`).
    pub spec: String,
    /// Seam activations found in this spec's resolved feature set (empty = OK).
    pub violations: Vec<SeamActivation>,
}

/// The full guard report across every checked release feature spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// Per-spec outcomes, in the order the specs were checked.
    pub outcomes: Vec<PresetOutcome>,
}

impl CheckReport {
    /// Did any checked spec resolve a forbidden internal/test-only seam feature?
    #[must_use]
    pub fn has_violations(&self) -> bool {
        self.outcomes.iter().any(|o| !o.violations.is_empty())
    }

    /// Render a human-readable report: one line per checked spec, each offending
    /// activation, and a final PASS/FAIL verdict.
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for outcome in &self.outcomes {
            if outcome.violations.is_empty() {
                lines.push(format!(
                    "  OK    {} — no internal/test-only seam feature",
                    outcome.spec
                ));
            } else {
                lines.push(format!(
                    "  FAIL  {} — resolves forbidden seam feature(s):",
                    outcome.spec
                ));
                for v in &outcome.violations {
                    lines.push(format!("          {}/{}", v.krate, v.feature));
                }
            }
        }
        lines.push(String::new());
        if self.has_violations() {
            lines.push(
                "release-feature guard FAILED: a shipped preset enables a test-only seam feature."
                    .to_owned(),
            );
            lines.push(
                "A `_test-seams`-family feature must never reach a release artifact (task #109)."
                    .to_owned(),
            );
        } else {
            lines.push(
                "release-feature guard OK: no shipped preset enables a test-only seam feature."
                    .to_owned(),
            );
        }
        lines.join("\n")
    }
}

/// Resolve each `spec` via the injected `resolve` (which returns the `cargo
/// tree` output for a spec) and collect a [`CheckReport`]. Pure over the
/// resolver, so the orchestration is unit-testable without invoking cargo.
///
/// # Errors
/// Propagates the first `resolve` error (e.g. a failed `cargo tree`).
pub fn report_from_resolver<F, E>(specs: &[&str], mut resolve: F) -> Result<CheckReport, E>
where
    F: FnMut(&str) -> Result<String, E>,
{
    let mut outcomes = Vec::with_capacity(specs.len());
    for spec in specs {
        let tree = resolve(spec)?;
        outcomes.push(PresetOutcome {
            spec: (*spec).to_owned(),
            violations: find_seam_activations(&tree),
        });
    }
    Ok(CheckReport { outcomes })
}

/// Errors from resolving a release preset's feature set with `cargo tree`.
#[derive(Debug, Error)]
pub enum ReleaseFeatureError {
    /// Spawning/awaiting the `cargo tree` process failed.
    #[error("running `cargo tree` for features `{spec}`: {source}")]
    Spawn {
        /// The `--features` spec being resolved.
        spec: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// `cargo tree` exited non-zero (e.g. an unknown feature / unresolved graph).
    #[error("`cargo tree` failed for features `{spec}` (exit {code}):\n{stderr}")]
    Resolve {
        /// The `--features` spec being resolved.
        spec: String,
        /// The process exit code (or `-1` if terminated by a signal).
        code: i32,
        /// Captured stderr from cargo.
        stderr: String,
    },
}

/// Resolve one release feature spec's effective feature set as `cargo tree`
/// `{p}|{f}` lines. RESOLUTION ONLY — `cargo tree` never compiles the crate.
///
/// `-e no-dev` excludes dev-dependency edges so the resolution matches a
/// non-test `cargo build --features <spec>`; `--locked` pins the committed
/// `Cargo.lock` (rule 33).
///
/// # Errors
/// [`ReleaseFeatureError::Spawn`] if cargo cannot be launched;
/// [`ReleaseFeatureError::Resolve`] if `cargo tree` exits non-zero.
fn cargo_tree_features(spec: &str) -> Result<String, ReleaseFeatureError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .args([
            "tree",
            "--locked",
            "-p",
            "multiview-cli",
            "--features",
            spec,
            "-e",
            "no-dev",
            "--prefix",
            "none",
            "--format",
            "{p}|{f}",
        ])
        .current_dir(workspace_root())
        .output()
        .map_err(|source| ReleaseFeatureError::Spawn {
            spec: spec.to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(ReleaseFeatureError::Resolve {
            spec: spec.to_owned(),
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The workspace root, derived from this crate's manifest dir (`xtask/`) so the
/// guard resolves the workspace regardless of the caller's working directory.
fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<root>/xtask`; the workspace root is its parent.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Run the release-artifact feature guard across every [`RELEASE_FEATURE_SPECS`]
/// entry, resolving each with `cargo tree`. Returns the [`CheckReport`]; the
/// caller inspects [`CheckReport::has_violations`] for the exit status.
///
/// # Errors
/// Propagates the first [`ReleaseFeatureError`] from resolving a spec.
pub fn check_release_features() -> Result<CheckReport, ReleaseFeatureError> {
    report_from_resolver(RELEASE_FEATURE_SPECS, cargo_tree_features)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seam_features_are_flagged() {
        assert!(is_test_seam_feature("_test-seams"));
        assert!(is_test_seam_feature("_test-seam"));
        assert!(is_test_seam_feature("TEST-SEAMS"));
    }

    #[test]
    fn ordinary_and_third_party_internal_features_are_not_seams() {
        for feature in [
            "openapi",
            "default",
            "full",
            "linux-vaapi",
            "",
            // Real third-party internal `_`/`__` features that legitimately
            // resolve in a release build — must NOT be flagged (a leading
            // underscore alone is not a test seam; found by running the guard
            // against the real presets, rule 26).
            "__rustls",
            "__rustls-ring",
            "__tls",
            "_crypto-common",
        ] {
            assert!(
                !is_test_seam_feature(feature),
                "`{feature}` must not be flagged as a test seam"
            );
        }
    }

    #[test]
    fn find_seam_activations_flags_a_reachable_test_seam() {
        // A realistic `cargo tree -f "{p}|{f}"` slice where multiview-control has
        // resolved `_test-seams` alongside its ordinary features.
        let tree = "\
multiview-cli v0.1.0 (/w/crates/multiview-cli)|default,ffmpeg,full,software
multiview-control v0.1.0 (/w/crates/multiview-control)|_test-seams,cast,default,openapi
multiview-core v0.1.0 (/w/crates/multiview-core)|
serde v1.0.0|default,derive (*)";
        let found = find_seam_activations(tree);
        assert_eq!(
            found,
            vec![SeamActivation {
                krate: "multiview-control".to_owned(),
                feature: "_test-seams".to_owned(),
            }],
            "the reachable `_test-seams` activation must be reported"
        );
    }

    #[test]
    fn find_seam_activations_ignores_a_clean_graph() {
        let tree = "\
multiview-cli v0.1.0 (/w/crates/multiview-cli)|default,ffmpeg,full,software
multiview-control v0.1.0 (/w/crates/multiview-control)|cast,default,devices-net,openapi
arc-swap v1.9.2|
hybrid-array v0.4.12| (*)";
        assert!(find_seam_activations(tree).is_empty());
    }

    #[test]
    fn find_seam_activations_strips_dedupe_marker_and_empty_features() {
        // The seam is on a node carrying the trailing ` (*)` dedupe marker; the
        // other lines exercise the empty-feature and empty-deduped shapes.
        let tree = "\
arc-swap v1.9.2|
multiview-core v0.1.0 (/w/crates/multiview-core)|
hybrid-array v0.4.12| (*)
gadget v0.1.0 (/w/crates/gadget)|_test-seams,default (*)";
        let found = find_seam_activations(tree);
        assert_eq!(
            found,
            vec![SeamActivation {
                krate: "gadget".to_owned(),
                feature: "_test-seams".to_owned(),
            }]
        );
    }

    #[test]
    fn find_seam_activations_dedupes_and_sorts_multiple_crates() {
        let tree = "\
beta v0.1.0 (/w/beta)|_test-seams,default
alpha v0.1.0 (/w/alpha)|_test-seams
beta v0.1.0 (/w/beta)|_test-seams,default (*)";
        let found = find_seam_activations(tree);
        assert_eq!(
            found,
            vec![
                SeamActivation {
                    krate: "alpha".to_owned(),
                    feature: "_test-seams".to_owned(),
                },
                SeamActivation {
                    krate: "beta".to_owned(),
                    feature: "_test-seams".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn report_from_resolver_passes_a_clean_set() {
        let specs = ["nvidia", "full"];
        let report = report_from_resolver(&specs, |spec| {
            Ok::<_, ReleaseFeatureError>(format!(
                "multiview-control v0.1.0 (/w)|default,openapi\nmultiview-cli v0.1.0 (/w)|{spec}"
            ))
        })
        .expect("clean resolver never errors");
        assert!(!report.has_violations());
        assert_eq!(report.outcomes.len(), 2);
        assert_eq!(report.outcomes[0].spec, "nvidia");
    }

    #[test]
    fn report_from_resolver_flags_a_preset_that_reaches_the_seam() {
        // The negative-fixture case as a pure test: one preset's resolved graph
        // reaches `multiview-control/_test-seams`; the guard must flag exactly it.
        let specs = ["nvidia", "full"];
        let report = report_from_resolver(&specs, |spec| {
            let feats = if spec == "full" {
                "_test-seams,default,openapi"
            } else {
                "default,openapi"
            };
            Ok::<_, ReleaseFeatureError>(format!(
                "multiview-control v0.1.0 (/w/crates/multiview-control)|{feats}"
            ))
        })
        .expect("resolver never errors");
        assert!(
            report.has_violations(),
            "a preset reaching `_test-seams` must make the guard report a violation"
        );
        let full = report
            .outcomes
            .iter()
            .find(|o| o.spec == "full")
            .expect("full outcome present");
        assert_eq!(
            full.violations,
            vec![SeamActivation {
                krate: "multiview-control".to_owned(),
                feature: "_test-seams".to_owned(),
            }]
        );
        let nvidia = report
            .outcomes
            .iter()
            .find(|o| o.spec == "nvidia")
            .expect("nvidia outcome present");
        assert!(nvidia.violations.is_empty());
    }

    #[test]
    fn report_from_resolver_propagates_a_resolver_error() {
        let specs = ["nvidia"];
        let result = report_from_resolver(&specs, |spec| {
            Err::<String, _>(ReleaseFeatureError::Resolve {
                spec: spec.to_owned(),
                code: 101,
                stderr: "boom".to_owned(),
            })
        });
        assert!(result.is_err());
    }

    #[test]
    fn render_reports_ok_for_a_clean_report() {
        let report = CheckReport {
            outcomes: vec![PresetOutcome {
                spec: "full".to_owned(),
                violations: Vec::new(),
            }],
        };
        let rendered = report.render();
        assert!(rendered.contains("OK    full"));
        assert!(rendered.contains("release-feature guard OK"));
        assert!(!report.has_violations());
    }

    #[test]
    fn render_reports_fail_and_names_the_seam() {
        let report = CheckReport {
            outcomes: vec![PresetOutcome {
                spec: "full".to_owned(),
                violations: vec![SeamActivation {
                    krate: "multiview-control".to_owned(),
                    feature: "_test-seams".to_owned(),
                }],
            }],
        };
        let rendered = report.render();
        assert!(rendered.contains("FAIL  full"));
        assert!(rendered.contains("multiview-control/_test-seams"));
        assert!(rendered.contains("release-feature guard FAILED"));
        assert!(report.has_violations());
    }

    #[test]
    fn release_feature_specs_cover_the_umbrella_presets() {
        for preset in ["nvidia", "apple", "linux-vaapi", "full"] {
            assert!(
                RELEASE_FEATURE_SPECS.contains(&preset),
                "release specs must check the `{preset}` umbrella preset"
            );
        }
    }

    #[test]
    fn feature_specs_in_workflow_extracts_matrix_features() {
        // `features:` is its own matrix line (the `-` is on the entry's first
        // line above). release.yml leaves it unquoted; docker.yml quotes it; the
        // build step's `--features "${{ matrix.features }}"` must NOT be picked up.
        let yaml = r#"
        include:
          - target: x86_64
            features: ffmpeg,linux-vaapi
          - variant: nvidia
            features: "ffmpeg,nvidia,web"
      run: cargo build --features "${{ matrix.features }}"
            features: "${{ matrix.templated }}"
"#;
        assert_eq!(
            feature_specs_in_workflow(yaml),
            vec![
                "ffmpeg,linux-vaapi".to_owned(),
                "ffmpeg,nvidia,web".to_owned()
            ]
        );
    }

    #[test]
    fn cargo_features_in_dockerfile_extracts_arg_default_only() {
        let dockerfile = r#"
        # example: --build-arg CARGO_FEATURES=ffmpeg,linux-vaapi,web,ntp,gpl-codecs
        ARG CARGO_FEATURES=ffmpeg,linux-vaapi,web,ntp
        RUN cargo build --features "${CARGO_FEATURES}"
"#;
        assert_eq!(
            cargo_features_in_dockerfile(dockerfile),
            vec!["ffmpeg,linux-vaapi,web,ntp".to_owned()]
        );
    }

    #[test]
    fn extract_shipped_specs_dispatches_on_kind() {
        assert_eq!(
            extract_shipped_specs(ShippingSourceKind::GithubWorkflow, "features: ffmpeg,apple"),
            vec!["ffmpeg,apple".to_owned()]
        );
        assert_eq!(
            extract_shipped_specs(
                ShippingSourceKind::Dockerfile,
                "ARG CARGO_FEATURES=ffmpeg,nvidia,web"
            ),
            vec!["ffmpeg,nvidia,web".to_owned()]
        );
    }

    #[test]
    fn uncovered_specs_is_empty_when_every_combo_is_covered_order_insensitively() {
        // `linux-vaapi,ffmpeg` (reordered) is covered by `ffmpeg,linux-vaapi`.
        let derived = vec!["linux-vaapi,ffmpeg".to_owned(), "ffmpeg,apple".to_owned()];
        let covered = ["ffmpeg,linux-vaapi", "ffmpeg,apple", "full"];
        assert!(uncovered_specs(&derived, &covered).is_empty());
    }

    #[test]
    fn uncovered_specs_flags_a_missing_shipped_combo() {
        let derived = vec![
            "ffmpeg,linux-vaapi".to_owned(),
            "ffmpeg,nvidia,web".to_owned(),
        ];
        // `ffmpeg,nvidia,web` is NOT covered by any entry.
        let covered = ["ffmpeg,linux-vaapi", "full"];
        assert_eq!(
            uncovered_specs(&derived, &covered),
            vec!["ffmpeg,nvidia,web".to_owned()]
        );
    }

    #[test]
    fn uncovered_specs_dedupes_repeated_missing_combos() {
        let derived = vec![
            "ffmpeg,nvidia,web".to_owned(),
            "ffmpeg,nvidia,web".to_owned(),
        ];
        let covered = ["full"];
        assert_eq!(
            uncovered_specs(&derived, &covered),
            vec!["ffmpeg,nvidia,web".to_owned()]
        );
    }

    #[test]
    fn shipping_sources_lists_the_canonical_files() {
        let paths: Vec<&str> = SHIPPING_SOURCES.iter().map(|s| s.path).collect();
        for expected in [
            ".github/workflows/release.yml",
            ".github/workflows/docker.yml",
            "deploy/Dockerfile",
            "deploy/Dockerfile.nvidia",
        ] {
            assert!(
                paths.contains(&expected),
                "SHIPPING_SOURCES must parse `{expected}`"
            );
        }
    }
}
