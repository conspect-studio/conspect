//! Safe muxer / output-container wrapper (the `ffmpeg` feature).
//!
//! [`Muxer`] allocates an output `AVFormatContext` for a container (mkv / mp4 /
//! mpegts), registers one stream per encoder, writes the header, accepts
//! encoded packets (rescaling their timestamps from encoder time-base into the
//! stream time-base and interleaving them), and writes the trailer. It owns the
//! context and frees it in `Drop` (via `ffmpeg_next`).
//!
//! ## Re-stamping discipline (invariants #1/#3)
//! Packets arrive stamped in the **encoder** time-base — which the encoder
//! derived from the output clock's tick counter, never from raw input PTS. The
//! muxer's only timestamp job is the mechanical encoder→stream time-base
//! rescale (libav's `av_packet_rescale_ts`); it never invents or trusts an
//! input PTS.
//! Callers must finish with [`Muxer::finish`] (writes the trailer) — dropping
//! without it leaves a container missing its trailer, so `finish` is explicit.

use std::path::Path;

use ffmpeg::{codec, format, Dictionary};
use ffmpeg_next as ffmpeg;

use multiview_core::time::Rational;

use crate::convert::{from_ff_rational, to_ff_rational};
use crate::decode::ensure_initialized;
use crate::error::{FfmpegError, Result};
use crate::mux_options::MuxOptions;

/// The typed error for a metadata key/value carrying an interior NUL byte: it
/// could never become a C string for `av_dict_set`. Mapped onto the muxer's
/// `InvalidData` (a contradictory request, surfaced not swallowed).
fn nul_err() -> FfmpegError {
    FfmpegError::Mux(ffmpeg::Error::InvalidData)
}

/// A registered output stream: its index and the encoder time-base whose
/// packets feed it (the source side of the rescale into stream time-base).
#[derive(Debug, Clone, Copy)]
struct StreamInfo {
    index: usize,
    encoder_time_base: Rational,
    stream_time_base: Rational,
}

/// A safe container muxer.
///
/// `!Sync` by construction; `Send` so it can run on the egress thread.
pub struct Muxer {
    output: format::context::Output,
    streams: Vec<StreamInfo>,
    /// Muxer `AVOption`s to apply at [`write_header`](Muxer::write_header)
    /// (GP-6 Piece B, ADR-0030 §4): e.g. `avoid_negative_ts=make_zero`,
    /// `max_interleave_delta=<n>`. Empty for [`create`](Muxer::create) /
    /// [`create_as`](Muxer::create_as) — those keep the legacy
    /// no-option behaviour unchanged.
    options: MuxOptions,
    header_written: bool,
    trailer_written: bool,
}

impl Muxer {
    /// Allocate an output container for `path`, inferring the format from the
    /// extension (`.mkv`, `.mp4`, `.ts`, …).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Mux`] if the container cannot be allocated/opened.
    pub fn create(path: &Path) -> Result<Self> {
        ensure_initialized()?;
        let output = format::output(&path).map_err(FfmpegError::Mux)?;
        Ok(Self {
            output,
            streams: Vec::new(),
            options: MuxOptions::new(),
            header_written: false,
            trailer_written: false,
        })
    }

    /// Allocate an output container for `path` forcing a specific muxer by name
    /// (e.g. `"matroska"`, `"mp4"`, `"mpegts"`).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Mux`] if the container cannot be allocated/opened.
    pub fn create_as(path: &Path, format_name: &str) -> Result<Self> {
        ensure_initialized()?;
        let output = format::output_as(&path, format_name).map_err(FfmpegError::Mux)?;
        Ok(Self {
            output,
            streams: Vec::new(),
            options: MuxOptions::new(),
            header_written: false,
            trailer_written: false,
        })
    }

