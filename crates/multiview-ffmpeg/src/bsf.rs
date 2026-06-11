//! In-band parameter-set / Annex-B framing **bitstream-filter stage** (GP-3,
//! ADR-0030 §4 "Framing prerequisite"). The FFI half of GP-3.
//!
//! A guarded passthrough splices a pre-baked slate into a *copied* elementary
//! stream. For a continuous-ES splice on MPEG-TS / SRT / RTSP-RTP / raw-Annex-B
//! targets the active SPS/PPS (H.264) or VPS/SPS/PPS (HEVC) **must** be repeated
//! **in-band** immediately before **both** the slate IDR and the recovery IDR;
//! `h264_mp4toannexb` alone inserts the parameter sets only at stream-start /
//! on an extradata change, which is insufficient for a mid-stream splice. This
//! module guarantees the in-band repeat + identical framing on the copied-input
//! side and the slate side via a safe RAII wrapper over the libav bitstream
//! filters `dump_extra` / `extract_extradata` / `h264|hevc_mp4toannexb`.
//!
//! The pure [`crate::bsf_select`] module decides *which* filters and in what
//! order (so the selection is unit-testable without libav); this module
//! instantiates that plan as a [`BsfChain`] of [`BitstreamFilter`] stages and
//! pumps packets through it.
//!
//! ## FFI ownership (CLAUDE.md §7)
//!
//! `ffmpeg-next` 8.1 exposes **no** safe wrapper for the `av_bsf_*` API, and the
//! `av_bsf_*` C symbols are **not** in `ffmpeg-sys-next`'s generated bindings
//! (that crate wraps `libavcodec/avcodec.h`, which since FFmpeg 5 no longer
//! includes `libavcodec/bsf.h`). So this module declares the small, ABI-stable
//! `av_bsf_*` surface and the two `AVBSFContext` fields it touches itself, in an
//! `extern "C"` block linked against the already-linked `libavcodec`. The crate
//! stays `unsafe_code = deny`: every `unsafe` block carries a `// SAFETY:` note,
//! the `AVBSFContext` is freed in `Drop` (RAII), and no Rust panic crosses the
//! FFI boundary.

use ffmpeg_next as ffmpeg;

use multiview_core::time::Rational;

use crate::bsf_select::{
    needs_keyframe_freq_option, plan_bsf_chain, BsfFraming, InputFraming, DUMP_EXTRA_FREQ_KEYFRAMES,
};
use crate::convert::to_ff_rational;
use crate::decode::ensure_initialized;
use crate::error::Result;
use crate::idr::CodecKind;

/// One filtered access unit emitted by a [`BsfChain`].
///
/// Wraps an owned `AVPacket`; its coded bytes are the chain's normalised output
/// (Annex-B framed with the parameter sets repeated in-band, per the chain's
/// [`BsfFraming`]). The wrapper keeps the public API free of raw libav types.
pub struct FilteredPacket {
    packet: ffmpeg::codec::packet::Packet,
}

impl FilteredPacket {
    /// The filtered coded payload, or `None` for an empty packet.
    #[must_use]
    pub fn data(&self) -> Option<&[u8]> {
        self.packet.data()
    }

    /// The payload length in bytes (`0` for an empty packet).
    #[must_use]
    pub fn len(&self) -> usize {
        self.packet.size()
    }

    /// Whether the filtered packet carries no payload.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packet.size() == 0
    }

    /// The presentation timestamp the filter carried through, if any (in the
    /// input stream time-base — the chain re-stamps nothing).
    #[must_use]
    pub fn pts(&self) -> Option<i64> {
        self.packet.pts()
    }

    /// The decode timestamp the filter carried through, if any.
    #[must_use]
    pub fn dts(&self) -> Option<i64> {
        self.packet.dts()
    }

    /// Whether the filtered packet is flagged as a keyframe.
    #[must_use]
    pub fn is_key(&self) -> bool {
        self.packet.is_key()
    }

    /// Consume the wrapper, yielding the owned packet (for forwarding to a
    /// muxer / slate ring without an extra copy).
    #[must_use]
    pub fn into_packet(self) -> ffmpeg::codec::packet::Packet {
        self.packet
    }
}

