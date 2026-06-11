//! Bundled OFL fonts and the deterministic, system-free [`cosmic_text::FontSystem`]
//! they back (ADR-0016 §7).
//!
//! Two faces are embedded with [`rust_embed`] so labels/clocks render with **no
//! host-font dependency** and identical metrics on every platform:
//!
//! - **`JetBrains` Mono** (`Mono`) — monospaced, for digits/timecode where column
//!   alignment matters.
//! - **Noto Sans** (`Sans`) — proportional, broad-Latin coverage, for labels.
//!
//! Both are licensed under the SIL Open Font License 1.1 (separate from the
//! crate's MIT OR Apache-2.0), attributed in the repo `NOTICE`. The
//! [`FontSystem`] is built from an **empty** `fontdb::Database` plus these two
//! faces and a [`NoSystemFallback`] that lists no platform fonts — so the engine
//! shapes **only** from the bundled faces and never scans the host (deterministic
//! output, the `fontconfig` default feature is disabled in `Cargo.toml`).

use cosmic_text::fontdb::Database;
use cosmic_text::{Fallback, FontSystem};
use rust_embed::RustEmbed;
use unicode_script::Script;

use crate::error::{Error, Result};

/// The bundled OFL font assets, embedded into the binary at build time.
#[derive(RustEmbed)]
#[folder = "assets/fonts/"]
#[include = "*.ttf"]
struct BundledFonts;

/// File name of the bundled monospaced (digit/timecode) face.
const MONO_FILE: &str = "JetBrainsMono-Regular.ttf";
/// File name of the bundled proportional (label) face.
const SANS_FILE: &str = "NotoSans-Regular.ttf";

/// The family name `cosmic-text` resolves for the bundled monospaced face.
///
/// Matched against the face's own `name` table (verified at load time), so a
/// future font swap that changes the family name fails loudly rather than
/// silently falling back.
pub(crate) const MONO_FAMILY: &str = "JetBrains Mono";
/// The family name `cosmic-text` resolves for the bundled proportional face.
pub(crate) const SANS_FAMILY: &str = "Noto Sans";

/// A font fallback that lists **no** platform fonts, so [`cosmic_text`] never
/// reaches for a host face — the engine renders strictly from the bundled OFL
/// faces (deterministic metrics, ADR-0016 §7).
struct NoSystemFallback;

impl Fallback for NoSystemFallback {
    fn common_fallback(&self) -> &[&'static str] {
        &[]
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        &[]
    }

    fn script_fallback(&self, _script: Script, _locale: &str) -> &[&'static str] {
        &[]
    }
}

/// Build the deterministic, system-free [`FontSystem`] from the two bundled OFL
/// faces.
///
/// # Errors
///
/// Returns [`crate::error::Error::FontLoad`] if either embedded asset is missing or its family
/// name does not match the expected [`MONO_FAMILY`] / [`SANS_FAMILY`] (i.e. the
/// bundled bytes were swapped without updating the constants).
pub(crate) fn build_font_system() -> Result<FontSystem> {
    let mut db = Database::new();
    load_face(&mut db, MONO_FILE)?;
    load_face(&mut db, SANS_FILE)?;

    verify_family(&db, MONO_FAMILY)?;
    verify_family(&db, SANS_FAMILY)?;

    // A fixed locale keeps shaping deterministic across hosts; the empty
    // fallback guarantees no system-font scan.
    Ok(FontSystem::new_with_locale_and_db_and_fallback(
        "en-US".to_owned(),
        db,
        NoSystemFallback,
    ))
}

/// Load one embedded face into the database.
fn load_face(db: &mut Database, file: &str) -> Result<()> {
    let asset = BundledFonts::get(file)
        .ok_or_else(|| Error::FontLoad(format!("bundled font asset missing: {file}")))?;
    db.load_font_data(asset.data.into_owned());
    Ok(())
}

/// Confirm the database actually exposes a face under `family` (so a font swap
/// that renames the family is caught at startup, not at first shape).
fn verify_family(db: &Database, family: &str) -> Result<()> {
    let present = db
        .faces()
        .any(|face| face.families.iter().any(|(name, _lang)| name == family));
    if present {
        Ok(())
    } else {
        Err(Error::FontLoad(format!(
            "bundled font family not found after load: {family}"
        )))
    }
}
