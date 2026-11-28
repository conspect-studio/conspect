//! A pinned HLS audio rendition whose segment URLs carry no media extension
//! (`CNN`'s `CloudFront` audio-only playlist serves `/v1/segment/.../15190375`)
//! must open: libav's HLS demuxer is "extension picky" by default and rejects
//! such a segment with "is not in `allowed_segment_extensions`", which left the
//! audio rail in an `open failed; source rides silence` loop in production.
#![cfg(feature = "ffmpeg")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use multiview_ffmpeg::AudioFileDecoder;

const RATE: u32 = 48_000;

fn segment_bytes(dir: &Path) -> Vec<u8> {
    let path = dir.join("seg.ts");
    let tone = format!("sine=frequency=440:sample_rate={RATE}:duration=4");
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", &tone])
        .args(["-c:a", "aac", "-ac", "2", "-f", "mpegts"])
        .arg(&path)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed");
    std::fs::read(path).unwrap()
}

fn serve(listener: &TcpListener, playlist: &str, segment: &[u8]) {
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { return };
        let (playlist, segment) = (playlist.to_owned(), segment.to_vec());
        std::thread::spawn(move || {
            let mut buf = [0_u8; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
            let target = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let body = if target.ends_with("0.m3u8") {
                playlist.into_bytes()
            } else {
                segment
            };
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(&body);
        });
    }
}

#[test]
fn live_audio_opens_hls_with_extensionless_segment_urls() {
    let dir = tempfile::tempdir().unwrap();
    let segment = segment_bytes(dir.path());
    let listener = TcpListener::bind("[::1]:0").expect("bind [::1]");
    let addr = listener.local_addr().unwrap();
    let playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:15190361\n\
         #EXTINF:4.0,\nhttp://{addr}/v1/segment/abc/15190361\n\
         #EXTINF:4.0,\nhttp://{addr}/v1/segment/abc/15190362\n#EXT-X-ENDLIST\n"
    );
    std::thread::spawn(move || serve(&listener, &playlist, &segment));

    let url = format!("http://{addr}/v1/manifest/abc/0.m3u8");
    let mut decoder = AudioFileDecoder::open_live(Path::new(&url), RATE, 2)
        .expect("extensionless HLS segments must open");
    let mut samples = 0usize;
    while let Ok(Some(block)) = decoder.next_block() {
        samples += block.frame_count();
    }
    assert!(
        samples > usize::try_from(RATE).unwrap(),
        "only {samples} samples decoded"
    );
}