/// A single safe bitstream-filter stage (one `AVBSFContext`).
///
/// Built from a filter name + the input codec parameters + input time-base, it
/// accepts coded packets and yields filtered packets. The whole `av_bsf_*`
/// lifecycle (`get_by_name` → `alloc` → set `par_in`/`time_base_in` → `init` →
/// `send`/`receive` → `free`) is owned here; the context is freed in [`Drop`].
///
/// `!Sync` by construction (a libav filter context needs external
/// synchronisation for shared access); `!Send` too — the egress thread that owns
/// the chain drives it serially.
pub struct BitstreamFilter {
    /// The owned `AVBSFContext`. Non-null between construction and `Drop`.
    ctx: ffi_bsf::BsfPtr,
}

impl BitstreamFilter {
    /// Build and initialise one bitstream filter named `filter_name`, seeded with
    /// `params` as `par_in` and `time_base` as `time_base_in`.
    ///
    /// `dump_extra` additionally gets its `freq` option set to keyframes-only so
    /// it repeats the parameter sets before **every** keyframe (the GP-3
    /// guarantee), per [`crate::bsf_select::DUMP_EXTRA_FREQ_KEYFRAMES`].
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::CodecNotFound`] — the named filter is not in this build.
    /// * [`FfmpegError::Bsf`] — alloc / option-set / init failed.
    fn new(
        filter_name: &str,
        params: &ffmpeg::codec::Parameters,
        time_base: Rational,
    ) -> Result<Self> {
        ensure_initialized()?;
        let tb = to_ff_rational(time_base)?;
        let set_keyframe_freq = needs_keyframe_freq_option(filter_name);
        let ctx = ffi_bsf::alloc_and_init(filter_name, params, tb.into(), set_keyframe_freq)?;
        Ok(Self { ctx })
    }

    /// Submit one packet to the filter (its bytes are copied/owned by libav).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Bsf`] on a libav send error other than the
    /// `EAGAIN` "drain first" signal (which is surfaced as an error here because
    /// the [`BsfChain`] drains fully between sends).
    fn send(&mut self, packet: &ffmpeg::codec::packet::Packet) -> Result<()> {
        ffi_bsf::send_packet(&self.ctx, Some(packet))
    }

    /// Signal end-of-stream so the filter can flush any trailing packets.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Bsf`] on a libav error.
    fn send_eof(&mut self) -> Result<()> {
        ffi_bsf::send_packet(&self.ctx, None)
    }

    /// Pull the next filtered packet, or `Ok(None)` on `EAGAIN` / `EOF` (the
    /// filter needs more input, or the stream is fully drained).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Bsf`] for a real libav error.
    fn receive(&mut self) -> Result<Option<ffmpeg::codec::packet::Packet>> {
        ffi_bsf::receive_packet(&self.ctx)
    }

    /// This stage's **output** codec parameters + time-base, to seed the next
    /// chain stage.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Bsf`] if the parameters cannot be copied out.
    fn out_params(&self) -> Result<(ffmpeg::codec::Parameters, Rational)> {
        ffi_bsf::out_params(&self.ctx)
    }
}

impl Drop for BitstreamFilter {
    fn drop(&mut self) {
        // SAFETY: `self.ctx` owns the AVBSFContext; `av_bsf_free` frees it and
        // nulls the pointer. Idempotent against a null pointer.
        ffi_bsf::free(&mut self.ctx);
    }
}

/// An ordered chain of [`BitstreamFilter`] stages (GP-3).
///
/// Composes the libav bitstream filters [`crate::bsf_select::plan_bsf_chain`]
/// selects for `(codec, input framing, desired framing)` and pumps packets
/// through them end-to-end: a packet sent to the chain flows through every
/// stage, and [`receive_packet`](BsfChain::receive_packet) yields the chain's
/// final output. An empty plan (AV1 / unmodelled codec) passes packets through
/// untouched.
///
/// `!Send`/`!Sync`: the egress thread owns and drives it serially.
pub struct BsfChain {
    /// The filter stages, in application order. Empty ⇒ pass-through.
    stages: Vec<BitstreamFilter>,
    /// Packets that have cleared the **last** stage, waiting to be received.
    out_queue: std::collections::VecDeque<ffmpeg::codec::packet::Packet>,
}