    /// Allocate an output container for `path` forcing a specific muxer by name
    /// (as [`create_as`](Muxer::create_as)) **and** record the `AVOption`s to
    /// apply at [`write_header`](Muxer::write_header).
    ///
    /// This is the additive GP-6 (ADR-0030 §4) sibling of
    /// [`create_as`](Muxer::create_as): a **guarded passthrough** sets
    /// `avoid_negative_ts=make_zero` (a **one-shot leading shift** that lands the
    /// first packets at 0 — not a mid-stream monotonicity fix) and, when it needs
    /// to bound interleave buffering, `max_interleave_delta=<n>` (an
    /// **interleave-flush** knob — **NOT** the abort guard; the abort guard is the
    /// per-stream `last_dts + 1` clamp in
    /// `multiview-output`'s `RestampAccumulator`). Passing an empty `options`
    /// slice is exactly equivalent to [`create_as`](Muxer::create_as).
    ///
    /// The options are validated up front (no interior NUL) and stashed; they are
    /// stuffed into a libav `AVDictionary` and handed to `avformat_write_header`
    /// only at [`write_header`](Muxer::write_header) — and any option libav does
    /// **not** consume there surfaces as a typed error (a misspelled key never
    /// passes silently).
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — the container cannot be allocated/opened, or an
    ///   option key/value carries an interior NUL byte.
    pub fn create_with_options(
        path: &Path,
        format_name: &str,
        options: &[(&str, &str)],
    ) -> Result<Self> {
        ensure_initialized()?;
        let options = MuxOptions::from_pairs(options)
            .map_err(|_| FfmpegError::Mux(ffmpeg::Error::InvalidData))?;
        let output = format::output_as(&path, format_name).map_err(FfmpegError::Mux)?;
        Ok(Self {
            output,
            streams: Vec::new(),
            options,
            header_written: false,
            trailer_written: false,
        })
    }

    /// Register a stream from an opened encoder's codec context, recording the
    /// encoder time-base used to rescale that stream's packets. Returns the new
    /// stream index.
    ///
    /// Must be called for every stream **before** [`Muxer::write_header`].
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — the container is sealed or libav refused the
    ///   stream.
    /// * [`FfmpegError::Rational`] — the encoder time-base does not fit.
    pub fn add_stream(
        &mut self,
        encoder: &codec::Context,
        encoder_time_base: Rational,
    ) -> Result<usize> {
        if self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let tb = to_ff_rational(encoder_time_base)?;
        let index = {
            let mut stream = self
                .output
                .add_stream_with(encoder)
                .map_err(FfmpegError::Mux)?;
            stream.set_time_base(tb);
            stream.index()
        };
        // libav may adjust the stream time-base in `write_header`; the actual
        // value is re-read at header time, but seed it with the requested one.
        self.streams.push(StreamInfo {
            index,
            encoder_time_base,
            stream_time_base: encoder_time_base,
        });
        Ok(index)
    }

    /// Register a stream from a `Send` codec-parameters snapshot, recording the
    /// encoder time-base used to rescale that stream's packets. Returns the new
    /// stream index.
    ///
    /// This is the encode-once-mux-many (invariant #7, ADR-0026) sibling of
    /// [`Muxer::add_stream`]: a mux-only sink runs on its own thread with no
    /// encoder instance, so it builds its stream from a
    /// [`StreamCodecParameters`](crate::packet::StreamCodecParameters) carried
    /// across the thread boundary. Functionally identical to `add_stream` — both
    /// copy the same codec parameters onto a fresh stream — but keyed off the
    /// thread-movable snapshot rather than the live encoder context.
    ///
    /// Must be called for every stream **before** [`Muxer::write_header`].
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — the container is sealed or libav refused the
    ///   stream.
    /// * [`FfmpegError::Rational`] — the encoder time-base does not fit.
    pub fn add_stream_from_parameters(
        &mut self,
        parameters: &crate::packet::StreamCodecParameters,
        encoder_time_base: Rational,
    ) -> Result<usize> {
        if self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let tb = to_ff_rational(encoder_time_base)?;
        let index = {
            // A new stream with no codec attached, then copy the codec
            // parameters in (`avcodec_parameters_copy`) — the safe ffmpeg-next
            // path, equivalent to `add_stream_with`'s
            // `avcodec_parameters_from_context` but seeded from the snapshot.
            let mut stream = self
                .output
                .add_stream(None::<ffmpeg::Codec>)
                .map_err(FfmpegError::Mux)?;
            stream.set_parameters(parameters.as_parameters().clone());
            stream.set_time_base(tb);
            stream.index()
        };
        self.streams.push(StreamInfo {
            index,
            encoder_time_base,
            stream_time_base: encoder_time_base,
        });
        Ok(index)
    }

