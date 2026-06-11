//! A persistent, byte-capped, LRU-evicting glyph atlas (ADR-0016 §4.3, T2/T4).
//!
//! Each **unique** rasterized glyph — keyed on its [`cosmic_text::CacheKey`]
//! (font, glyph id, size, subpixel bin) — is shelf-packed **once** into a single
//! premultiplied-RGBA texture-space buffer via [`etagere`]. Re-rendering an
//! unchanged or glyph-overlapping string finds every glyph already resident and
//! inserts nothing new (the mpv #7615 lesson). The atlas is **bounded**: it
//! never exceeds [`GlyphAtlas::byte_cap`]; when an insert would overflow, the
//! least-recently-used glyphs are evicted until it fits (or the glyph is
//! rejected if it cannot fit even in an empty atlas).
//!
//! Stage 1 stores **CPU-side** atlas geometry (the shelf rectangle per glyph) so
//! the GPU sub-pass (a later stage) can map each resident glyph to a sub-texture
//! region and upload only the dirty region. No GPU resources are touched here.

use std::collections::HashMap;

use cosmic_text::CacheKey;
use etagere::{size2, AllocId, AtlasAllocator};

use crate::error::{Error, Result};

/// Bytes per atlas texel: premultiplied RGBA8 (ADR-0016 §4.3).
pub const BYTES_PER_TEXEL: usize = 4;

/// One glyph's resident location in the atlas: its packed rectangle (texture
/// space, top-left origin) and size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasSlot {
    /// X of the packed rectangle's top-left, in atlas texels.
    pub x: u32,
    /// Y of the packed rectangle's top-left, in atlas texels.
    pub y: u32,
    /// Glyph coverage box width, in texels.
    pub width: u32,
    /// Glyph coverage box height, in texels.
    pub height: u32,
}

impl AtlasSlot {
    /// Premultiplied-RGBA byte footprint of this slot's glyph (`w * h * 4`).
    #[must_use]
    pub fn byte_size(self) -> usize {
        texel_bytes(self.width, self.height)
    }
}

/// Per-glyph atlas bookkeeping: where it sits, its allocator handle, its byte
/// footprint, and its place in the LRU order.
#[derive(Debug, Clone, Copy)]
struct Entry {
    slot: AtlasSlot,
    /// The shelf allocation backing this glyph, or `None` for a zero-area glyph
    /// (space/control) that occupies no atlas space and must never be passed to
    /// [`AtlasAllocator::deallocate`].
    alloc: Option<AllocId>,
    bytes: usize,
    /// Monotonic "last touched" tick for LRU eviction (higher = more recent).
    last_used: u64,
}

/// A bounded, shelf-packed, LRU-evicting glyph atlas.
///
/// The atlas owns the [`etagere`] shelf allocator and a `CacheKey → Entry` map.
/// It is bounded by [`GlyphAtlas::byte_cap`]: [`GlyphAtlas::used_bytes`] never
/// exceeds it.
pub struct GlyphAtlas {
    allocator: AtlasAllocator,
    entries: HashMap<CacheKey, Entry>,
    /// Square texture side length, in texels (the etagere allocation space).
    side: u32,
    used_bytes: usize,
    byte_cap: usize,
    /// Monotonic clock for LRU; bumped on every touch (insert or hit).
    clock: u64,
}

impl GlyphAtlas {
    /// Default square atlas side (texels). 1024 keeps the empty allocator small
    /// while comfortably holding many label/clock glyphs at typical sizes.
    pub const DEFAULT_SIDE: u32 = 1024;

    /// Build an atlas of `side × side` texels bounded to `byte_cap` premultiplied
    /// RGBA bytes.
    ///
    /// `byte_cap` is the hard ceiling on resident glyph bytes (T4); it is
    /// independent of `side`, which only bounds packing geometry. A `side` of 0
    /// is clamped to 1 so the allocator is always valid.
    #[must_use]
    pub fn new(side: u32, byte_cap: usize) -> Self {
        let side = side.max(1);
        let isize_side = i32::try_from(side).unwrap_or(i32::MAX);
        Self {
            allocator: AtlasAllocator::new(size2(isize_side, isize_side)),
            entries: HashMap::new(),
            side,
            used_bytes: 0,
            byte_cap,
            clock: 0,
        }
    }

    /// The atlas's hard byte cap (T4).
    #[must_use]
    pub const fn byte_cap(&self) -> usize {
        self.byte_cap
    }

    /// Square side length of the packing space, in texels.
    #[must_use]
    pub const fn side(&self) -> u32 {
        self.side
    }

