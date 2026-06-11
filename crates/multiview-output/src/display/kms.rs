//! The real DRM/KMS backend (feature `display-kms`): drm-rs 0.15 atomic
//! commits + GBM/dumb-buffer scanout allocation, implementing [`KmsBackend`]
//! for the hardware-free sink loop in [`super::sink`].
//!
//! **Hardware-only module.** Everything here speaks real ioctls against
//! `/dev/dri/cardN`; CI compiles it (the feature build gate) but exercises it
//! only on hardware — the sink's behaviour (conflation, EBUSY, modeset
//! discipline) is CI-proven through the scripted mock instead. All code is
//! safe Rust: drm-rs is a pure-ioctl safe wrapper, and GBM GEM handles are
//! obtained via prime-fd export/import rather than the C union accessor.
//!
//! ## Frame path — the per-hardware buffer strategy (DEV-B3, brief §2)
//!
//! On first frame the backend resolves the head's
//! [`BufferStrategy`](super::strategy::BufferStrategy) from the probed primary
//! plane's formats/modifiers (`get_plane` + the `IN_FORMATS` blob), the
//! canvas's delivery shape ([`DisplayCanvas::delivery`]), and whether a wgpu
//! importer is wired (see the wgpu-version verdict below). Three paths:
//!
//! * **NV12-direct** (Intel Gen9+ / vc4, incl. the SAND128 modifier): the
//!   canvas's NV12/P010 dmabuf is `prime_fd_to_buffer`-imported and turned into
//!   a planar framebuffer (`add_planar_framebuffer`, `ADDFB2` with modifiers)
//!   that is flipped straight onto the plane — **0 copies, 0 render passes**.
//!   This is drm/gbm-only (no wgpu, no `unsafe`) and runs on the current pin.
//! * **CPU NV12→XRGB** (the guaranteed default, DEV-B1): the CPU conversion
//!   ([`super::canvas::nv12_to_xrgb`], BT.709 limited→full, 8.8 integer) into a
//!   double-buffered XRGB8888 scanout pool — GBM-allocated
//!   (`SCANOUT | WRITE | LINEAR`) where a Mesa GBM backend exists, KMS **dumb
//!   buffers** otherwise (NVIDIA, or no GBM).
//! * **wgpu NV12→XRGB pass** (AMD DCE11 with a GPU): see the verdict below —
//!   **not wired in this crate on the current wgpu pin**; the selector never
//!   resolves to it here, and the backend treats it as the CPU path.
//!
//! ## wgpu-version verdict (DEV-B3) — dmabuf import is deferred; CPU path ships
//!
//! Verified against the workspace pin **wgpu 29.0.3** (the compositor's pin):
//!
//! * `wgpu::SurfaceTargetUnsafe::Drm` **exists** (the NVIDIA tier-2
//!   DRM-surface path) and `wgpu::Device::create_texture_from_hal` **exists**.
//! * `wgpu_hal::vulkan::Device::texture_from_raw(.., external_memory_image_create_info)`
//!   **exists** — the dmabuf-import primitive — but there is **no** safe
//!   high-level `texture_from_dmabuf_fd` in wgpu 29 (that lands in a later
//!   release). Importing an NV12 dmabuf as a wgpu texture on this pin therefore
//!   requires `wgpu-hal` + `ash` as new **direct** deps of `multiview-output`
//!   and a block of raw-Vulkan `unsafe` (build a `vk::Image` with
//!   `VK_EXT_image_drm_format_modifier`, import the fd via
//!   `VK_EXT_external_memory_dma_buf`).
//! * **Verdict: the wgpu render pass is deferred — the CPU NV12→XRGB path is
//!   the shipped AMD/fallback default**, for two reasons honestly stated:
//!   (1) pulling raw-Vulkan `unsafe` into this `forbid(unsafe_code)` crate to
//!   hand-roll dmabuf import is not warranted while the CPU path is correct and
//!   the AMD budget (~0.7 GB/s @ 1080p60) is modest; (2) bumping the **whole
//!   workspace** wgpu to a release with safe dmabuf import risks every GPU
//!   crate and is exactly the unilateral bump DEV-B3 says not to make. The
//!   selector's `WgpuXrgbPass` variant + the `gpu_pass_available` seam compile
//!   and are unit-tested; flipping the seam on is a localized follow-up when
//!   the workspace wgpu pin advances (or a dmabuf-canvas reaches a vc4/Intel
//!   target, where NV12-direct skips the pass entirely).
//!
//! [`BufferStrategy`]: super::strategy::BufferStrategy
//! [`DisplayCanvas::delivery`]: super::canvas::DisplayCanvas::delivery

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use drm::buffer::{Buffer as DrmBuffer, DrmFourcc, DrmModifier, Handle as DrmBufferHandle};
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{
    atomic::AtomicModeReq, connector, crtc, framebuffer, plane, property, AtomicCommitFlags,
    Device as ControlDevice, FbCmd2Flags, Mode, ModeFlags, ModeTypeFlags,
};
use drm::{ClientCapability, Device as BaseDevice};
use rustix::event::{PollFd, PollFlags};

use super::canvas::{nv12_to_xrgb, DisplayCanvas, DmabufImage};
use super::device::{
    ConnectorDesc, ConnectorSelector, DisplayError, FlipEvent, HeadSetup, KmsBackend, SubmitError,
};
use super::mode::DisplayModeInfo;
use super::strategy::{
    parse_in_formats_blob, select_buffer_strategy, BufferStrategy, DrmFormat, PlaneFormatCaps,
    ScanoutCaps,
};

/// `DRM_PLANE_TYPE_PRIMARY` (uapi `drm_mode.h`): the value of a plane's
/// `type` property identifying the primary plane.
const PLANE_TYPE_PRIMARY: u64 = 1;

/// The minimal DRM device wrapper drm-rs needs (the classic `Card(File)`
/// pattern). Opening the primary node while no other KMS master exists makes
/// this fd the implicit DRM master (brief §10).
#[derive(Debug)]
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BaseDevice for Card {}
impl ControlDevice for Card {}

/// One probed connector kept with its native handles/modes so a later
/// [`HeadSetup`] (plain data) can be resolved back onto kernel objects.
#[derive(Debug)]
struct ProbedConnector {
    handle: connector::Handle,
    modes: Vec<Mode>,
}

/// One scanout buffer: the framebuffer the plane flips to, plus how to map
/// and write it.
enum ScanoutBuffer {
    /// GBM-allocated (`SCANOUT | WRITE | LINEAR`), preferred where available.
    Gbm {
        /// Keeps the BO (and its GEM handle) alive for the framebuffer.
        bo: gbm::BufferObject<()>,
        fb: framebuffer::Handle,
    },
    /// KMS dumb buffer — the universal CPU-scanout fallback.
    Dumb {
        db: DumbBuffer,
        fb: framebuffer::Handle,
        pitch: u32,
    },
}

impl ScanoutBuffer {
    fn framebuffer(&self) -> framebuffer::Handle {
        match self {
            ScanoutBuffer::Gbm { fb, .. } | ScanoutBuffer::Dumb { fb, .. } => *fb,
        }
    }
}

/// The atomic property handles a lit head commits against.
#[derive(Debug, Clone, Copy)]
struct HeadProps {
    conn_crtc_id: property::Handle,
    crtc_mode_id: property::Handle,
    crtc_active: property::Handle,
    plane_fb_id: property::Handle,
    plane_crtc_id: property::Handle,
    plane_src_x: property::Handle,
    plane_src_y: property::Handle,
    plane_src_w: property::Handle,
    plane_src_h: property::Handle,
    plane_crtc_x: property::Handle,
    plane_crtc_y: property::Handle,
    plane_crtc_w: property::Handle,
    plane_crtc_h: property::Handle,
}