    /// Set a **format-level** (container) metadata key/value on the output
    /// `AVFormatContext.metadata` dictionary (ADR-0088 §3) — e.g. `title`,
    /// `comment`, the MPEG-TS `service_name`/`service_provider` (SDT). Must be
    /// called **before** [`write_header`](Muxer::write_header) (the muxer reads
    /// the metadata dict when it writes the header/SDT).
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — the header is already written (the container is
    ///   sealed) or `key`/`value` carries an interior NUL byte (it could not
    ///   become a C string for `av_dict_set`).
    #[allow(unsafe_code)]
    pub fn set_format_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        if self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let c_key = std::ffi::CString::new(key).map_err(|_| nul_err())?;
        let c_val = std::ffi::CString::new(value).map_err(|_| nul_err())?;
        // SAFETY: `output.as_mut_ptr()` is the live, owned output
        // `AVFormatContext` (we hold `&mut self`). `&raw mut (*ctx).metadata` is
        // its metadata dict slot; `av_dict_set` allocates/extends the dict in
        // place. `c_key`/`c_val` are valid NUL-terminated C strings that outlive
        // the call. The header is not yet written, so mutating the dict is the
        // supported pre-header path.
        let r = unsafe {
            let ctx = self.output.as_mut_ptr();
            ffmpeg::ffi::av_dict_set(&raw mut (*ctx).metadata, c_key.as_ptr(), c_val.as_ptr(), 0)
        };
        if r < 0 {
            return Err(FfmpegError::Mux(ffmpeg::Error::from(r)));
        }
        Ok(())
    }

