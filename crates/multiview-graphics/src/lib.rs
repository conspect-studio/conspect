//! Graphics (CG) runtime: the playout state machine and the process-wide hub
//! shared by the control plane (which mutates it) and the graphics source
//! generators (which read it without blocking).
//!
//! * [`GraphicsHub`] holds the effective [`GraphicsConfig`] (items, data
//!   sources, channels + rundowns) and each channel's live PVW/PGM state.
//! * Mutations (cue / take / clear / next, config replacement) run on the
//!   management plane under a short mutex and publish an immutable
//!   [`ChannelSnapshot`]; generators read snapshots with `try_read` and keep
//!   their cached frame on contention (never block the output cadence).
//! * Transitions are described, not executed, here: a snapshot carries a
//!   monotonically increasing `seq` and the [`TransitionRecord`]. A generator
//!   latches its **own tick counter** when it first sees a new `seq` and asks
//!   [`mix_at`] for the layer + opacity of each later tick — so a fade is a
//!   pure function of (transition, ticks elapsed): deterministic, and costs one
//!   opacity application on an already-prepared image.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};

use multiview_config::{GraphicsChannel, GraphicsConfig, Transition, TransitionKind};
use serde::Serialize;

/// The channel every installation gets even before one is configured.
pub const DEFAULT_CHANNEL_ID: &str = "gfx-channel-1";

/// The suffix of a channel's preview-bus source id (`gfx-channel-1-preview`).
pub const PREVIEW_SUFFIX: &str = "-preview";

/// A playout error (mapped to 404 / 409 by the API).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlayoutError {
    /// No such channel.
    #[error("unknown graphics channel {0:?}")]
    UnknownChannel(String),
    /// No such item in the library.
    #[error("unknown graphics item {0:?}")]
    UnknownItem(String),
    /// TAKE with nothing on preview.
    #[error("nothing is cued on preview")]
    NothingCued,
    /// NEXT past the end of the rundown.
    #[error("end of rundown")]
    EndOfRundown,
}

/// The program change a TAKE / CLEAR produced.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TransitionRecord {
    /// Item on program before the change.
    pub from: Option<String>,
    /// Item on program after the change.
    pub to: Option<String>,
    /// Cut or mix.
    pub kind: TransitionKind,
    /// Mix length in output frames.
    pub frames: u32,
}

/// An immutable published view of one channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelSnapshot {
    /// Channel id.
    pub channel: String,
    /// The cued item (preview bus).
    pub preview: Option<String>,
    /// The on-air item (program bus).
    pub program: Option<String>,
    /// Increments on every program change.
    pub seq: u64,
    /// The latest program change.
    pub transition: TransitionRecord,
}

#[derive(Debug, Default, Clone)]
struct Playout {
    preview: Option<String>,
    program: Option<String>,
    seq: u64,
    transition: TransitionRecord,
}

#[derive(Debug, Default)]
struct Inner {
    /// Effective configuration (always includes at least one channel).
    config: RwLock<Arc<GraphicsConfig>>,
    /// Bumped on every config replacement (the generators' def key).
    revision: AtomicU64,
    /// Mutation state (management plane only).
    playout: Mutex<BTreeMap<String, Playout>>,
    /// Published snapshots (render workers read with `try_read`).
    snapshots: RwLock<BTreeMap<String, Arc<ChannelSnapshot>>>,
}

/// The shared graphics runtime handle (cheap to clone).
#[derive(Debug, Clone, Default)]
pub struct GraphicsHub {
    inner: Arc<Inner>,
}

fn with_default_channel(mut config: GraphicsConfig) -> GraphicsConfig {
    if config.channels.is_empty() {
        config
            .channels
            .push(GraphicsChannel::new(DEFAULT_CHANNEL_ID, "Channel 1"));
    }
    config
}

impl GraphicsHub {
    /// A private hub seeded with `config` (tests, the in-memory harness).
    #[must_use]
    pub fn new(config: GraphicsConfig) -> Self {
        let hub = Self::default();
        hub.set_config(config);
        hub
    }