/// A prepared (validated and/or lit) head: resolved kernel objects, the mode
/// blob, and the double-buffered scanout pool.
struct PreparedHead {
    connector: connector::Handle,
    crtc: crtc::Handle,
    plane: plane::Handle,
    /// The `MODE_ID` blob id created for the selected timing.
    mode_blob_id: u64,
    props: HeadProps,
    buffers: Vec<ScanoutBuffer>,
    /// Index of the buffer currently (or about to be) on glass; the other is
    /// the write target. With commit-only-when-idle, two buffers suffice.
    front: usize,
    width: u32,
    height: u32,
    /// The primary plane's probed formats/modifiers (`get_plane` +
    /// `IN_FORMATS`) — the NV12-direct gate input (DEV-B3 / brief §2).
    plane_caps: PlaneFormatCaps,
    /// The chosen per-frame buffer strategy, resolved on the **first** frame
    /// from the canvas's delivery shape + `plane_caps` + GPU availability, then
    /// cached (the canvas delivery shape is stable for a run).
    strategy: Option<BufferStrategy>,
    /// The NV12-direct framebuffer most recently imported + flipped, together
    /// with the GEM handles that back it. Held so the framebuffer **and** its
    /// imported handles stay alive while it is on glass; on the next direct
    /// frame the old fb is destroyed and its handles closed (open/close parity
    /// — see [`DirectScanoutState`]).
    direct: DirectScanoutState,
}

/// The NV12-direct path's live scanout resources: the framebuffer most recently
/// flipped and the GEM handles `prime_fd_to_buffer`-imported to build it.
///
/// Each direct frame imports one GEM handle per dmabuf plane (≈2 for NV12) and
/// builds an `ADDFB2` framebuffer over them. Those handles are **not** freed by
/// `destroy_framebuffer` — they must be closed explicitly (`GEM_CLOSE`) or a
/// long direct-scanout run leaks ~`fps × planes` handles/second until the GEM
/// handle table is exhausted. This state tracks the current handles so they are
/// closed exactly once: when the fb they back is retired (replaced by a newer
/// direct frame, or rejected on the EBUSY/device-error path), and on teardown.
///
/// Ordering invariant: the framebuffer is destroyed **before** its handles are
/// closed (the fb references the handles), and the handles of the fb currently
/// on glass are never closed until that fb is retired.
#[derive(Default)]
struct DirectScanoutState {
    /// The framebuffer currently (or about to be) on glass, if any.
    fb: Option<framebuffer::Handle>,
    /// The GEM handles backing [`Self::fb`] (one per dmabuf plane).
    handles: Vec<DrmBufferHandle>,
}

/// The real KMS backend: owns the card fd (and the GBM device dup'd onto it)
/// for the lifetime of the sink. After
/// [`DisplaySink::start`](super::sink::DisplaySink::start) hands it to the
/// flip thread, that thread is the only user — "a dedicated thread owns the
/// DRM fd" (ADR-0044).
///
/// Kernel-side resources (framebuffers, dumb buffers, the mode blob) are
/// released by the kernel when the fd closes at drop. The **NV12-direct** path
/// is the exception during a *run*: each direct frame imports GEM handles
/// (`prime_fd_to_buffer`) that `destroy_framebuffer` does **not** free, so they
/// are tracked in the head's [`DirectScanoutState`] and closed (`GEM_CLOSE`)
/// when each fb is retired — and any final on-glass handles are closed in
/// [`Drop`] — so a long direct-scanout run never leaks GEM handles (it does not
/// wait for fd-close to bound the live handle count).
pub struct KmsDisplayDevice {
    card: Card,
    card_path: PathBuf,
    /// The GBM allocator over a dup of the same fd; `None` when the driver
    /// has no GBM backend (the dumb-buffer fallback is used instead).
    gbm: Option<gbm::Device<Card>>,
    probed: HashMap<String, ProbedConnector>,
    head: Option<PreparedHead>,
    /// Whether a wgpu NV12→XRGB import-and-render pass is wired for this build.
    /// **`false` on the current wgpu 29 pin** (no safe dmabuf-import API; see
    /// the module-level wgpu-version verdict) — so the strategy selector never
    /// resolves to `WgpuXrgbPass` here, and AMD/RGB-only heads take the CPU
    /// NV12→XRGB path. The seam exists so flipping it on is a localized change.
    gpu_pass_available: bool,
}

