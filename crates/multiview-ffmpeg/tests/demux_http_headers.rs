//! Operator HTTP headers reach the wire on a libav open (`DemuxOptions`).
#![cfg(feature = "ffmpeg")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use multiview_ffmpeg::{DemuxOptions, Demuxer, HttpHeaders};

fn capture(headers: HttpHeaders) -> String {
    let listener = TcpListener::bind("[::1]:0").expect("bind [::1]");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut conn, _)) = listener.accept() {
            let mut buf = [0_u8; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let _ = conn.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    let opts = DemuxOptions::new()
        .with_rw_timeout(Duration::from_secs(3))
        .with_http_headers(headers);
    let _ = Demuxer::open_with_interrupt(Path::new(&format!("http://{addr}/x.m3u8")), opts);
    rx.recv_timeout(Duration::from_secs(5))
        .expect("request seen")
        .to_ascii_lowercase()
}

#[test]
fn user_agent_and_referer_are_sent_by_libav() {
    let head = capture(HttpHeaders::new(
        Some("Opera/Test 9"),
        Some("https://ref.example/"),
    ));
    assert!(head.contains("user-agent: opera/test 9"), "{head}");
    assert!(head.contains("referer: https://ref.example/"), "{head}");
}

#[test]
fn control_characters_are_dropped_not_injected() {
    let h = HttpHeaders::new(Some("ok\r\nX-Evil: 1"), Some("https://r/\n"));
    assert!(h.is_empty());
    let head = capture(h);
    assert!(!head.contains("x-evil"), "{head}");
}