    /// The process-global hub the binary's control plane and generators share.
    #[must_use]
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<GraphicsHub> = OnceLock::new();
        GLOBAL.get_or_init(|| Self::new(GraphicsConfig::default()))
    }

    /// The effective config (blocking read; management plane).
    #[must_use]
    pub fn config(&self) -> Arc<GraphicsConfig> {
        let guard = self
            .inner
            .config
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if guard.channels.is_empty() {
            // Never-seeded hub: present the default channel.
            return Arc::new(with_default_channel((**guard).clone()));
        }
        Arc::clone(&guard)
    }

    /// Non-blocking read for render workers: `(revision, config)`, or `None`
    /// when the writer holds the lock right now.
    #[must_use]
    pub fn try_config(&self) -> Option<(u64, Arc<GraphicsConfig>)> {
        let guard = self.inner.config.try_read().ok()?;
        Some((
            self.inner.revision.load(Ordering::Acquire),
            Arc::clone(&guard),
        ))
    }

    /// Replace the effective config. On-air / cued items that no longer exist
    /// are cut off (an item deleted while on air leaves program clear), and
    /// removed channels disappear.
    pub fn set_config(&self, config: GraphicsConfig) {
        let config = Arc::new(with_default_channel(config));
        let mut playout = self
            .inner
            .playout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        {
            let mut guard = self
                .inner
                .config
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            *guard = Arc::clone(&config);
            self.inner.revision.fetch_add(1, Ordering::AcqRel);
        }
        playout.retain(|id, _| config.channel(id).is_some());
        for ch in &config.channels {
            let p = playout.entry(ch.id.clone()).or_default();
            if p.preview
                .as_deref()
                .is_some_and(|i| config.item(i).is_none())
            {
                p.preview = None;
            }
            if p.program
                .as_deref()
                .is_some_and(|i| config.item(i).is_none())
            {
                p.seq += 1;
                p.transition = TransitionRecord {
                    from: None,
                    to: None,
                    kind: TransitionKind::Cut,
                    frames: 1,
                };
                p.program = None;
            }
        }
        self.publish_all(&playout);
    }

    fn publish_all(&self, playout: &BTreeMap<String, Playout>) {
        let mut snaps = self
            .inner
            .snapshots
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *snaps = playout
            .iter()
            .map(|(id, p)| (id.clone(), Arc::new(snapshot_of(id, p))))
            .collect();
    }

    fn publish_one(&self, id: &str, p: &Playout) -> ChannelSnapshot {
        let snap = snapshot_of(id, p);
        self.inner
            .snapshots
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_owned(), Arc::new(snap.clone()));
        snap
    }

    /// The channel's published snapshot (blocking; management plane).
    #[must_use]
    pub fn channel(&self, id: &str) -> Option<ChannelSnapshot> {
        let snaps = self
            .inner
            .snapshots
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(s) = snaps.get(id) {
            return Some((**s).clone());
        }
        drop(snaps);
        // A never-seeded hub still answers for the default channel.
        (id == DEFAULT_CHANNEL_ID && self.config().channel(id).is_some())
            .then(|| snapshot_of(id, &Playout::default()))
    }

    /// Non-blocking snapshot read for render workers. `Err(())` on contention
    /// (keep the cached frame); `Ok(None)` for an unknown channel.
    ///
    /// # Errors
    /// `Err(())` when the publisher holds the lock right now.
    #[allow(clippy::result_unit_err)]
    pub fn try_channel(&self, id: &str) -> Result<Option<Arc<ChannelSnapshot>>, ()> {
        match self.inner.snapshots.try_read() {
            Ok(s) => Ok(s.get(id).cloned()),
            Err(_) => Err(()),
        }
    }

    fn mutate<F>(&self, channel: &str, f: F) -> Result<ChannelSnapshot, PlayoutError>
    where
        F: FnOnce(&GraphicsConfig, &GraphicsChannel, &mut Playout) -> Result<(), PlayoutError>,
    {
        let config = self.config();
        let ch = config
            .channel(channel)
            .ok_or_else(|| PlayoutError::UnknownChannel(channel.to_owned()))?;
        let mut playout = self
            .inner
            .playout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let p = playout.entry(channel.to_owned()).or_default();
        f(&config, ch, p)?;
        let p = p.clone();
        drop(playout);
        Ok(self.publish_one(channel, &p))
    }

    /// CUE: put `item` on the preview bus.
    ///
    /// # Errors
    /// Unknown channel / item.
    pub fn cue(&self, channel: &str, item: &str) -> Result<ChannelSnapshot, PlayoutError> {
        self.mutate(channel, |config, _, p| {
            if config.item(item).is_none() {
                return Err(PlayoutError::UnknownItem(item.to_owned()));
            }
            p.preview = Some(item.to_owned());
            Ok(())
        })
    }

    /// TAKE: preview → program with the channel preset (or `transition`), then
    /// auto-cue the rundown entry after the taken item.
    ///
    /// # Errors
    /// Unknown channel, or nothing cued.
    pub fn take(
        &self,
        channel: &str,
        transition: Option<Transition>,
    ) -> Result<ChannelSnapshot, PlayoutError> {
        self.mutate(channel, |_, ch, p| {
            let to = p.preview.clone().ok_or(PlayoutError::NothingCued)?;
            let t = transition.unwrap_or(ch.transition);
            p.transition = TransitionRecord {
                from: p.program.clone(),
                to: Some(to.clone()),
                kind: t.kind,
                frames: t.frames.max(1),
            };
            p.seq += 1;
            p.program = Some(to.clone());
            p.preview = after(&ch.rundown, &to);
            Ok(())
        })
    }

    /// CLEAR / OUT: take program off air (fade out with a mix preset).
    ///
    /// # Errors
    /// Unknown channel.
    pub fn clear(
        &self,
        channel: &str,
        transition: Option<Transition>,
    ) -> Result<ChannelSnapshot, PlayoutError> {
        self.mutate(channel, |_, ch, p| {
            if p.program.is_none() {
                return Ok(());
            }
            let t = transition.unwrap_or(ch.transition);
            p.transition = TransitionRecord {
                from: p.program.take(),
                to: None,
                kind: t.kind,
                frames: t.frames.max(1),
            };
            p.seq += 1;
            Ok(())
        })
    }

    /// NEXT: cue the rundown entry after the current preview (or after
    /// program when nothing is cued, or the first entry).
    ///
    /// # Errors
    /// Unknown channel, or the end of the rundown.
    pub fn next(&self, channel: &str) -> Result<ChannelSnapshot, PlayoutError> {
        self.mutate(channel, |_, ch, p| {
            let anchor = p.preview.as_deref().or(p.program.as_deref());
            let next = match anchor {
                Some(a) if ch.rundown.iter().any(|i| i == a) => after(&ch.rundown, a),
                _ => ch.rundown.first().cloned(),
            };
            let next = next.ok_or(PlayoutError::EndOfRundown)?;
            p.preview = Some(next);
            Ok(())
        })
    }
}