impl std::fmt::Debug for KmsDisplayDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KmsDisplayDevice")
            .field("card_path", &self.card_path)
            .field("gbm", &self.gbm.is_some())
            .field("probed", &self.probed.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Drop for KmsDisplayDevice {
    fn drop(&mut self) {
        // Close any NV12-direct GEM handles still on glass at teardown so a
        // stopped sink nets to open/close parity (the kernel would also reap
        // them at fd-close, but this bounds the live count deterministically).
        if let Some(head) = self.head.as_mut() {
            let mut direct = std::mem::take(&mut head.direct);
            direct.teardown(&self.card);
        }
    }
}

/// Wrap an ioctl error with context.
fn dev_err(context: &str, e: &std::io::Error) -> DisplayError {
    DisplayError::Device(format!("{context}: {e}"))
}

impl KmsDisplayDevice {
    /// Open one DRM primary node (`/dev/dri/cardN`) and enable the atomic +
    /// universal-planes client capabilities.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Device`] when the node cannot be opened or the driver
    /// does not support atomic modesetting.
    pub fn open_path(path: &Path) -> Result<Self, DisplayError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| dev_err(&format!("opening {}", path.display()), &e))?;
        let card = Card(file);
        card.set_client_capability(ClientCapability::UniversalPlanes, true)
            .map_err(|e| dev_err("enabling universal planes", &e))?;
        card.set_client_capability(ClientCapability::Atomic, true)
            .map_err(|e| dev_err("enabling atomic modesetting", &e))?;
        // A GBM device over a dup of the same fd; absence (e.g. the NVIDIA
        // proprietary driver) selects the dumb-buffer pool instead.
        let gbm = card
            .0
            .try_clone()
            .ok()
            .and_then(|dup| gbm::Device::new(Card(dup)).ok());
        if gbm.is_none() {
            tracing::info!(
                card = %path.display(),
                "no GBM backend; using KMS dumb buffers for scanout"
            );
        }
        Ok(Self {
            card,
            card_path: path.to_path_buf(),
            gbm,
            probed: HashMap::new(),
            head: None,
            // No wgpu importer is wired in this crate on the current wgpu 29
            // pin (module-level verdict): AMD/RGB-only heads take the CPU path.
            gpu_pass_available: false,
        })
    }

    /// Scan `/dev/dri/card*` for the device exposing the selected connector
    /// (`Auto` = the first card with any connected connector).
    ///
    /// # Errors
    ///
    /// [`DisplayError`] naming what was probed when no card matches.
    pub fn open_for_connector(selector: &ConnectorSelector) -> Result<Self, DisplayError> {
        let mut card_paths: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
            .map_err(|e| dev_err("listing /dev/dri", &e))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("card"))
            })
            .collect();
        card_paths.sort();
        let mut seen: Vec<String> = Vec::new();
        for path in &card_paths {
            let Ok(mut device) = Self::open_path(path) else {
                continue;
            };
            let Ok(connectors) = device.probe_connectors() else {
                continue;
            };
            let hit = match selector {
                ConnectorSelector::Auto => connectors.iter().any(|c| c.connected),
                ConnectorSelector::Name(name) => connectors.iter().any(|c| &c.name == name),
            };
            if hit {
                return Ok(device);
            }
            seen.extend(connectors.into_iter().map(|c| c.name));
        }
        match selector {
            ConnectorSelector::Auto => Err(DisplayError::NoneConnected { probed: seen }),
            ConnectorSelector::Name(name) => Err(DisplayError::ConnectorNotFound {
                requested: name.clone(),
                available: seen,
            }),
        }
    }

    /// Resolve a probed connector by kernel name.
    fn probed(&self, name: &str) -> Result<&ProbedConnector, DisplayError> {
        self.probed
            .get(name)
            .ok_or_else(|| DisplayError::ConnectorNotFound {
                requested: name.to_owned(),
                available: self.probed.keys().cloned().collect(),
            })
    }

    /// Find the primary plane that can drive `crtc` and scans out XRGB8888.
    fn find_primary_plane(&self, crtc: crtc::Handle) -> Result<plane::Handle, DisplayError> {
        let res = self
            .card
            .resource_handles()
            .map_err(|e| dev_err("reading resources", &e))?;
        let planes = self
            .card
            .plane_handles()
            .map_err(|e| dev_err("listing planes", &e))?;
        // The XRGB8888 fourcc code (`XR24`), as planes report formats raw.
        let xrgb = u32::from_le_bytes(*b"XR24");
        for ph in planes {
            let info = self
                .card
                .get_plane(ph)
                .map_err(|e| dev_err("reading plane", &e))?;
            if !res.filter_crtcs(info.possible_crtcs()).contains(&crtc) {
                continue;
            }
            if !info.formats().contains(&xrgb) {
                continue;
            }
            if self.plane_type(ph)? == PLANE_TYPE_PRIMARY {
                return Ok(ph);
            }
        }
        Err(DisplayError::Device(
            "no XRGB8888-capable primary plane for the chosen CRTC".to_owned(),
        ))
    }

    /// Probe a plane's advertised scanout formats and modifiers into the pure
    /// [`PlaneFormatCaps`] the strategy selector reasons over (DEV-B3): the
    /// format list from `get_plane`, and the modifier list from the plane's
    /// `IN_FORMATS` property blob where the driver exposes one (legacy drivers
    /// without the blob are treated as linear-only by the pure layer).
    fn probe_plane_caps(&self, plane: plane::Handle) -> Result<PlaneFormatCaps, DisplayError> {
        let info = self
            .card
            .get_plane(plane)
            .map_err(|e| dev_err("reading plane formats", &e))?;
        let formats: Vec<DrmFormat> = info
            .formats()
            .iter()
            .map(|raw| DrmFormat::from_fourcc(raw.to_le_bytes()))
            .collect();
        // IN_FORMATS carries the per-format modifier set; absent on legacy
        // drivers, in which case `parse_in_formats_blob` is never reached and
        // we keep the format list with an empty (linear-only) modifier list.
        if let Some(blob_id) = self.in_formats_blob_id(plane)? {
            if let Ok(bytes) = self.card.get_property_blob(blob_id) {
                if let Some(caps) = parse_in_formats_blob(&bytes) {
                    return Ok(caps);
                }
            }
        }
        Ok(PlaneFormatCaps::new(formats, Vec::new()))
    }

    /// The `IN_FORMATS` blob id for a plane, if the property exists and is set.
    fn in_formats_blob_id(&self, plane: plane::Handle) -> Result<Option<u64>, DisplayError> {
        let props = self
            .card
            .get_properties(plane)
            .map_err(|e| dev_err("reading plane properties", &e))?;
        let (handles, values) = props.as_props_and_values();
        for (ph, value) in handles.iter().zip(values.iter()) {
            let info = self
                .card
                .get_property(*ph)
                .map_err(|e| dev_err("reading property", &e))?;
            if info.name().to_str() == Ok("IN_FORMATS") {
                // A zero blob id means the property exists but is unset.
                return Ok((*value != 0).then_some(*value));
            }
        }
        Ok(None)
    }

    /// Read a plane's `type` property value.
    fn plane_type(&self, plane: plane::Handle) -> Result<u64, DisplayError> {
        let props = self
            .card
            .get_properties(plane)
            .map_err(|e| dev_err("reading plane properties", &e))?;
        let (handles, values) = props.as_props_and_values();
        for (ph, value) in handles.iter().zip(values.iter()) {
            let info = self
                .card
                .get_property(*ph)
                .map_err(|e| dev_err("reading property", &e))?;
            if info.name().to_str() == Ok("type") {
                return Ok(*value);
            }
        }
        Err(DisplayError::Device(
            "plane exposes no `type` property".to_owned(),
        ))
    }

    /// Look up a named property on a KMS object.
    fn find_prop<T>(&self, handle: T, name: &str) -> Result<property::Handle, DisplayError>
    where
        T: drm::control::ResourceHandle,
    {
        let props = self
            .card
            .get_properties(handle)
            .map_err(|e| dev_err("reading properties", &e))?;
        let (handles, _) = props.as_props_and_values();
        for ph in handles {
            let info = self
                .card
                .get_property(*ph)
                .map_err(|e| dev_err("reading property", &e))?;
            if info.name().to_str() == Ok(name) {
                return Ok(*ph);
            }
        }
        Err(DisplayError::Device(format!(
            "required KMS property {name:?} not found"
        )))
    }

    /// Allocate one XRGB8888 scanout buffer: GBM first, dumb-buffer fallback.
    fn allocate_buffer(&self, width: u32, height: u32) -> Result<ScanoutBuffer, DisplayError> {
        if let Some(gbm) = &self.gbm {
            match Self::allocate_gbm(gbm, &self.card, width, height) {
                Ok(buffer) => return Ok(buffer),
                Err(e) => {
                    tracing::info!(
                        error = %e,
                        "GBM scanout allocation unavailable; falling back to dumb buffers"
                    );
                }
            }
        }
        let mut db = self
            .card
            .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)
            .map_err(|e| dev_err("creating dumb buffer", &e))?;
        let pitch = db.pitch();
        // Light the buffer black before first use (dumb buffers are
        // zero-filled by the kernel; the explicit clear keeps the contract
        // independent of that detail).
        {
            let mut mapping = self
                .card
                .map_dumb_buffer(&mut db)
                .map_err(|e| dev_err("mapping dumb buffer", &e))?;
            mapping.as_mut().fill(0);
        }
        let fb = self
            .card
            .add_framebuffer(&db, 24, 32)
            .map_err(|e| dev_err("adding dumb framebuffer", &e))?;
        Ok(ScanoutBuffer::Dumb { db, fb, pitch })
    }

    /// Allocate a GBM scanout BO and import it as a framebuffer via prime-fd
    /// export → GEM import (the safe path; no C union access).
    fn allocate_gbm(
        gbm: &gbm::Device<Card>,
        card: &Card,
        width: u32,
        height: u32,
    ) -> Result<ScanoutBuffer, DisplayError> {
        use gbm::BufferObjectFlags;
        let mut bo = gbm
            .create_buffer_object::<()>(
                width,
                height,
                gbm::Format::Xrgb8888,
                BufferObjectFlags::SCANOUT | BufferObjectFlags::WRITE | BufferObjectFlags::LINEAR,
            )
            .map_err(|e| dev_err("creating GBM scanout BO", &e))?;
        // Clear to black before first scanout (GBM memory is uninitialized).
        bo.map_mut(0, 0, width, height, |mapping| {
            mapping.buffer_mut().fill(0);
        })
        .map_err(|e| dev_err("mapping GBM BO", &e))?;
        let prime = bo
            .fd()
            .map_err(|e| DisplayError::Device(format!("exporting GBM BO fd: {e}")))?;
        let handle = card
            .prime_fd_to_buffer(prime.as_fd())
            .map_err(|e| dev_err("importing GBM BO", &e))?;
        let adapter = PrimeBuffer {
            size: (width, height),
            pitch: bo.stride(),
            handle,
        };
        let fb = card
            .add_framebuffer(&adapter, 24, 32)
            .map_err(|e| dev_err("adding GBM framebuffer", &e))?;
        Ok(ScanoutBuffer::Gbm { bo, fb })
    }

    /// Prepare (or reuse) the head state for `setup`: resolve kernel objects,
    /// build the mode blob, allocate the scanout pool, gather property
    /// handles. Idempotent across `validate_setup` → `apply_modeset`.
    fn prepare_head(&mut self, setup: &HeadSetup) -> Result<(), DisplayError> {
        if self.head.is_some() {
            return Ok(());
        }
        if self.probed.is_empty() {
            self.probe_connectors()?;
        }
        let probed = self.probed(&setup.connector)?;
        let conn_handle = probed.handle;
        // The native mode: the EDID mode whose timings equal the selection,
        // or a user-defined mode built from the (CVT-RB) timings.
        let native = probed
            .modes
            .iter()
            .find(|m| mode_to_info(m) == setup.mode)
            .copied()
            .map_or_else(|| native_mode_from_info(&setup.mode), Ok)?;
        // CRTC: the connector's current encoder's CRTC when lit, else the
        // first CRTC any of its encoders can drive.
        let conn_info = self
            .card
            .get_connector(conn_handle, false)
            .map_err(|e| dev_err("reading connector", &e))?;
        let res = self
            .card
            .resource_handles()
            .map_err(|e| dev_err("reading resources", &e))?;
        let mut chosen_crtc: Option<crtc::Handle> = None;
        if let Some(enc) = conn_info.current_encoder() {
            if let Ok(info) = self.card.get_encoder(enc) {
                chosen_crtc = info.crtc();
            }
        }
        if chosen_crtc.is_none() {
            for enc in conn_info.encoders() {
                let Ok(info) = self.card.get_encoder(*enc) else {
                    continue;
                };
                if let Some(c) = res.filter_crtcs(info.possible_crtcs()).first() {
                    chosen_crtc = Some(*c);
                    break;
                }
            }
        }
        let crtc = chosen_crtc.ok_or_else(|| {
            DisplayError::Device(format!("no CRTC can drive connector {}", setup.connector))
        })?;
        let plane = self.find_primary_plane(crtc)?;
        let blob_value = self
            .card
            .create_property_blob(&native)
            .map_err(|e| dev_err("creating mode blob", &e))?;
        let property::Value::Blob(mode_blob_id) = blob_value else {
            return Err(DisplayError::Device(
                "mode blob creation returned a non-blob value".to_owned(),
            ));
        };
        let props = HeadProps {
            conn_crtc_id: self.find_prop(conn_handle, "CRTC_ID")?,
            crtc_mode_id: self.find_prop(crtc, "MODE_ID")?,
            crtc_active: self.find_prop(crtc, "ACTIVE")?,
            plane_fb_id: self.find_prop(plane, "FB_ID")?,
            plane_crtc_id: self.find_prop(plane, "CRTC_ID")?,
            plane_src_x: self.find_prop(plane, "SRC_X")?,
            plane_src_y: self.find_prop(plane, "SRC_Y")?,
            plane_src_w: self.find_prop(plane, "SRC_W")?,
            plane_src_h: self.find_prop(plane, "SRC_H")?,
            plane_crtc_x: self.find_prop(plane, "CRTC_X")?,
            plane_crtc_y: self.find_prop(plane, "CRTC_Y")?,
            plane_crtc_w: self.find_prop(plane, "CRTC_W")?,
            plane_crtc_h: self.find_prop(plane, "CRTC_H")?,
        };
        let buffers = vec![
            self.allocate_buffer(setup.mode.width, setup.mode.height)?,
            self.allocate_buffer(setup.mode.width, setup.mode.height)?,
        ];
        let plane_caps = self.probe_plane_caps(plane)?;
        self.head = Some(PreparedHead {
            connector: conn_handle,
            crtc,
            plane,
            mode_blob_id,
            props,
            buffers,
            front: 0,
            width: setup.mode.width,
            height: setup.mode.height,
            plane_caps,
            strategy: None,
            direct: DirectScanoutState::default(),
        });
        Ok(())
    }

    /// Build the full-state atomic request (connector→CRTC→plane→fb) used by
    /// both the `TEST_ONLY` validation and the real modeset.
    fn full_state_request(head: &PreparedHead, fb: framebuffer::Handle) -> AtomicModeReq {
        let mut req = AtomicModeReq::new();
        let p = head.props;
        req.add_property(
            head.connector,
            p.conn_crtc_id,
            property::Value::CRTC(Some(head.crtc)),
        );
        req.add_property(
            head.crtc,
            p.crtc_mode_id,
            property::Value::Blob(head.mode_blob_id),
        );
        req.add_property(head.crtc, p.crtc_active, property::Value::Boolean(true));
        req.add_property(
            head.plane,
            p.plane_fb_id,
            property::Value::Framebuffer(Some(fb)),
        );
        req.add_property(
            head.plane,
            p.plane_crtc_id,
            property::Value::CRTC(Some(head.crtc)),
        );
        req.add_property(head.plane, p.plane_src_x, property::Value::UnsignedRange(0));
        req.add_property(head.plane, p.plane_src_y, property::Value::UnsignedRange(0));
        // SRC_W/H are 16.16 fixed point.
        req.add_property(
            head.plane,
            p.plane_src_w,
            property::Value::UnsignedRange(u64::from(head.width) << 16),
        );
        req.add_property(
            head.plane,
            p.plane_src_h,
            property::Value::UnsignedRange(u64::from(head.height) << 16),
        );
        req.add_property(head.plane, p.plane_crtc_x, property::Value::SignedRange(0));
        req.add_property(head.plane, p.plane_crtc_y, property::Value::SignedRange(0));
        req.add_property(
            head.plane,
            p.plane_crtc_w,
            property::Value::UnsignedRange(u64::from(head.width)),
        );
        req.add_property(
            head.plane,
            p.plane_crtc_h,
            property::Value::UnsignedRange(u64::from(head.height)),
        );
        req
    }

    /// Write `frame` into scanout buffer `index` (CPU NV12→XRGB v1 path).
    fn write_frame(&mut self, index: usize, frame: &dyn DisplayCanvas) -> Result<(), DisplayError> {
        let Some(head) = self.head.as_mut() else {
            return Err(DisplayError::Device("no lit head".to_owned()));
        };
        let (width, height) = (head.width, head.height);
        match head.buffers.get_mut(index) {
            Some(ScanoutBuffer::Gbm { bo, .. }) => bo
                .map_mut(0, 0, width, height, |mapping| {
                    let stride = mapping.stride();
                    nv12_to_xrgb(frame, mapping.buffer_mut(), width, height, stride)
                })
                .map_err(|e| dev_err("mapping GBM BO", &e))?
                .map_err(|e| DisplayError::Device(format!("converting frame: {e}"))),
            Some(ScanoutBuffer::Dumb { db, pitch, .. }) => {
                let pitch = *pitch;
                let mut mapping = self
                    .card
                    .map_dumb_buffer(db)
                    .map_err(|e| dev_err("mapping dumb buffer", &e))?;
                nv12_to_xrgb(frame, mapping.as_mut(), width, height, pitch)
                    .map_err(|e| DisplayError::Device(format!("converting frame: {e}")))
            }
            None => Err(DisplayError::Device("scanout buffer missing".to_owned())),
        }
    }

    /// Resolve (and cache) the head's buffer strategy from `frame`'s delivery
    /// shape, the probed plane caps, and GPU availability (DEV-B3 / brief §2).
    /// Resolved once on the first frame (the canvas delivery shape is stable
    /// for a run); cached thereafter. Falls back to the CPU convert when no
    /// head is prepared (defensive — `submit_frame` errors on that anyway).
    fn resolve_strategy(&mut self, frame: &dyn DisplayCanvas) -> BufferStrategy {
        let gpu = self.gpu_pass_available;
        let Some(head) = self.head.as_mut() else {
            return BufferStrategy::CpuXrgbConvert;
        };
        if let Some(cached) = head.strategy {
            return cached;
        }
        let caps = ScanoutCaps {
            plane: head.plane_caps.clone(),
            canvas: frame.delivery(),
            gpu_pass_available: gpu,
        };
        let chosen = select_buffer_strategy(&caps);
        tracing::info!(strategy = ?chosen, "display buffer strategy resolved");
        head.strategy = Some(chosen);
        chosen
    }

    /// The CPU NV12→XRGB path (DEV-B1): convert into the back scanout buffer,
    /// then flip it. The portable, always-correct default.
    fn submit_xrgb(&mut self, frame: &dyn DisplayCanvas) -> Result<(), SubmitError> {
        let back = match self.head.as_ref() {
            Some(head) => 1 - head.front,
            None => {
                return Err(SubmitError::Device(DisplayError::Device(
                    "no lit head".to_owned(),
                )))
            }
        };
        self.write_frame(back, frame).map_err(SubmitError::Device)?;
        let fb = match self.head.as_ref() {
            Some(head) => head.buffers.get(back).map(ScanoutBuffer::framebuffer),
            None => None,
        }
        .ok_or_else(|| {
            SubmitError::Device(DisplayError::Device("scanout buffer missing".to_owned()))
        })?;
        self.flip_to(fb, Some(back))
    }

    /// The NV12-direct path (DEV-B3, brief §2): `prime_fd_to_buffer`-import the
    /// canvas's NV12/P010 dmabuf, build a planar framebuffer over it
    /// (`add_planar_framebuffer` / `ADDFB2` with modifiers), and flip it — **0
    /// copies, 0 render passes**. The strategy gate guarantees the format +
    /// modifier are plane-compatible; we re-validate the canvas actually offers
    /// a matching dmabuf image and error (→ caller's CPU fallback) otherwise.
    ///
    /// Resolves the plane + `FB_ID` property from the lit head, then delegates
    /// the import / flip / handle-lifecycle to [`submit_direct_over`] over the
    /// card (the device seam). The imported GEM handles are tracked in the
    /// head's [`DirectScanoutState`] and closed on retire — no per-frame leak.
    fn submit_direct(
        &mut self,
        frame: &dyn DisplayCanvas,
        format: DrmFormat,
        modifier: Option<u64>,
    ) -> Result<(), SubmitError> {
        let image = frame.dmabuf_image().ok_or_else(|| {
            SubmitError::Device(DisplayError::Device(
                "NV12-direct chosen but the canvas exposed no dmabuf image".to_owned(),
            ))
        })?;
        // Split-borrow `card` (the device seam) and the head's direct state so
        // the lifecycle helper can hold both disjointly.
        let card = &self.card;
        let head = self
            .head
            .as_mut()
            .ok_or_else(|| SubmitError::Device(DisplayError::Device("no lit head".to_owned())))?;
        let plane = head.plane;
        let fb_id = head.props.plane_fb_id;
        submit_direct_over(
            card,
            &mut head.direct,
            plane,
            fb_id,
            &image,
            format,
            modifier,
        )
    }

    /// Issue the one flip commit: set `fb` on the primary plane,
    /// `NONBLOCK | PAGE_FLIP_EVENT`, never `ALLOW_MODESET` (ADR-0044 §1). On
    /// success advances `front` to `back` when the XRGB pool is in use.
    /// `EBUSY` is the kernel's one-in-flight conflation (never queue/retry).
    fn flip_to(&mut self, fb: framebuffer::Handle, back: Option<usize>) -> Result<(), SubmitError> {
        let plane = match self.head.as_ref() {
            Some(head) => head.plane,
            None => {
                return Err(SubmitError::Device(DisplayError::Device(
                    "no lit head".to_owned(),
                )))
            }
        };
        let fb_id = match self.head.as_ref() {
            Some(head) => head.props.plane_fb_id,
            None => {
                return Err(SubmitError::Device(DisplayError::Device(
                    "no lit head".to_owned(),
                )))
            }
        };
        let mut req = AtomicModeReq::new();
        req.add_property(plane, fb_id, property::Value::Framebuffer(Some(fb)));
        match self.card.atomic_commit(
            AtomicCommitFlags::NONBLOCK | AtomicCommitFlags::PAGE_FLIP_EVENT,
            req,
        ) {
            Ok(()) => {
                if let (Some(back), Some(head)) = (back, self.head.as_mut()) {
                    head.front = back;
                }
                Ok(())
            }
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::BUSY.raw_os_error()) => {
                Err(SubmitError::Busy)
            }
            Err(e) => Err(SubmitError::Device(dev_err("flip commit failed", &e))),
        }
    }
}