impl BsfChain {
    /// Build the GP-3 chain for `(codec, input, desired)` from a stream's codec
    /// `params` and input `time_base`.
    ///
    /// Each stage after the first is seeded with the **previous** stage's output
    /// codec parameters / time-base (`par_out` / `time_base_out`), so a
    /// converter that rewrites the framing hands the next stage the correct
    /// parameters — exactly libav's own chained-BSF contract.
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::CodecNotFound`] — a named filter is missing.
    /// * [`FfmpegError::Bsf`] — a stage failed to alloc / configure / init.
    pub fn new(
        codec: CodecKind,
        input: InputFraming,
        desired: BsfFraming,
        params: &ffmpeg::codec::Parameters,
        time_base: Rational,
    ) -> Result<Self> {
        ensure_initialized()?;
        let plan = plan_bsf_chain(codec, input, desired);

        let mut stages: Vec<BitstreamFilter> = Vec::with_capacity(plan.len());
        // The first stage takes the stream's params/time-base; each subsequent
        // stage takes the prior stage's output params/time-base.
        let mut cur_params = params.clone();
        let mut cur_tb = time_base;
        for name in plan.names() {
            let stage = BitstreamFilter::new(name, &cur_params, cur_tb)?;
            // Read this stage's output params/time-base to seed the next one.
            (cur_params, cur_tb) = stage.out_params()?;
            stages.push(stage);
        }

        Ok(Self {
            stages,
            out_queue: std::collections::VecDeque::new(),
        })
    }

    /// Submit one coded packet to the chain. Drain with
    /// [`receive_packet`](BsfChain::receive_packet) until it returns `None`.
    ///
    /// With an empty chain the packet is queued for output unchanged.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Bsf`] on a libav filter error.
    pub fn send_packet(&mut self, packet: &ffmpeg::codec::packet::Packet) -> Result<()> {
        if self.stages.is_empty() {
            // Pass-through: clone the packet into the output queue (an owned
            // ref-counted copy, independent of the caller's packet).
            self.out_queue.push_back(packet.clone());
            return Ok(());
        }
        self.feed_from_first(packet)
    }

    /// Signal end-of-stream so each stage flushes trailing packets.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Bsf`] on a libav filter error.
    pub fn send_eof(&mut self) -> Result<()> {
        if self.stages.is_empty() {
            return Ok(());
        }
        // EOF the first stage, then cascade drained packets through the rest.
        if let Some(first) = self.stages.first_mut() {
            first.send_eof()?;
        }
        self.cascade_from(0)
    }

    /// Pull the next fully-filtered packet, or `Ok(None)` when the chain has no
    /// output ready (it needs more input or is drained).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Bsf`] on a libav filter error.
    pub fn receive_packet(&mut self) -> Result<Option<FilteredPacket>> {
        Ok(self
            .out_queue
            .pop_front()
            .map(|packet| FilteredPacket { packet }))
    }

    /// Feed one packet into stage 0, then cascade its output through every
    /// later stage, finally queueing what clears the last stage.
    fn feed_from_first(&mut self, packet: &ffmpeg::codec::packet::Packet) -> Result<()> {
        if let Some(first) = self.stages.first_mut() {
            first.send(packet)?;
        }
        self.cascade_from(0)
    }

    /// Drain stage `from`, push each output into stage `from+1` (cascading), and
    /// queue whatever clears the **last** stage onto `out_queue`.
    fn cascade_from(&mut self, from: usize) -> Result<()> {
        let last = self.stages.len().saturating_sub(1);
        // Outputs leaving stage `idx`, to be pushed into stage `idx+1`.
        let mut pending: Vec<ffmpeg::codec::packet::Packet> = Vec::new();

        // Drain the starting stage.
        if let Some(stage) = self.stages.get_mut(from) {
            while let Some(out) = stage.receive()? {
                pending.push(out);
            }
        }

        let mut idx = from;
        while idx < last {
            let next = idx + 1;
            let feed = std::mem::take(&mut pending);
            for pkt in feed {
                if let Some(stage) = self.stages.get_mut(next) {
                    stage.send(&pkt)?;
                }
            }
            if let Some(stage) = self.stages.get_mut(next) {
                while let Some(out) = stage.receive()? {
                    pending.push(out);
                }
            }
            idx = next;
        }

        // Whatever cleared the last stage is chain output.
        for pkt in pending {
            self.out_queue.push_back(pkt);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FFI: the `av_bsf_*` surface (ADR-0030 GP-3 framing prerequisite).
//
// reason: `ffmpeg-next` 8.1 exposes no BSF wrapper, and `av_bsf_*` is absent
// from `ffmpeg-sys-next`'s bindings (it wraps `avcodec.h`, which no longer
// includes `bsf.h`). The small, ABI-stable surface below is declared and driven
// directly against the already-linked `libavcodec`. The crate is
// `unsafe_code = deny`; every `unsafe` operation carries a `// SAFETY:` note and
// is bounded to libav objects we allocate/own. No Rust panic crosses the FFI
// boundary, and the AVBSFContext is freed via RAII `Drop`.
#[allow(unsafe_code)]
mod ffi_bsf {
    use std::ffi::CString;
    use std::ptr;

    use ffmpeg::codec::packet::{Mut, Ref};
    use ffmpeg::ffi;
    use ffmpeg_next as ffmpeg;
    use libc::{c_char, c_int, c_void};

    use multiview_core::time::Rational;

    use crate::convert::from_ff_rational;
    use crate::error::{FfmpegError, Result};

    /// Opaque `AVBitStreamFilter` — we only ever hold a pointer to it.
    #[repr(C)]
    struct AVBitStreamFilter {
        _private: [u8; 0],
    }

    /// The prefix of `AVBSFContext` we read/write (CLAUDE.md §7: only the fields
    /// GP-3 touches, laid out exactly as `libavcodec/bsf.h` declares them so the
    /// field offsets match the ABI). Everything after `time_base_out` is libav's
    /// private business; we never index past it.
    #[repr(C)]
    struct AVBSFContext {
        av_class: *const c_void,
        filter: *const AVBitStreamFilter,
        priv_data: *mut c_void,
        par_in: *mut ffi::AVCodecParameters,
        par_out: *mut ffi::AVCodecParameters,
        time_base_in: ffi::AVRational,
        time_base_out: ffi::AVRational,
    }

    // The ABI-stable `av_bsf_*` surface, linked against `libavcodec`.
    extern "C" {
        fn av_bsf_get_by_name(name: *const c_char) -> *const AVBitStreamFilter;
        fn av_bsf_alloc(filter: *const AVBitStreamFilter, ctx: *mut *mut AVBSFContext) -> c_int;
        fn av_bsf_init(ctx: *mut AVBSFContext) -> c_int;
        fn av_bsf_send_packet(ctx: *mut AVBSFContext, pkt: *mut ffi::AVPacket) -> c_int;
        fn av_bsf_receive_packet(ctx: *mut AVBSFContext, pkt: *mut ffi::AVPacket) -> c_int;
        fn av_bsf_free(ctx: *mut *mut AVBSFContext);
    }

    /// An owned `AVBSFContext` pointer (RAII via [`super::BitstreamFilter`]'s
    /// `Drop`). `null` after [`free`].
    pub(super) struct BsfPtr(*mut AVBSFContext);

    impl BsfPtr {
        /// The raw context pointer (internal to this FFI module only).
        fn raw(&self) -> *mut AVBSFContext {
            self.0
        }
    }

    /// `av_bsf_get_by_name` → `av_bsf_alloc` → fill `par_in`/`time_base_in`
    /// (+ optional `freq=keyframes` on `dump_extra`) → `av_bsf_init`.
    pub(super) fn alloc_and_init(
        filter_name: &str,
        params: &ffmpeg::codec::Parameters,
        time_base_in: ffi::AVRational,
        set_keyframe_freq: bool,
    ) -> Result<BsfPtr> {
        let cname = CString::new(filter_name).map_err(|_| bsf_err("filter name has a NUL"))?;
        // SAFETY: `cname` is a valid NUL-terminated string; `av_bsf_get_by_name`
        // returns a borrowed static filter pointer or null (unknown filter).
        let filter = unsafe { av_bsf_get_by_name(cname.as_ptr()) };
        if filter.is_null() {
            return Err(FfmpegError::CodecNotFound(static_filter_name(filter_name)));
        }

        let mut ctx: *mut AVBSFContext = ptr::null_mut();
        // SAFETY: `filter` is a valid filter; `&raw mut ctx` receives a freshly
        // allocated context (or stays null on error). We own it from here.
        let rc = unsafe { av_bsf_alloc(filter, &raw mut ctx) };
        if rc < 0 || ctx.is_null() {
            return Err(bsf_err_code("av_bsf_alloc", rc));
        }
        let mut owned = BsfPtr(ctx);

        // Copy the input codec parameters into `par_in` and set `time_base_in`.
        // SAFETY: `owned` is a freshly-allocated context whose `par_in` libav
        // allocated; `params.as_ptr()` is a valid AVCodecParameters we only read.
        // `avcodec_parameters_copy` overwrites the destination in place. `owned`
        // is freed explicitly on every error path below.
        let copy_rc = unsafe {
            let ctx_ref = &mut *owned.raw();
            ffi::avcodec_parameters_copy(ctx_ref.par_in, params.as_ptr())
        };
        if copy_rc < 0 {
            free(&mut owned);
            return Err(bsf_err_code("avcodec_parameters_copy(par_in)", copy_rc));
        }
        // SAFETY: `owned` is live; `time_base_in` is a plain POD field.
        unsafe {
            (*owned.raw()).time_base_in = time_base_in;
        }

        if set_keyframe_freq {
            if let Err(e) = set_dump_extra_freq(&owned) {
                free(&mut owned);
                return Err(e);
            }
        }

        // SAFETY: `owned` is allocated and its `par_in`/`time_base_in` are set;
        // `av_bsf_init` prepares it for filtering and sets `par_out`/`time_base_out`.
        let init_rc = unsafe { av_bsf_init(owned.raw()) };
        if init_rc < 0 {
            free(&mut owned);
            return Err(bsf_err_code("av_bsf_init", init_rc));
        }
        Ok(owned)
    }

    /// Set `dump_extra`'s `freq` option to `keyframes` via `av_opt_set` on the
    /// filter's `priv_data` (the AVOptions-enabled private struct).
    fn set_dump_extra_freq(ctx: &BsfPtr) -> Result<()> {
        let key = CString::new("freq").map_err(|_| bsf_err("opt key NUL"))?;
        let val = CString::new(DUMP_EXTRA_FREQ_KEYFRAMES).map_err(|_| bsf_err("opt val NUL"))?;
        // SAFETY: `ctx` is a freshly-allocated (pre-init) AVBSFContext;
        // `priv_data` is its AVOptions-enabled private struct (non-null for
        // dump_extra, which has a priv_class). `av_opt_set` only reads the two
        // CStrings. A null priv_data would make av_opt_set return an error we map.
        let priv_data = unsafe { (*ctx.raw()).priv_data };
        if priv_data.is_null() {
            return Err(bsf_err("dump_extra has no priv_data for the freq option"));
        }
        // SAFETY: `priv_data` is non-null and AVOptions-enabled; key/val are
        // valid NUL-terminated strings; search_flags 0.
        let rc = unsafe { ffi::av_opt_set(priv_data, key.as_ptr(), val.as_ptr(), 0) };
        if rc < 0 {
            return Err(bsf_err_code("av_opt_set(freq=keyframes)", rc));
        }
        Ok(())
    }

    const DUMP_EXTRA_FREQ_KEYFRAMES: &str = super::DUMP_EXTRA_FREQ_KEYFRAMES;

    /// Read a stage's **output** codec parameters + time-base (`par_out` /
    /// `time_base_out`), as an owned snapshot to seed the next chain stage.
    pub(super) fn out_params(ctx: &BsfPtr) -> Result<(ffmpeg::codec::Parameters, Rational)> {
        // Allocate a fresh, owner-less AVCodecParameters and copy par_out into it.
        let mut params = ffmpeg::codec::Parameters::new();
        // SAFETY: `ctx` is a live initialised context; `par_out` is libav-owned
        // and valid post-init; `params.as_mut_ptr()` is our fresh allocation.
        let rc = unsafe {
            let par_out = (*ctx.raw()).par_out;
            ffi::avcodec_parameters_copy(params.as_mut_ptr(), par_out)
        };
        if rc < 0 {
            return Err(bsf_err_code("avcodec_parameters_copy(par_out)", rc));
        }
        // SAFETY: `ctx` is live; `time_base_out` is a plain POD field set by init.
        let tb = unsafe { (*ctx.raw()).time_base_out };
        Ok((params, from_ff_rational(tb.into())))
    }

    /// `av_bsf_send_packet`, with `None` meaning the EOF flush (NULL packet).
    pub(super) fn send_packet(
        ctx: &BsfPtr,
        packet: Option<&ffmpeg::codec::packet::Packet>,
    ) -> Result<()> {
        // SAFETY: `ctx` is a live initialised context. For a real packet we hand
        // libav the AVPacket pointer (it refs the buffer, leaving our `Packet`
        // owned + Drop-freed); a null pointer signals EOF. We cast away const for
        // the C signature but libav does not mutate a sent packet's payload.
        let rc = unsafe {
            match packet {
                Some(p) => av_bsf_send_packet(ctx.raw(), p.as_ptr().cast_mut()),
                None => av_bsf_send_packet(ctx.raw(), ptr::null_mut()),
            }
        };
        if rc < 0 {
            return Err(bsf_err_code("av_bsf_send_packet", rc));
        }
        Ok(())
    }

    /// `av_bsf_receive_packet`, mapping `EAGAIN`/`EOF` to `Ok(None)`.
    pub(super) fn receive_packet(ctx: &BsfPtr) -> Result<Option<ffmpeg::codec::packet::Packet>> {
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        // SAFETY: `ctx` is a live initialised context; `packet.as_mut_ptr()` is a
        // valid AVPacket libav fills (and which we own + Drop-free). On EAGAIN/EOF
        // libav leaves the packet empty.
        let rc = unsafe { av_bsf_receive_packet(ctx.raw(), packet.as_mut_ptr()) };
        if rc == 0 {
            return Ok(Some(packet));
        }
        // SAFETY: comparing against the libav AVERROR(EAGAIN)/EOF sentinels.
        let eagain = ffi::AVERROR(libc::EAGAIN);
        let eof = ffi::AVERROR_EOF;
        if rc == eagain || rc == eof {
            Ok(None)
        } else {
            Err(bsf_err_code("av_bsf_receive_packet", rc))
        }
    }

    /// Free the `AVBSFContext` and null the pointer (RAII / idempotent).
    pub(super) fn free(ctx: &mut BsfPtr) {
        if ctx.0.is_null() {
            return;
        }
        // SAFETY: `ctx.0` is our owned context (or null, handled above);
        // `av_bsf_free` frees it and nulls the pointer we pass.
        unsafe {
            av_bsf_free(&raw mut ctx.0);
        }
    }

    /// An `FfmpegError::Bsf` for a setup failure with a static reason.
    fn bsf_err(reason: &'static str) -> FfmpegError {
        FfmpegError::Bsf {
            op: reason,
            code: 0,
        }
    }

    /// An `FfmpegError::Bsf` carrying the libav return code of a failed op.
    fn bsf_err_code(op: &'static str, code: c_int) -> FfmpegError {
        FfmpegError::Bsf {
            op,
            code: i64::from(code),
        }
    }

    /// Map a runtime filter name to a `'static` label for the typed
    /// `CodecNotFound` message.
    fn static_filter_name(name: &str) -> &'static str {
        match name {
            "dump_extra" => "dump_extra",
            "extract_extradata" => "extract_extradata",
            "h264_mp4toannexb" => "h264_mp4toannexb",
            "hevc_mp4toannexb" => "hevc_mp4toannexb",
            _ => "<bsf>",
        }
    }
}