    /// Set a **per-stream** metadata key/value on `AVStream[index].metadata`
    /// (ADR-0088 §3) — e.g. the ISO-639 `language` PMT/container tag. Must be
    /// called after the stream is added and **before**
    /// [`write_header`](Muxer::write_header).
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — the header is already written, the stream index
    ///   is unknown, or `key`/`value` carries an interior NUL byte.
    #[allow(unsafe_code)]
    pub fn set_stream_metadata(&mut self, index: usize, key: &str, value: &str) -> Result<()> {
        if self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        if !self.streams.iter().any(|s| s.index == index) {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let c_key = std::ffi::CString::new(key).map_err(|_| nul_err())?;
        let c_val = std::ffi::CString::new(value).map_err(|_| nul_err())?;
        let mut stream = self
            .output
            .stream_mut(index)
            .ok_or(FfmpegError::Mux(ffmpeg::Error::InvalidData))?;
        // SAFETY: `stream.as_mut_ptr()` is the live `AVStream` this `StreamMut`
        // wraps (owned by the `AVFormatContext` we hold `&mut`).
        // `&raw mut (*sptr).metadata` is its metadata dict slot; `av_dict_set`
        // allocates/extends it in place. `c_key`/`c_val` are valid
        // NUL-terminated C strings outliving the call; the header is not yet
        // written.
        let r = unsafe {
            let sptr = stream.as_mut_ptr();
            ffmpeg::ffi::av_dict_set(&raw mut (*sptr).metadata, c_key.as_ptr(), c_val.as_ptr(), 0)
        };
        if r < 0 {
            return Err(FfmpegError::Mux(ffmpeg::Error::from(r)));
        }
        Ok(())
    }

    /// Attach a **display-rotation matrix** as `AV_PKT_DATA_DISPLAYMATRIX`
    /// stream side data on `AVStream[index]` (ADR-0089 mechanism *a*, the tag
    /// path). The muxer (MP4/MOV) writes it into the `tkhd` display matrix so a
    /// tag-aware player rotates on render — **zero pixel cost** (invariant #8:
    /// tag, never convert). Must be called after the stream is added and
    /// **before** [`write_header`](Muxer::write_header).
    ///
    /// `matrix` is the libav 16.16 fixed-point 3×3 row-major matrix (the
    /// `multiview_output::metadata::display_matrix` form); the nine `i32`s are
    /// written as 36 little-endian bytes, exactly the `AVPacketSideData`/
    /// `displaymatrix` wire layout libav's `av_display_rotation_get` reads back.
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — the header is written, the stream index is
    ///   unknown, or libav could not allocate the side-data block.
    #[allow(unsafe_code)]
    pub fn set_stream_display_matrix(&mut self, index: usize, matrix: [i32; 9]) -> Result<()> {
        /// The DISPLAYMATRIX side-data size: nine 32-bit fixed-point entries.
        const N: usize = 9 * std::mem::size_of::<i32>();
        if self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        if !self.streams.iter().any(|s| s.index == index) {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let mut stream = self
            .output
            .stream_mut(index)
            .ok_or(FfmpegError::Mux(ffmpeg::Error::InvalidData))?;
        // Pack the nine i32s little-endian (the displaymatrix wire layout) via
        // iterators — no slice indexing.
        let mut bytes = [0u8; N];
        for (chunk, value) in bytes.chunks_exact_mut(4).zip(matrix) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        // SAFETY: `stream.as_mut_ptr()` is the live `AVStream` owned by the
        // `AVFormatContext` we hold `&mut`. `av_packet_side_data_add` appends a
        // side-data block of `N` bytes to the stream's `codecpar.coded_side_data`
        // array (it takes ownership of the heap buffer we hand it, freeing it on
        // failure) — the FFmpeg-7 successor to the removed
        // `av_stream_new_side_data`. We allocate the buffer with `av_memdup` of
        // the little-endian displaymatrix so libav can free it uniformly. A null
        // return (OOM) is surfaced. The header is not yet written.
        let added = unsafe {
            let sptr = stream.as_mut_ptr();
            let buf = ffmpeg::ffi::av_memdup(bytes.as_ptr().cast(), N);
            if buf.is_null() {
                return Err(FfmpegError::Mux(ffmpeg::Error::from(ffmpeg::ffi::AVERROR(
                    ffmpeg::ffi::ENOMEM,
                ))));
            }
            let sd = ffmpeg::ffi::av_packet_side_data_add(
                &raw mut (*(*sptr).codecpar).coded_side_data,
                &raw mut (*(*sptr).codecpar).nb_coded_side_data,
                ffmpeg::ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
                buf,
                N,
                0,
            );
            !sd.is_null()
        };
        if !added {
            return Err(FfmpegError::Mux(ffmpeg::Error::from(ffmpeg::ffi::AVERROR(
                ffmpeg::ffi::ENOMEM,
            ))));
        }
        Ok(())
    }

    /// Write the container header. Call once, after all streams are added.
    ///
    /// If this muxer was built with [`create_with_options`](Muxer::create_with_options),
    /// the recorded `AVOption`s are stuffed into a libav dictionary and passed to
    /// `avformat_write_header`. Any option libav does **not** consume (a
    /// misspelled key) comes back in the leftover dictionary and is treated as a
    /// typed error — options never pass silently.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Mux`] if the header cannot be written or an option
    /// was not accepted by the muxer.
    pub fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Ok(());
        }
        if self.options.is_empty() {
            self.output.write_header().map_err(FfmpegError::Mux)?;
        } else {
            // Build a libav option dictionary from the validated pairs and hand
            // it to `avformat_write_header` (via ffmpeg-next's safe
            // `write_header_with`, which RAII-frees the dict). The returned
            // dictionary holds whatever options the muxer did NOT recognise; a
            // non-empty leftover means a bad/unsupported key, which we surface as
            // a typed error rather than swallow.
            let mut dict = Dictionary::new();
            for (key, value) in self.options.as_pairs() {
                dict.set(key, value);
            }
            let leftover = self
                .output
                .write_header_with(dict)
                .map_err(FfmpegError::Mux)?;
            let unconsumed = leftover.iter().next().is_some();
            if unconsumed {
                return Err(FfmpegError::Mux(ffmpeg::Error::OptionNotFound));
            }
        }
        self.header_written = true;
        // Refresh each stream's (possibly muxer-adjusted) time-base so the
        // packet rescale targets the real value libav will use.
        for info in &mut self.streams {
            if let Some(stream) = self.output.stream(info.index) {
                info.stream_time_base = from_ff_rational(stream.time_base());
            }
        }
        Ok(())
    }

    /// Write one encoded packet to `stream_index`, rescaling its timestamps from
    /// the stream's encoder time-base into the stream time-base and interleaving
    /// it with the other streams.
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — header not yet written, unknown stream, or a
    ///   libav write error.
    pub fn write_packet(
        &mut self,
        stream_index: usize,
        mut packet: codec::packet::Packet,
    ) -> Result<()> {
        if !self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let info = *self
            .streams
            .iter()
            .find(|s| s.index == stream_index)
            .ok_or(FfmpegError::Mux(ffmpeg::Error::StreamNotFound))?;

        packet.set_stream(stream_index);
        let src = to_ff_rational(info.encoder_time_base)?;
        let dst = to_ff_rational(info.stream_time_base)?;
        packet.rescale_ts(src, dst);
        packet
            .write_interleaved(&mut self.output)
            .map_err(FfmpegError::Mux)
    }

    /// Write the container trailer, finalizing the file. Idempotent.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Mux`] if the trailer cannot be written.
    pub fn finish(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        if !self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        self.output.write_trailer().map_err(FfmpegError::Mux)?;
        self.trailer_written = true;
        Ok(())
    }
}