impl KmsBackend for KmsDisplayDevice {
    fn probe_connectors(&mut self) -> Result<Vec<ConnectorDesc>, DisplayError> {
        let res = self
            .card
            .resource_handles()
            .map_err(|e| dev_err("reading resources", &e))?;
        let mut out = Vec::new();
        self.probed.clear();
        for handle in res.connectors() {
            // force_probe: this runs at startup only (never the frame path),
            // exactly when fresh EDID/connection state is wanted.
            let info = self
                .card
                .get_connector(*handle, true)
                .map_err(|e| dev_err("probing connector", &e))?;
            let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
            let connected = info.state() == connector::State::Connected;
            let modes: Vec<DisplayModeInfo> = info.modes().iter().map(mode_to_info).collect();
            self.probed.insert(
                name.clone(),
                ProbedConnector {
                    handle: *handle,
                    modes: info.modes().to_vec(),
                },
            );
            out.push(ConnectorDesc {
                name,
                connected,
                modes,
            });
        }
        Ok(out)
    }

    fn validate_setup(&mut self, setup: &HeadSetup) -> Result<(), DisplayError> {
        self.prepare_head(setup)?;
        let Some(head) = self.head.as_ref() else {
            return Err(DisplayError::Device("head preparation lost".to_owned()));
        };
        let fb = head
            .buffers
            .first()
            .map(ScanoutBuffer::framebuffer)
            .ok_or_else(|| DisplayError::Device("scanout pool empty".to_owned()))?;
        let req = Self::full_state_request(head, fb);
        self.card
            .atomic_commit(
                AtomicCommitFlags::TEST_ONLY | AtomicCommitFlags::ALLOW_MODESET,
                req,
            )
            .map_err(|e| dev_err("TEST_ONLY validation rejected the configuration", &e))
    }