fn after(rundown: &[String], item: &str) -> Option<String> {
    let pos = rundown.iter().position(|i| i == item)?;
    rundown.get(pos + 1).cloned()
}

fn snapshot_of(id: &str, p: &Playout) -> ChannelSnapshot {
    ChannelSnapshot {
        channel: id.to_owned(),
        preview: p.preview.clone(),
        program: p.program.clone(),
        seq: p.seq,
        transition: p.transition.clone(),
    }
}

// ----------------------------------------------------------------- transitions

/// Which prepared layer a tick shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// The outgoing item.
    From,
    /// The incoming item.
    To,
}

/// One tick of a transition: the layer (or `None` = transparent) and its
/// opacity (`255` = fully opaque, i.e. the prepared image unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixFrame {
    /// Layer to show.
    pub layer: Option<Layer>,
    /// Opacity 0–255 applied to that layer's alpha.
    pub opacity: u8,
}

impl MixFrame {
    const fn settled(has_to: bool) -> Self {
        Self {
            layer: if has_to { Some(Layer::To) } else { None },
            opacity: 255,
        }
    }
}

fn ramp(num: u64, den: u64) -> u8 {
    let den = den.max(1);
    u8::try_from(num.min(den) * 255 / den).unwrap_or(u8::MAX)
}

/// The frame shown `elapsed` ticks after a transition was first observed.
///
/// * cut, or `elapsed >= frames`: the incoming item (or transparent);
/// * mix in (nothing → item): opacity ramps up over `frames`;
/// * mix out (item → nothing): opacity ramps down over `frames`;
/// * mix item → item: a **dip through clear** — the outgoing item fades out
///   over the first half, the incoming fades in over the second. Each tick is
///   one opacity application on one prepared image (no per-frame blending of
///   two full images).
///
/// Integer only (no float time), so a ramp is bit-reproducible.
#[must_use]
pub fn mix_at(
    has_from: bool,
    has_to: bool,
    kind: TransitionKind,
    frames: u32,
    elapsed: u64,
) -> MixFrame {
    let frames = u64::from(frames.max(1));
    if kind != TransitionKind::Mix || elapsed >= frames || (!has_from && !has_to) {
        return MixFrame::settled(has_to);
    }
    match (has_from, has_to) {
        (false, true) => MixFrame {
            layer: Some(Layer::To),
            opacity: ramp(elapsed + 1, frames),
        },
        (true, false) => MixFrame {
            layer: Some(Layer::From),
            opacity: ramp(frames - 1 - elapsed, frames),
        },
        _ => {
            let out = frames / 2;
            if elapsed < out {
                MixFrame {
                    layer: Some(Layer::From),
                    opacity: ramp(out - 1 - elapsed, out),
                }
            } else {
                MixFrame {
                    layer: Some(Layer::To),
                    opacity: ramp(elapsed - out + 1, frames - out),
                }
            }
        }
    }
}

/// Scale a straight-alpha plane by `opacity` (0–255) into `out` (resized to
/// match). One table lookup per pixel; `alpha = None` means opaque (255).
pub fn scale_alpha(alpha: Option<&[u8]>, len: usize, opacity: u8, out: &mut Vec<u8>) {
    let mut lut = [0_u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let v = (i * usize::from(opacity) + 127) / 255;
        *slot = u8::try_from(v).unwrap_or(u8::MAX);
    }
    out.clear();
    match alpha {
        Some(a) => out.extend(
            a.iter()
                .take(len)
                .map(|v| lut.get(usize::from(*v)).copied().unwrap_or(opacity)),
        ),
        None => out.resize(len, opacity),
    }
    out.resize(len, 0);
}

#[cfg(test)]
mod tests;
