//! UMD (Under-Monitor Display) label model with **live-text** updates.
//!
//! A UMD label is the per-tile text strip that sits in the tally
//! [`crate::tally::TallyRegion::Text`] band. Broadcast practice drives this text
//! *live* from a switcher/router/automation over the TSL UMD protocol family
//! (broadcast brief §2) — source name-following, OMD readouts, etc. — so the
//! defining requirement of this model is that the **text can change without a
//! layout reload**: a renderer caches the field layout (positions, alignment)
//! and only re-rasterizes glyphs when the text actually changes.
//!
//! To make that cache-friendly, [`UmdLabel`] carries a monotonically increasing
//! [`revision`](UmdLabel::revision) that bumps **only when visible text
//! changes**. Setting a field to its current value is a no-op and does not bump
//! the revision, so an unchanged label never invalidates the renderer's cached
//! glyph atlas.
//!
//! A label can have one field (the common single UMD) or several
//! ([`UmdLabel::multi`], for multi-field OMD readouts). Field layout (count,
//! per-field alignment) is set at construction and is *not* changed by a
//! [`set_text`](UmdLabel::set_text)/[`set_field_text`](UmdLabel::set_field_text)
//! update — only the glyphs change. Pure model: no rasterizer, no GPU.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Horizontal alignment of a UMD field's text within its band.
///
/// Serialised tagged (`snake_case` variant names); never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UmdAlign {
    /// Left-justified within the field band.
    Left,
    /// Centred within the field band (the conventional UMD default).
    #[default]
    Center,
    /// Right-justified within the field band.
    Right,
}

/// One field of a UMD label: its text and its [`UmdAlign`].
///
/// The text is mutated in place by the label's update methods; the alignment is
/// part of the field *layout* and is set at construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UmdField {
    text: String,
    align: UmdAlign,
}

impl UmdField {
    /// The field's current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The field's alignment (part of the layout, unchanged by text updates).
    #[must_use]
    pub const fn align(&self) -> UmdAlign {
        self.align
    }
}

/// A live-updatable UMD label: one or more [`UmdField`]s plus a [`revision`]
/// counter that bumps only on a visible text change.
///
/// [`revision`]: UmdLabel::revision
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UmdLabel {
    fields: Vec<UmdField>,
    revision: u64,
}

impl UmdLabel {
    /// A single-field label with the given text, centred.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            fields: vec![UmdField {
                text: text.into(),
                align: UmdAlign::Center,
            }],
            revision: 0,
        }
    }

    /// A multi-field label; each item becomes a centred field in order.
    #[must_use]
    pub fn multi<I, S>(texts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let fields = texts
            .into_iter()
            .map(|t| UmdField {
                text: t.into(),
                align: UmdAlign::Center,
            })
            .collect();
        Self {
            fields,
            revision: 0,
        }
    }

    /// Set every field's alignment, builder-style (layout, not a text update —
    /// does not bump the revision).
    #[must_use]
    pub fn with_align(mut self, align: UmdAlign) -> Self {
        for field in &mut self.fields {
            field.align = align;
        }
        self
    }

    /// The label's fields, in order.
    #[must_use]
    pub fn fields(&self) -> &[UmdField] {
        &self.fields
    }

    /// The current revision counter. It increments on each visible text change
    /// and never on a no-op set, so a renderer can key its cached glyph atlas on
    /// it.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The first field's text — the common single-field convenience accessor.
    /// Empty for a label with no fields (which [`UmdLabel::new`]/[`UmdLabel::multi`]
    /// never produce for a non-empty input).
    #[must_use]
    pub fn text(&self) -> &str {
        self.fields.first().map_or("", |f| f.text.as_str())
    }

    /// Set the first field's text (live update). A no-op — including no revision
    /// bump — if the text is unchanged.
    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        if let Some(field) = self.fields.first_mut() {
            if field.text != text {
                field.text = text;
                self.revision = self.revision.saturating_add(1);
            }
        }
    }

    /// Set field `index`'s text (live update). A no-op — including no revision
    /// bump — if the text is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`Error::FieldIndex`] if `index` is out of range.
    pub fn set_field_text(&mut self, index: usize, text: impl Into<String>) -> Result<()> {
        let text = text.into();
        let field = self.fields.get_mut(index).ok_or(Error::FieldIndex(index))?;
        if field.text != text {
            field.text = text;
            self.revision = self.revision.saturating_add(1);
        }
        Ok(())
    }

    /// The first field's text projected to at most `max_glyphs` characters.
    ///
    /// TSL v3.1/v4.0 carry a fixed 16-glyph field; this truncates (does **not**
    /// pad) the displayed text to fit a wire budget. Counts Unicode scalar
    /// values, not bytes.
    #[must_use]
    pub fn displayed(&self, max_glyphs: usize) -> String {
        self.text().chars().take(max_glyphs).collect()
    }
}