    fn apply_modeset(&mut self, setup: &HeadSetup) -> Result<(), DisplayError> {
        self.prepare_head(setup)?;
        let Some(head) = self.head.as_mut() else {
            return Err(DisplayError::Device("head preparation lost".to_owned()));
        };
        let fb = head
            .buffers
            .first()
            .map(ScanoutBuffer::framebuffer)
            .ok_or_else(|| DisplayError::Device("scanout pool empty".to_owned()))?;
        let req = Self::full_state_request(head, fb);
        head.front = 0;
        // The ONE blocking ALLOW_MODESET commit (startup / Class-2 only).
        self.card
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .map_err(|e| dev_err("modeset commit failed", &e))
    }

    fn submit_frame(&mut self, frame: &dyn DisplayCanvas) -> Result<(), SubmitError> {
        match self.resolve_strategy(frame) {
            // Intel/vc4: import the canvas NV12 dmabuf and flip it — 0 copies,
            // 0 render passes. If the dmabuf import fails for any reason, fall
            // back to the always-correct CPU XRGB path rather than dropping a
            // frame (bad inputs are the purpose — the display never falters).
            BufferStrategy::Nv12Direct { format, modifier } => {
                match self.submit_direct(frame, format, modifier) {
                    Ok(()) => Ok(()),
                    Err(SubmitError::Busy) => Err(SubmitError::Busy),
                    Err(SubmitError::Device(e)) => {
                        tracing::warn!(
                            error = %e,
                            "NV12-direct scanout failed; this frame via the CPU XRGB path"
                        );
                        self.submit_xrgb(frame)
                    }
                }
            }
            // AMD/RGB-only, and the GPU pass is not wired on this wgpu pin
            // (module verdict): the CPU NV12→XRGB conversion is the path.
            BufferStrategy::WgpuXrgbPass | BufferStrategy::CpuXrgbConvert => {
                self.submit_xrgb(frame)
            }
        }
    }

