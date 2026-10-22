//! Kernel `/proc/asound/cardN/eld#C.P` **text** parsing + ELD-entry selection
//! tests (DEV-B4 / display-out §5).
//!
//! On x86 HDA the kernel publishes the ELD as a *text* dump (one `key\tvalue`
//! line per field, `snd_hdmi_print_eld_info`); the sink's `/proc` reader hands
//! that text to a **pure** parser so the gating logic is CI-proven without
//! hardware. The fixtures below quote the kernel's verbatim print formats
//! (`sad%d_coding_type\t[0x%x] %s` with the LPCM name `LPCM`,
//! `sad%d_rates\t\t[0x%x] 32000 44100 …` — verified against
//! `sound/pci/hda/hda_eld.c`). The selection helpers (which ELD entry / which
//! vc4 card serves a connector) are pure too, so the discovery policy is pinned
//! here and only the `/proc`+libasound I/O shells run on hardware.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::display::audio::{
    parse_proc_eld_text, pick_eld_entry, vc4_card_candidates, EldCapability,
};

/// A verbatim-format kernel ELD text dump for a stereo LPCM monitor (the
/// t630-class measured shape: 2-ch LPCM, 32/44.1/48 kHz + extras).
const KERNEL_ELD_TEXT: &str = "monitor_present\t\t1\n\
eld_valid\t\t1\n\
monitor_name\t\tACME LCD\n\
connection_type\t\tHDMI\n\
eld_version\t\t[0x2] CEA-861D or below\n\
edid_version\t\t[0x3] CEA-861-B, C or D\n\
manufacture_id\t\t0x593a\n\
product_id\t\t0x0\n\
port_id\t\t0x0\n\
support_hdcp\t\t0\n\
support_ai\t\t0\n\
audio_sync_delay\t0\n\
speakers\t\t[0x1] FL/FR\n\
sad_count\t\t1\n\
sad0_coding_type\t[0x1] LPCM\n\
sad0_channels\t\t2\n\
sad0_rates\t\t[0x5e0] 32000 44100 48000 88200 96000\n\
sad0_bits\t\t[0xe0000] 16 20 24\n";

#[test]
fn parses_the_kernel_text_dump() {
    let cap = parse_proc_eld_text(KERNEL_ELD_TEXT)
        .expect("a valid kernel ELD text dump must parse to a capability");
    assert_eq!(cap.max_channels(), 2, "sad0_channels 2 => stereo");
    assert!(cap.supports_rate(48_000), "48 kHz is in the rate list");
    assert!(cap.supports_rate(88_200), "88.2 kHz is in the rate list");
    assert!(
        !cap.supports_rate(176_400),
        "176.4 kHz was never printed => unsupported"
    );
    assert!(cap.supports_lpcm(), "coding type [0x1] LPCM => LPCM path");
    assert_eq!(
        cap.monitor_name(),
        "ACME LCD",
        "multi-word monitor names survive"
    );
}

#[test]
fn eld_invalid_means_no_audio_path() {
    // The pipe not lit / EDID-less head: the kernel prints `eld_valid 0` (and
    // usually no SADs). Must be None — no audio path — never a panic.
    let text = "monitor_present\t\t0\n\
eld_valid\t\t0\n\
monitor_name\t\t\n";
    assert!(parse_proc_eld_text(text).is_none());
}

#[test]
fn empty_text_means_no_audio_path() {
    assert!(parse_proc_eld_text("").is_none());
}

#[test]
fn non_lpcm_only_sink_has_no_audio_path() {
    // A (hypothetical) sink advertising only a compressed format: we never
    // bitstream, so no LPCM SAD => no audio path.
    let text = "monitor_present\t\t1\n\
eld_valid\t\t1\n\
monitor_name\t\tAVR\n\
sad_count\t\t1\n\
sad0_coding_type\t[0x2] AC-3\n\
sad0_channels\t\t6\n\
sad0_rates\t\t[0x1c0] 48000\n";
    assert!(parse_proc_eld_text(text).is_none(), "AC-3-only => None");
}

#[test]
fn multiple_sads_take_the_lpcm_max() {
    // Two LPCM SADs (2ch + 6ch): the capability carries the max channel count
    // and the union of rates.
    let text = "eld_valid\t\t1\n\
monitor_name\t\tBIG\n\
sad_count\t\t2\n\
sad0_coding_type\t[0x1] LPCM\n\
sad0_channels\t\t2\n\
sad0_rates\t\t[0x e0] 32000 44100 48000\n\
sad1_coding_type\t[0x1] LPCM\n\
sad1_channels\t\t6\n\
sad1_rates\t\t[0x80] 96000\n";
    let cap = parse_proc_eld_text(text).expect("valid");
    assert_eq!(cap.max_channels(), 6);
    assert!(cap.supports_rate(48_000));
    assert!(cap.supports_rate(96_000));
}

#[test]
fn entry_selection_prefers_the_first_valid_eld() {
    // Discovery scans every eld#D.P of a card and must pick the first entry
    // with a VALID parsed capability (a lit pipe), so a machine with one lit
    // HDMI head among several pins finds it regardless of pin order.
    let lit = EldCapability::lpcm(2, &[48_000], "LIT");
    let entries = vec![None, Some(lit), None];
    assert_eq!(
        pick_eld_entry(&entries),
        Some(1),
        "the lit (valid) entry wins"
    );
}

#[test]
fn entry_selection_falls_back_to_the_first_entry_when_none_valid() {
    // No pin is lit yet (e.g. boot before hotplug): pick the first entry so the
    // sink can keep polling it and light up when the pipe does — never give up
    // the audio path entirely just because boot raced the monitor.
    let entries: Vec<Option<EldCapability>> = vec![None, None];
    assert_eq!(pick_eld_entry(&entries), Some(0));
    assert_eq!(
        pick_eld_entry(&[]),
        None,
        "no entries at all => no candidate"
    );
}

#[test]
fn vc4_cards_map_from_the_connector_index() {
    // Pi vc4-hdmi exposes one ALSA card per HDMI port: HDMI-A-1 => vc4hdmi0
    // (or plain `vc4hdmi` on single-port models), HDMI-A-2 => vc4hdmi1. A
    // non-HDMI connector has no vc4 mapping.
    assert_eq!(
        vc4_card_candidates("HDMI-A-1"),
        vec!["vc4hdmi0".to_owned(), "vc4hdmi".to_owned()]
    );
    assert_eq!(vc4_card_candidates("HDMI-A-2"), vec!["vc4hdmi1".to_owned()]);
    assert!(vc4_card_candidates("DP-1").is_empty());
    assert!(vc4_card_candidates("HDMI-A-").is_empty());
}
