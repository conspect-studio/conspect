use multiview_input::hls::{CueMetadata, MasterPlaylist, MediaPlaylist, MediaType};

#[test]
fn vendor_data_renditions_are_preserved() {
    let text = "#EXTM3U\n#EXT-X-MEDIA:TYPE=DATA,GROUP-ID=\"scte_35\",NAME=\"scte_35\",URI=\"cue.m3u8\",X-VENDOR=\"a,b\"\n#EXT-X-MEDIA:TYPE=DATA,GROUP-ID=\"timed_id3\",NAME=\"timed_id3\"\n";
    let master = MasterPlaylist::parse(text).unwrap();
    assert_eq!(master.renditions[0].media_type, MediaType::Data);
    let data = MasterPlaylist::parse_data_renditions(text).unwrap();
    assert_eq!(data.len(), 2);
    assert!(data[0]
        .attributes
        .contains(&("X-VENDOR".into(), "a,b".into())));
}

#[test]
fn binds_observations_without_claiming_ad_boundaries() {
    let text = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:70\n#EXT-X-DISCONTINUITY-SEQUENCE:4\n#EXT-X-DATERANGE:ID=\"cue\",START-DATE=\"2026-11-05T00:00:00Z\",SCTE35-OUT=0xFC00\n#EXT-X-ASSET:CAID=0x41442D31\n#EXTINF:6,\na.ts\n#EXT-X-DISCONTINUITY\n#EXT-X-ASSET:CAID=0x41442D31\n#EXTINF:6,\nb.ts\n";
    let parsed = MediaPlaylist::parse_cue_metadata(text).unwrap();
    assert_eq!(parsed.observations.len(), 3);
    let first = parsed.observations[0].segment.as_ref().unwrap();
    assert_eq!(
        (
            first.segment_index,
            first.media_sequence,
            first.discontinuity_sequence
        ),
        (0, Some(70), Some(4))
    );
    assert!(
        matches!(&parsed.observations[0].metadata, CueMetadata::DateRange { attributes } if attributes.iter().any(|(k,v)| k == "SCTE35-OUT" && v == "0xFC00"))
    );
    assert!(
        matches!(&parsed.observations[1].metadata, CueMetadata::Asset { raw_caid, decoded_label } if raw_caid == "0x41442D31" && decoded_label.as_deref() == Some("AD-1"))
    );
    let second = parsed.observations[2].segment.as_ref().unwrap();
    assert_eq!(
        (
            second.segment_index,
            second.media_sequence,
            second.discontinuity_sequence
        ),
        (1, Some(71), Some(5))
    );
    // Reused CAID remains two observations, not a globally unique break identity.
    assert_eq!(
        parsed.observations[1].metadata,
        parsed.observations[2].metadata
    );
    // A sliding window without the marker supplies no manufactured end.
    assert!(
        MediaPlaylist::parse_cue_metadata("#EXTM3U\n#EXTINF:6,\nc.ts\n")
            .unwrap()
            .observations
            .is_empty()
    );
}

#[test]
fn dangling_markers_and_nonprintable_assets_remain_observational() {
    let parsed = MediaPlaylist::parse_cue_metadata("#EXTM3U\n#EXT-X-ASSET:CAID=0x001B\n").unwrap();
    assert!(parsed.observations[0].segment.is_none());
    assert!(
        matches!(&parsed.observations[0].metadata, CueMetadata::Asset { raw_caid, decoded_label } if raw_caid == "0x001B" && decoded_label.is_none())
    );
}

#[test]
fn malformed_and_oversized_metadata_is_rejected_not_partially_trusted() {
    let text = format!("#EXTM3U\n#EXT-X-ASSET:CAID=\"unterminated\n#EXT-X-ASSET:CAID=a,CAID=b\n#EXT-X-DATERANGE:ID=\"bad\",SCTE35-OUT=0xFG\n#EXT-X-ASSET:CAID={}\n", "a".repeat(20_000));
    let parsed = MediaPlaylist::parse_cue_metadata(&text).unwrap();
    assert!(parsed.observations.is_empty());
    assert_eq!(parsed.rejected, 4);
}

#[test]
fn sequence_overflow_and_invalid_context_are_unknown() {
    let text = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:18446744073709551615\n#EXT-X-DISCONTINUITY-SEQUENCE:18446744073709551615\n#EXT-X-ASSET:CAID=asset\na.ts\n#EXT-X-DISCONTINUITY\n#EXT-X-ASSET:CAID=asset\nb.ts\n#EXT-X-MEDIA-SEQUENCE:nope\n#EXT-X-ASSET:CAID=asset\nc.ts\n";
    let parsed = MediaPlaylist::parse_cue_metadata(text).unwrap();
    assert_eq!(
        parsed.observations[0]
            .segment
            .as_ref()
            .unwrap()
            .media_sequence,
        Some(u64::MAX)
    );
    let overflow = parsed.observations[1].segment.as_ref().unwrap();
    assert_eq!(overflow.media_sequence, None);
    assert_eq!(overflow.discontinuity_sequence, None);
    assert_eq!(
        parsed.observations[2]
            .segment
            .as_ref()
            .unwrap()
            .media_sequence,
        None
    );
}

#[test]
fn attribute_count_and_scte_in_are_preserved_safely() {
    let attrs = (0..65)
        .map(|i| format!("X-{i}=value"))
        .collect::<Vec<_>>()
        .join(",");
    let text = format!("#EXTM3U\n#EXT-X-ASSET:CAID=asset,{attrs}\n#EXT-X-DATERANGE:ID=cue,SCTE35-IN=0xFC00,END-DATE=\"2026-11-05T00:00:00Z\"\na.ts\n");
    let parsed = MediaPlaylist::parse_cue_metadata(&text).unwrap();
    assert_eq!(parsed.rejected, 1);
    assert_eq!(parsed.observations.len(), 1);
    assert!(
        matches!(&parsed.observations[0].metadata, CueMetadata::DateRange { attributes } if attributes.iter().any(|(k,v)| k == "SCTE35-IN" && v == "0xFC00"))
    );
}

#[test]
fn observation_budget_is_explicit() {
    let text = format!("#EXTM3U\n{}", "#EXT-X-ASSET:CAID=asset\n".repeat(2000));
    let parsed = MediaPlaylist::parse_cue_metadata(&text).unwrap();
    assert_eq!(parsed.observations.len(), 1024);
    assert_eq!(parsed.dropped, 976);
}