    fn wait_events(&mut self, timeout: Duration) -> Result<Vec<FlipEvent>, DisplayError> {
        let timeout_ms = i32::try_from(timeout.as_millis())
            .unwrap_or(i32::MAX)
            .max(1);
        let mut fds = [PollFd::new(&self.card, PollFlags::IN)];
        match rustix::event::poll(&mut fds, timeout_ms) {
            Ok(0) => Ok(Vec::new()),
            Ok(_) => {
                let events = self
                    .card
                    .receive_events()
                    .map_err(|e| dev_err("reading DRM events", &e))?;
                Ok(events
                    .filter_map(|event| match event {
                        drm::control::Event::PageFlip(flip) => Some(FlipEvent {
                            crtc_frame: flip.frame,
                            timestamp: flip.duration,
                        }),
                        _ => None,
                    })
                    .collect())
            }
            // A signal interrupting the wait is not an error; the loop simply
            // re-checks the mailbox/stop flag.
            Err(e) if e == rustix::io::Errno::INTR => Ok(Vec::new()),
            Err(e) => Err(DisplayError::Device(format!("polling the DRM fd: {e}"))),
        }
    }
}

/// A drm `Buffer` view over a prime-imported GBM BO (handle + geometry),
/// enough for `add_framebuffer`.
struct PrimeBuffer {
    size: (u32, u32),
    pitch: u32,
    handle: drm::buffer::Handle,
}

impl DrmBuffer for PrimeBuffer {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Xrgb8888
    }
    fn pitch(&self) -> u32 {
        self.pitch
    }
    fn handle(&self) -> drm::buffer::Handle {
        self.handle
    }
}

/// A drm [`PlanarBuffer`](drm::buffer::PlanarBuffer) view over a
/// prime-imported, possibly multi-plane canvas dmabuf (NV12/P010 + modifier) —
/// enough for `add_planar_framebuffer` (`ADDFB2`). Built by
/// [`KmsDisplayDevice::import_planar_fb`] for the NV12-direct scanout path.
struct PlanarPrimeBuffer {
    size: (u32, u32),
    format: DrmFourcc,
    modifier: Option<DrmModifier>,
    handles: [Option<DrmBufferHandle>; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
}

impl drm::buffer::PlanarBuffer for PlanarPrimeBuffer {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        self.format
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier
    }
    fn pitches(&self) -> [u32; 4] {
        self.pitches
    }
    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        self.handles
    }
    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
}

/// The device operations the NV12-direct scanout path performs, factored behind
/// a trait so the GEM-handle open/close lifecycle is unit-testable without
/// hardware: the real implementation is [`Card`] (drm-rs ioctls); tests use a
/// counting mock that records imports vs closes. Every method maps 1:1 onto a
/// drm-rs `ControlDevice` call.
trait DirectScanoutDevice {
    /// Import a dmabuf prime fd to a GEM buffer handle (`prime_fd_to_buffer`).
    fn import_plane(&self, fd: BorrowedFd<'_>) -> Result<DrmBufferHandle, DisplayError>;
    /// Add a planar framebuffer over imported handles (`ADDFB2`).
    fn add_planar_fb(
        &self,
        buffer: &PlanarPrimeBuffer,
        flags: FbCmd2Flags,
    ) -> Result<framebuffer::Handle, DisplayError>;
    /// Commit a nonblocking page flip of `fb` onto `plane` (`FB_ID`).
    fn flip_to_fb(
        &self,
        plane: plane::Handle,
        fb_id: property::Handle,
        fb: framebuffer::Handle,
    ) -> Result<(), SubmitError>;
    /// Remove a framebuffer (`RMFB`). Does **not** free the GEM handles it
    /// referenced — those need [`Self::close_handle`].
    fn destroy_fb(&self, fb: framebuffer::Handle);
    /// Close a GEM buffer handle (`GEM_CLOSE`) — releases the dmabuf import.
    fn close_handle(&self, handle: DrmBufferHandle);
}

impl DirectScanoutDevice for Card {
    fn import_plane(&self, fd: BorrowedFd<'_>) -> Result<DrmBufferHandle, DisplayError> {
        self.prime_fd_to_buffer(fd)
            .map_err(|e| dev_err("importing canvas dmabuf plane", &e))
    }

    fn add_planar_fb(
        &self,
        buffer: &PlanarPrimeBuffer,
        flags: FbCmd2Flags,
    ) -> Result<framebuffer::Handle, DisplayError> {
        self.add_planar_framebuffer(buffer, flags)
            .map_err(|e| dev_err("adding NV12-direct planar framebuffer", &e))
    }

    fn flip_to_fb(
        &self,
        plane: plane::Handle,
        fb_id: property::Handle,
        fb: framebuffer::Handle,
    ) -> Result<(), SubmitError> {
        let mut req = AtomicModeReq::new();
        req.add_property(plane, fb_id, property::Value::Framebuffer(Some(fb)));
        match self.atomic_commit(
            AtomicCommitFlags::NONBLOCK | AtomicCommitFlags::PAGE_FLIP_EVENT,
            req,
        ) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::BUSY.raw_os_error()) => {
                Err(SubmitError::Busy)
            }
            Err(e) => Err(SubmitError::Device(dev_err("flip commit failed", &e))),
        }
    }

    fn destroy_fb(&self, fb: framebuffer::Handle) {
        // Best-effort teardown; a failure leaks one fb, not a frame.
        let _ = self.destroy_framebuffer(fb);
    }

    fn close_handle(&self, handle: DrmBufferHandle) {
        // Best-effort `GEM_CLOSE`; the kernel also frees it at fd-close, this
        // bounds the live handle count during a run.
        let _ = self.close_buffer(handle);
    }
}

impl DirectScanoutState {
    /// Destroy the currently-tracked direct fb and close every GEM handle that
    /// backed it, in the correct order: the framebuffer is removed **first**
    /// (it references the handles), then each handle is closed. Leaves the
    /// state empty.
    fn retire_current(&mut self, dev: &impl DirectScanoutDevice) {
        if let Some(fb) = self.fb.take() {
            dev.destroy_fb(fb);
        }
        for handle in self.handles.drain(..) {
            dev.close_handle(handle);
        }
    }

    /// Release all direct-scanout resources (teardown at sink stop). Identical
    /// to [`Self::retire_current`] — every imported handle is closed so a run
    /// nets to open/close parity rather than leaving the last frame's handles
    /// for the kernel to reap at fd-close.
    fn teardown(&mut self, dev: &impl DirectScanoutDevice) {
        self.retire_current(dev);
    }
}