    /// Number of glyphs currently resident.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Total premultiplied-RGBA bytes currently held by resident glyphs.
    #[must_use]
    pub const fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// If `key` is resident, mark it most-recently-used and return its slot;
    /// otherwise `None`. A cache **hit** uploads nothing (T2).
    pub fn touch(&mut self, key: CacheKey) -> Option<AtlasSlot> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = clock;
        Some(entry.slot)
    }

    /// Whether `key` is currently resident (without touching LRU order).
    #[must_use]
    pub fn contains(&self, key: CacheKey) -> bool {
        self.entries.contains_key(&key)
    }

    /// Insert a newly-rasterized glyph of `width × height` texels under `key`,
    /// evicting least-recently-used glyphs as needed to honor the byte cap.
    ///
    /// Returns the glyph's [`AtlasSlot`]. A zero-area glyph (e.g. a space) is
    /// stored as an empty slot that occupies no atlas space.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::AtlasGlyphTooLarge`] if the glyph cannot fit even in an
    /// otherwise-empty atlas (its byte size exceeds the cap, or its dimensions
    /// exceed the packing side) — the caller should skip the glyph/layer rather
    /// than crash (hot-path safety rule #3).
    pub fn insert(&mut self, key: CacheKey, width: u32, height: u32) -> Result<AtlasSlot> {
        if let Some(slot) = self.touch(key) {
            return Ok(slot);
        }

        // Zero-area glyphs (space, control) occupy no atlas space; record an
        // empty resident slot so they count as "cached" (no re-raster) yet add
        // zero bytes.
        if width == 0 || height == 0 {
            return Ok(self.record_empty(key));
        }

        let bytes = texel_bytes(width, height);
        if bytes > self.byte_cap || width > self.side || height > self.side {
            return Err(Error::AtlasGlyphTooLarge(format!(
                "glyph {width}x{height} ({bytes} B) exceeds atlas cap {} B / side {}",
                self.byte_cap, self.side
            )));
        }

        // Make byte-room first (LRU), then allocate shelf space; if the shelf is
        // fragmented, evicting also frees allocator space, so retry allocation
        // after each eviction.
        self.evict_until_fits(bytes);
        let alloc = self.allocate_with_eviction(width, height)?;

        self.clock = self.clock.wrapping_add(1);
        let slot = slot_from_alloc(&alloc, width, height)?;
        self.used_bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                slot,
                alloc: Some(alloc.id),
                bytes,
                last_used: self.clock,
            },
        );
        Ok(slot)
    }

    /// Record a zero-area glyph as resident with an empty slot (no bytes, no
    /// allocator space).
    fn record_empty(&mut self, key: CacheKey) -> AtlasSlot {
        self.clock = self.clock.wrapping_add(1);
        let slot = AtlasSlot {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        self.entries.insert(
            key,
            Entry {
                slot,
                alloc: None,
                bytes: 0,
                last_used: self.clock,
            },
        );
        slot
    }

    /// Evict least-recently-used glyphs until at least `need` bytes of headroom
    /// exist under the cap (or the atlas is empty).
    fn evict_until_fits(&mut self, need: usize) {
        while self.used_bytes + need > self.byte_cap {
            if !self.evict_one() {
                break;
            }
        }
    }

    /// Try to shelf-allocate `width × height`; if the allocator is full, evict
    /// the LRU glyph and retry until it fits or nothing is left to evict.
    fn allocate_with_eviction(&mut self, width: u32, height: u32) -> Result<etagere::Allocation> {
        let w = i32::try_from(width).unwrap_or(i32::MAX);
        let h = i32::try_from(height).unwrap_or(i32::MAX);
        loop {
            if let Some(alloc) = self.allocator.allocate(size2(w, h)) {
                return Ok(alloc);
            }
            if !self.evict_one() {
                return Err(Error::AtlasGlyphTooLarge(format!(
                    "glyph {width}x{height} does not fit the {} texel atlas even when empty",
                    self.side
                )));
            }
        }
    }

    /// Evict the single least-recently-used glyph. Returns `false` if there is
    /// nothing to evict.
    fn evict_one(&mut self) -> bool {
        let Some((&victim, entry)) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, entry)| (key, *entry))
        else {
            return false;
        };
        if let Some(alloc) = entry.alloc {
            self.allocator.deallocate(alloc);
        }
        self.used_bytes = self.used_bytes.saturating_sub(entry.bytes);
        self.entries.remove(&victim);
        true
    }
}

/// Premultiplied-RGBA byte footprint of a `width × height` texel region, using
/// saturating widening conversions (no lossy `as` casts; `u32 -> usize` is
/// widening on every Multiview target).
fn texel_bytes(width: u32, height: u32) -> usize {
    let w = usize::try_from(width).unwrap_or(usize::MAX);
    let h = usize::try_from(height).unwrap_or(usize::MAX);
    w.saturating_mul(h).saturating_mul(BYTES_PER_TEXEL)
}

/// Convert an [`etagere`] allocation rectangle (which may be padded larger than
/// requested) into a glyph slot using the glyph's true pixel size at the
/// rectangle's top-left.
fn slot_from_alloc(alloc: &etagere::Allocation, width: u32, height: u32) -> Result<AtlasSlot> {
    let min = alloc.rectangle.min;
    let x = u32::try_from(min.x)
        .map_err(|_| Error::AtlasGlyphTooLarge(format!("negative atlas x {}", min.x)))?;
    let y = u32::try_from(min.y)
        .map_err(|_| Error::AtlasGlyphTooLarge(format!("negative atlas y {}", min.y)))?;
    Ok(AtlasSlot {
        x,
        y,
        width,
        height,
    })
}