/// Import `image`'s dmabuf planes into a fresh `ADDFB2` framebuffer over `dev`,
/// flip it onto `plane`, and manage the GEM-handle lifecycle in `state`.
///
/// On a successful (or `EBUSY`-conflated) flip the *new* fb becomes the tracked
/// one; the *previously* tracked fb is retired — destroyed and its handles
/// closed — so at most one direct fb (and its ≈2 handles) is ever live. On a
/// device error the freshly-built fb is retired immediately. Either way every
/// handle this import opens is eventually closed (no per-frame handle leak).
fn submit_direct_over<D: DirectScanoutDevice>(
    dev: &D,
    state: &mut DirectScanoutState,
    plane: plane::Handle,
    fb_id: property::Handle,
    image: &DmabufImage<'_>,
    format: DrmFormat,
    modifier: Option<u64>,
) -> Result<(), SubmitError> {
    let fourcc = DrmFourcc::try_from(format.fourcc()).map_err(|_| {
        SubmitError::Device(DisplayError::Device(format!(
            "unsupported direct-scanout fourcc {:#x}",
            format.fourcc()
        )))
    })?;
    if image.planes.is_empty() || image.planes.len() > 4 {
        return Err(SubmitError::Device(DisplayError::Device(format!(
            "direct-scanout image has {} planes (need 1..=4)",
            image.planes.len()
        ))));
    }
    let mut handles: [Option<DrmBufferHandle>; 4] = [None; 4];
    let mut pitches: [u32; 4] = [0; 4];
    let mut offsets: [u32; 4] = [0; 4];
    // Imported handles for *this* frame, tracked alongside the fb so they can
    // be closed when this fb is retired. Built up as each plane is imported so
    // an import failure part-way still closes what it opened.
    let mut new_handles: Vec<DrmBufferHandle> = Vec::with_capacity(image.planes.len());
    for (((slot, pitch), offset), src) in handles
        .iter_mut()
        .zip(pitches.iter_mut())
        .zip(offsets.iter_mut())
        .zip(image.planes.iter())
    {
        let handle = match dev.import_plane(src.fd) {
            Ok(handle) => handle,
            Err(e) => {
                // Close the handles opened so far before bailing — a partial
                // import must not leak either.
                for opened in new_handles.drain(..) {
                    dev.close_handle(opened);
                }
                return Err(SubmitError::Device(e));
            }
        };
        new_handles.push(handle);
        *slot = Some(handle);
        *pitch = src.pitch;
        *offset = src.offset;
    }
    let adapter = PlanarPrimeBuffer {
        size: (image.width, image.height),
        format: fourcc,
        modifier: modifier.map(DrmModifier::from),
        handles,
        pitches,
        offsets,
    };
    let flags = if modifier.is_some() {
        FbCmd2Flags::MODIFIERS
    } else {
        FbCmd2Flags::empty()
    };
    let new_fb = match dev.add_planar_fb(&adapter, flags) {
        Ok(fb) => fb,
        Err(e) => {
            // The fb add failed, so the imported handles are dangling — close
            // them before bailing.
            for opened in new_handles.drain(..) {
                dev.close_handle(opened);
            }
            return Err(SubmitError::Device(e));
        }
    };
    match dev.flip_to_fb(plane, fb_id, new_fb) {
        // Accepted (or EBUSY-conflated): `new_fb` is now (about to be) on glass.
        // Retire the *previously* tracked fb — fb destroyed, then its handles
        // closed — and adopt the new fb + its handles as the live ones.
        Ok(()) => {
            let mut retired = std::mem::replace(
                state,
                DirectScanoutState {
                    fb: Some(new_fb),
                    handles: new_handles,
                },
            );
            retired.retire_current(dev);
            Ok(())
        }
        Err(SubmitError::Busy) => {
            // EBUSY: the kernel still has the *previous* flip in flight, so the
            // previous fb stays on glass and keeps its handles. Retire just the
            // fb we built for this (conflated-away) frame.
            destroy_fresh(dev, new_fb, new_handles);
            Err(SubmitError::Busy)
        }
        Err(e) => {
            // Device error: nothing flipped — retire the just-built fb so it
            // does not leak. The previous tracked fb is untouched.
            destroy_fresh(dev, new_fb, new_handles);
            Err(e)
        }
    }
}

/// Retire a freshly-built-but-not-adopted direct fb: destroy the fb, then close
/// the handles it referenced (ordering: fb first, handles after).
fn destroy_fresh(
    dev: &impl DirectScanoutDevice,
    fb: framebuffer::Handle,
    handles: Vec<DrmBufferHandle>,
) {
    dev.destroy_fb(fb);
    for handle in handles {
        dev.close_handle(handle);
    }
}

/// Convert a kernel mode into the plain [`DisplayModeInfo`] the pure policy
/// layer consumes.
fn mode_to_info(mode: &Mode) -> DisplayModeInfo {
    let (width, height) = mode.size();
    let (hsync_start, hsync_end, htotal) = mode.hsync();
    let (vsync_start, vsync_end, vtotal) = mode.vsync();
    DisplayModeInfo {
        width: u32::from(width),
        height: u32::from(height),
        clock_khz: mode.clock(),
        hsync_start: u32::from(hsync_start),
        hsync_end: u32::from(hsync_end),
        htotal: u32::from(htotal),
        vsync_start: u32::from(vsync_start),
        vsync_end: u32::from(vsync_end),
        vtotal: u32::from(vtotal),
        hsync_positive: mode.flags().contains(ModeFlags::PHSYNC),
        vsync_positive: mode.flags().contains(ModeFlags::PVSYNC),
        preferred: mode.mode_type().contains(ModeTypeFlags::PREFERRED),
    }
}

/// Build a user-defined kernel mode from computed (CVT-RB forced) timings.
fn native_mode_from_info(info: &DisplayModeInfo) -> Result<Mode, DisplayError> {
    let geometry_err =
        |what: &str| DisplayError::Device(format!("forced mode {what} exceeds the KMS u16 range"));
    let mut raw = drm_ffi::drm_mode_modeinfo {
        clock: info.clock_khz,
        hdisplay: u16::try_from(info.width).map_err(|_| geometry_err("width"))?,
        hsync_start: u16::try_from(info.hsync_start).map_err(|_| geometry_err("hsync_start"))?,
        hsync_end: u16::try_from(info.hsync_end).map_err(|_| geometry_err("hsync_end"))?,
        htotal: u16::try_from(info.htotal).map_err(|_| geometry_err("htotal"))?,
        vdisplay: u16::try_from(info.height).map_err(|_| geometry_err("height"))?,
        vsync_start: u16::try_from(info.vsync_start).map_err(|_| geometry_err("vsync_start"))?,
        vsync_end: u16::try_from(info.vsync_end).map_err(|_| geometry_err("vsync_end"))?,
        vtotal: u16::try_from(info.vtotal).map_err(|_| geometry_err("vtotal"))?,
        type_: drm_ffi::DRM_MODE_TYPE_USERDEF,
        ..Default::default()
    };
    raw.flags = if info.hsync_positive {
        drm_ffi::DRM_MODE_FLAG_PHSYNC
    } else {
        drm_ffi::DRM_MODE_FLAG_NHSYNC
    } | if info.vsync_positive {
        drm_ffi::DRM_MODE_FLAG_PVSYNC
    } else {
        drm_ffi::DRM_MODE_FLAG_NVSYNC
    };
    // The cosmetic integer refresh (the kernel recomputes from the timings).
    let denom = u64::from(info.htotal).saturating_mul(u64::from(info.vtotal));
    raw.vrefresh = u64::from(info.clock_khz)
        .saturating_mul(1000)
        .checked_div(denom)
        .and_then(|hz| u32::try_from(hz).ok())
        .unwrap_or(0);
    Ok(Mode::from(raw))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::cell::Cell;
    use std::os::fd::AsFd;

    use drm::control::from_u32;

    use super::super::canvas::DmabufPlane;
    use super::*;

    /// A hardware-free [`DirectScanoutDevice`] that counts GEM-handle imports
    /// (`prime_fd_to_buffer`) vs closes (`GEM_CLOSE`) and lets the test script
    /// each flip's outcome — the seam through which the per-frame handle leak
    /// becomes observable without a GPU.
    struct CountingDevice {
        /// Total `import_plane` calls (handles opened).
        opens: Cell<u32>,
        /// Total `close_handle` calls (handles closed).
        closes: Cell<u32>,
        /// Total `add_planar_fb` calls (framebuffers created).
        fbs_added: Cell<u32>,
        /// Total `destroy_fb` calls (framebuffers destroyed).
        fbs_destroyed: Cell<u32>,
        /// Next raw handle id to hand out (monotonic, non-zero).
        next_handle: Cell<u32>,
        /// Next raw framebuffer id to hand out (monotonic, non-zero).
        next_fb: Cell<u32>,
        /// Scripted flip outcomes, consumed front-to-back; exhausted ⇒ `Ok`.
        flip_script: Cell<usize>,
        flips: Vec<FlipOutcome>,
    }

    #[derive(Clone, Copy)]
    enum FlipOutcome {
        Ok,
        Busy,
        Device,
    }

    impl CountingDevice {
        fn new(flips: Vec<FlipOutcome>) -> Self {
            Self {
                opens: Cell::new(0),
                closes: Cell::new(0),
                fbs_added: Cell::new(0),
                fbs_destroyed: Cell::new(0),
                next_handle: Cell::new(1),
                next_fb: Cell::new(1),
                flip_script: Cell::new(0),
                flips,
            }
        }

        fn live_handles(&self) -> i64 {
            i64::from(self.opens.get()) - i64::from(self.closes.get())
        }

        fn live_fbs(&self) -> i64 {
            i64::from(self.fbs_added.get()) - i64::from(self.fbs_destroyed.get())
        }
    }

    impl DirectScanoutDevice for CountingDevice {
        fn import_plane(&self, _fd: BorrowedFd<'_>) -> Result<DrmBufferHandle, DisplayError> {
            let raw = self.next_handle.get();
            self.next_handle.set(raw + 1);
            self.opens.set(self.opens.get() + 1);
            Ok(from_u32(raw).expect("non-zero handle"))
        }

        fn add_planar_fb(
            &self,
            _buffer: &PlanarPrimeBuffer,
            _flags: FbCmd2Flags,
        ) -> Result<framebuffer::Handle, DisplayError> {
            let raw = self.next_fb.get();
            self.next_fb.set(raw + 1);
            self.fbs_added.set(self.fbs_added.get() + 1);
            Ok(from_u32(raw).expect("non-zero fb"))
        }

        fn flip_to_fb(
            &self,
            _plane: plane::Handle,
            _fb_id: property::Handle,
            _fb: framebuffer::Handle,
        ) -> Result<(), SubmitError> {
            let idx = self.flip_script.get();
            self.flip_script.set(idx + 1);
            match self.flips.get(idx).copied().unwrap_or(FlipOutcome::Ok) {
                FlipOutcome::Ok => Ok(()),
                FlipOutcome::Busy => Err(SubmitError::Busy),
                FlipOutcome::Device => Err(SubmitError::Device(DisplayError::Device(
                    "scripted".to_owned(),
                ))),
            }
        }

        fn destroy_fb(&self, _fb: framebuffer::Handle) {
            self.fbs_destroyed.set(self.fbs_destroyed.get() + 1);
        }

        fn close_handle(&self, _handle: DrmBufferHandle) {
            self.closes.set(self.closes.get() + 1);
        }
    }

    /// A 2-plane (NV12-shaped) dmabuf image borrowing one real fd — enough to
    /// drive the import/flip lifecycle; the device mock ignores the fd contents.
    fn nv12_image(fd: BorrowedFd<'_>) -> DmabufImage<'_> {
        DmabufImage {
            format: DrmFormat::NV12,
            modifier: None,
            width: 4,
            height: 4,
            planes: vec![
                DmabufPlane {
                    fd,
                    offset: 0,
                    pitch: 4,
                },
                DmabufPlane {
                    fd,
                    offset: 16,
                    pitch: 4,
                },
            ],
        }
    }

    fn dummy_plane() -> plane::Handle {
        from_u32(1).expect("non-zero plane")
    }

    fn dummy_fb_id() -> property::Handle {
        from_u32(1).expect("non-zero property")
    }

    /// Drive N successful direct-scanout frames, then tear down: every GEM
    /// handle opened (2/frame for NV12) must be closed — open/close parity over
    /// the whole run, and exactly one fb + its handles live mid-run (bounded).
    #[test]
    fn direct_scanout_closes_every_imported_handle_over_n_frames() {
        const N: usize = 64;
        let stdin = std::io::stdin();
        let fd = stdin.as_fd();
        let dev = CountingDevice::new(vec![FlipOutcome::Ok; N]);
        let mut state = DirectScanoutState::default();

        for _ in 0..N {
            submit_direct_over(
                &dev,
                &mut state,
                dummy_plane(),
                dummy_fb_id(),
                &nv12_image(fd),
                DrmFormat::NV12,
                None,
            )
            .expect("scripted flip succeeds");
            // At most one direct fb (and its 2 handles) is ever live: each new
            // frame retires the previous one. This bounds the leak the fix
            // closes — never more than a single frame's worth in flight.
            assert!(
                dev.live_fbs() <= 1,
                "at most one direct fb live mid-run, saw {}",
                dev.live_fbs()
            );
            assert!(
                dev.live_handles() <= 2,
                "at most one frame's handles live mid-run, saw {}",
                dev.live_handles()
            );
        }

        // Teardown closes the final frame's still-on-glass handles + fb.
        state.teardown(&dev);

        assert_eq!(
            usize::try_from(dev.opens.get()).expect("fits usize"),
            N * 2,
            "two GEM handles imported per NV12 frame"
        );
        assert_eq!(
            dev.opens.get(),
            dev.closes.get(),
            "every imported GEM handle must be closed (open/close parity over {N} frames): \
             opened {}, closed {}",
            dev.opens.get(),
            dev.closes.get()
        );
        assert_eq!(
            dev.fbs_added.get(),
            dev.fbs_destroyed.get(),
            "every direct framebuffer must be destroyed"
        );
        assert_eq!(dev.live_handles(), 0, "no GEM handle leaked after teardown");
    }

    /// A flip the kernel rejects with a device error retires the freshly-built
    /// fb AND closes its just-imported handles — a rejected frame must not leak.
    #[test]
    fn direct_scanout_rejected_fb_closes_its_handles() {
        let stdin = std::io::stdin();
        let fd = stdin.as_fd();
        let dev = CountingDevice::new(vec![FlipOutcome::Device]);
        let mut state = DirectScanoutState::default();

        let err = submit_direct_over(
            &dev,
            &mut state,
            dummy_plane(),
            dummy_fb_id(),
            &nv12_image(fd),
            DrmFormat::NV12,
            None,
        )
        .expect_err("scripted device error");
        assert!(matches!(err, SubmitError::Device(_)));

        // The rejected fb was created then destroyed, and both imported handles
        // were closed — nothing of the rejected frame survives.
        assert_eq!(dev.opens.get(), 2, "two handles imported for the frame");
        assert_eq!(dev.closes.get(), 2, "both handles closed after rejection");
        assert_eq!(dev.fbs_added.get(), 1);
        assert_eq!(dev.fbs_destroyed.get(), 1, "rejected fb destroyed");
        assert_eq!(
            dev.live_handles(),
            0,
            "no handle leaked by a rejected frame"
        );
        assert!(
            state.handles.is_empty(),
            "no rejected handle tracked as live"
        );
        assert!(state.fb.is_none(), "no rejected fb tracked as live");
    }

    /// An EBUSY (conflation) flip keeps the *previous* fb on glass and closes
    /// only the conflated-away frame's resources — the on-glass fb's handles
    /// are never closed while it is still scanned out.
    #[test]
    fn direct_scanout_ebusy_keeps_previous_fb_and_closes_only_the_conflated_frame() {
        let stdin = std::io::stdin();
        let fd = stdin.as_fd();
        // Frame 1 flips Ok (becomes on-glass); frame 2 is EBUSY (conflated).
        let dev = CountingDevice::new(vec![FlipOutcome::Ok, FlipOutcome::Busy]);
        let mut state = DirectScanoutState::default();

        submit_direct_over(
            &dev,
            &mut state,
            dummy_plane(),
            dummy_fb_id(),
            &nv12_image(fd),
            DrmFormat::NV12,
            None,
        )
        .expect("frame 1 flips");
        let on_glass_fb = state.fb;
        assert!(on_glass_fb.is_some(), "frame 1 is on glass");

        let busy = submit_direct_over(
            &dev,
            &mut state,
            dummy_plane(),
            dummy_fb_id(),
            &nv12_image(fd),
            DrmFormat::NV12,
            None,
        )
        .expect_err("frame 2 EBUSY");
        assert!(matches!(busy, SubmitError::Busy));

        // The on-glass fb (frame 1) is unchanged and STILL live — its handles
        // were not closed. Only frame 2's fb + handles were retired.
        assert_eq!(state.fb, on_glass_fb, "frame 1 stays on glass under EBUSY");
        assert_eq!(state.handles.len(), 2, "frame 1's handles stay live");
        assert_eq!(dev.opens.get(), 4, "two frames imported (2 handles each)");
        assert_eq!(
            dev.closes.get(),
            2,
            "only the conflated frame 2's handles closed; frame 1 stays on glass"
        );
        assert_eq!(
            dev.live_handles(),
            2,
            "exactly frame 1's handles remain live"
        );

        // Teardown drains frame 1 too → full parity.
        state.teardown(&dev);
        assert_eq!(dev.opens.get(), dev.closes.get(), "parity after teardown");
        assert_eq!(dev.live_handles(), 0);
    }
}
