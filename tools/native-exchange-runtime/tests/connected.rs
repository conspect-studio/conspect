//! Connected local MoQ model + IPv6 TCP ciphertext seam (not a QUIC wire test).
//! Every encoded access unit is a separate authenticated MoQ group/object.
//! FFmpeg/ffprobe with libx265 and AAC are mandatory; no skipped decoder fallback.
use bytes::Bytes;
use conspect_media_crypto::{Binding, CounterReservation, Publisher};
use conspect_native_exchange_runtime::{publish, Flush, ReceiverRuntime};
use ed25519_dalek::SigningKey;
use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

struct Counter(u64);
impl CounterReservation for Counter {
    fn reserve(
        &mut self,
        _: [u8; 16],
        _: u64,
        _: [u8; 16],
        _: u32,
    ) -> conspect_media_crypto::Result<u64> {
        let n = self.0;
        self.0 += 1;
        Ok(n)
    }
}
const CAP: usize = 4;
#[derive(Default)]
struct Output {
    encoded: VecDeque<u64>,
    decoded: VecDeque<u64>,
    render: VecDeque<u64>,
    audio: VecDeque<u64>,
    high_water: usize,
    dropped: usize,
    flushed: usize,
}
impl Output {
    fn push(q: &mut VecDeque<u64>, pts: u64) -> usize {
        let dropped = usize::from(q.len() == CAP);
        if dropped != 0 {
            q.pop_front();
        }
        q.push_back(pts);
        dropped
    }
    fn enqueue_encoded(&mut self, pts: u64) {
        self.dropped += Self::push(&mut self.encoded, pts);
        self.high_water = self.high_water.max(self.encoded.len());
    }
    fn enqueue_decoded(&mut self, pts: u64, audio: bool) {
        self.dropped += Self::push(&mut self.decoded, pts);
        self.dropped += Self::push(
            if audio {
                &mut self.audio
            } else {
                &mut self.render
            },
            pts,
        );
        self.high_water = self.high_water.max(self.decoded.len());
    }
    fn pending(&self) -> usize {
        self.encoded.len() + self.decoded.len() + self.render.len() + self.audio.len()
    }
}
impl Flush for Output {
    fn flush(&mut self) {
        self.flushed += self.pending();
        self.encoded.clear();
        self.decoded.clear();
        self.render.clear();
        self.audio.clear();
    }
}
fn command(args: &[&str]) -> std::process::Output {
    let out = Command::new(args[0])
        .args(&args[1..])
        .output()
        .expect("installed FFmpeg tools");
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}
struct Fixture {
    name: String,
    format: &'static str,
    packets: Vec<Vec<u8>>,
}
fn fixture(dir: &Path, audio: Option<usize>) -> Fixture {
    let name = audio.map_or_else(|| "video".into(), |i| format!("audio-{i}"));
    let format = if audio.is_some() { "aac" } else { "hevc" };
    let path = dir.join(format!("{name}.{format}"));
    let path_str = path.to_str().unwrap();
    if let Some(i) = audio {
        command(&[
            "ffmpeg",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency={}:sample_rate=48000", 400 + i * 100),
            "-t",
            "1.0",
            "-c:a",
            "aac",
            "-threads",
            "1",
            "-b:a",
            "48k",
            "-f",
            "adts",
            path_str,
        ]);
    } else {
        command(&[
            "ffmpeg",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x64:rate=10",
            "-t",
            "1.0",
            "-c:v",
            "libx265",
            "-threads",
            "1",
            "-x265-params",
            "pools=none:frame-threads=1:keyint=1:repeat-headers=1:log-level=error",
            "-f",
            "hevc",
            path_str,
        ]);
    }
    let probe = command(&[
        "ffprobe",
        "-v",
        "error",
        "-show_packets",
        "-show_entries",
        "packet=pos,size",
        "-of",
        "csv=p=0",
        path_str,
    ]);
    let file = fs::read(&path).unwrap();
    let packets: Vec<_> = String::from_utf8(probe.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let values: Vec<usize> = line.split(',').map(|v| v.parse().unwrap()).collect();
            let (size, pos) = (values[0], values[1]);
            file[pos..pos + size].to_vec()
        })
        .collect();
    assert!(packets.len() >= 8, "requires multiple actual access units");
    assert_eq!(packets.iter().map(Vec::len).sum::<usize>(), file.len());
    Fixture {
        name,
        format,
        packets,
    }
}
// Timestamp input comes ONLY from the actual FFmpeg decoder filter, never packet arrival.
fn decode(dir: &Path, fixture: &Fixture, taker: usize, bytes: &[u8]) -> Vec<u64> {
    let path = dir.join(format!(
        "decoded-input-{taker}-{}.{}",
        fixture.name, fixture.format
    ));
    fs::write(&path, bytes).unwrap();
    let mut args = vec![
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "info",
        "-threads",
        "1",
    ];
    if fixture.format == "hevc" {
        args.extend(["-r", "10"]);
    }
    args.extend(["-f", fixture.format, "-i", path.to_str().unwrap()]);
    if fixture.format == "hevc" {
        args.extend(["-vf", "showinfo"]);
    } else {
        args.extend(["-af", "ashowinfo"]);
    }
    args.extend(["-f", "null", "-"]);
    let out = command(&args);
    let times: Vec<u64> = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("showinfo") && l.contains("pts_time:"))
        .map(|l| {
            let time = l
                .split("pts_time:")
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse::<f64>()
                .unwrap();
            (time * 1000.0).round() as u64
        })
        .collect();
    assert!(
        times.len() >= 3,
        "actual decoded frames: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(times.windows(2).all(|w| w[1] > w[0]));
    times
}
fn binding(taker: u8) -> Binding {
    Binding {
        booking: [1; 16],
        taker: [taker; 16],
        feed: [9; 16],
        version: [3; 16],
        engine: [4; 16],
        publisher: [5; 16],
        generation: 0,
        publisher_generation: 11,
    }
}
fn tcp_copy(tx: &mut TcpStream, rx: &mut TcpStream, wire: &[u8]) -> Vec<u8> {
    tx.write_all(&(wire.len() as u32).to_be_bytes()).unwrap();
    tx.write_all(wire).unwrap();
    let mut header = [0; 4];
    rx.read_exact(&mut header).unwrap();
    let size = u32::from_be_bytes(header) as usize;
    assert!(size < 1024 * 1024);
    let mut data = vec![0; size];
    rx.read_exact(&mut data).unwrap();
    data
}
#[tokio::test]
async fn connected_hevc_five_aac_two_takers_deny_isolated_with_measured_queue_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut fixtures = vec![fixture(dir.path(), None)];
    fixtures.extend((0..5).map(|i| fixture(dir.path(), Some(i))));
    let signer = SigningKey::from_bytes(&[42; 32]);
    let b = binding(2);
    let mut publisher = Publisher::new(
        b.feed,
        b.version,
        b.publisher,
        b.publisher_generation,
        10_000,
        Box::new(Counter(0)),
    )
    .unwrap();
    let mut receivers: Vec<_> = (0..2)
        .map(|i| {
            let b = binding(2 + i as u8);
            let grant = publisher.grant(&signer, b, true, 15_000, 10_000).unwrap();
            ReceiverRuntime::accept(
                &signer.verifying_key(),
                b,
                &grant,
                10_000,
                0,
                dir.path().join(format!("state-{i}")),
            )
            .unwrap()
        })
        .collect();
    let mut outputs = [Output::default(), Output::default()];
    let broadcast = moq_net::broadcast::Producer::new(Default::default());
    let consumer = broadcast.consume();
    let mut tracks: Vec<_> = fixtures
        .iter()
        .map(|f| {
            broadcast
                .create_track(
                    f.name.clone(),
                    Some(moq_net::track::Info::default().with_timescale(moq_net::Timescale::MICRO)),
                )
                .unwrap()
        })
        .collect();
    let mut subscriptions = Vec::new();
    for _ in 0..2 {
        let mut taker = Vec::new();
        for f in &fixtures {
            taker.push(
                consumer
                    .track(&f.name)
                    .unwrap()
                    .subscribe(None)
                    .await
                    .unwrap(),
            );
        }
        subscriptions.push(taker);
    }
    let listener = TcpListener::bind("[::1]:0").unwrap();
    let mut connections: Vec<_> = (0..2)
        .map(|_| {
            let tx = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (rx, _) = listener.accept().unwrap();
            tx.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            rx.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            (tx, rx)
        })
        .collect();
    let mut received = vec![vec![Vec::<u8>::new(); 6]; 2];
    let mut intervals = [[0usize; 6]; 2];
    use conspect_media_crypto::{
        journal::DurableCounterJournal,
        receipt::{DecodedObservation, DurableReceiptSequence, ReceiptAdapter},
        spool::ReceiptSpool,
    };
    let journal = DurableCounterJournal::open(&dir.path().join("receipt-sequence")).unwrap();
    let mut adapter = ReceiptAdapter::new(b.booking, 8, DurableReceiptSequence(journal));
    let mut spool = ReceiptSpool::open(&dir.path().join("receipts")).unwrap();
    let mut recorded = false;
    for phase in 0..2 {
        for (t, f) in fixtures.iter().enumerate() {
            let split = f.packets.len() / 2;
            let range = if phase == 0 {
                0..split
            } else {
                split..f.packets.len()
            };
            for n in range {
                let id = publish(
                    &mut publisher,
                    &mut tracks[t],
                    &f.name,
                    n as u64 * 20_000,
                    10_000,
                    10_000,
                    Bytes::copy_from_slice(&f.packets[n]),
                )
                .unwrap();
                for taker in 0..2 {
                    let group = tokio::time::timeout(
                        Duration::from_secs(2),
                        subscriptions[taker][t].recv_group(),
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                    assert_eq!(group.sequence, id.group);
                    let mut group = group;
                    let frame = group.read_frame().await.unwrap().unwrap();
                    assert!(group.read_frame().await.unwrap().is_none());
                    assert_eq!(&frame.payload[..4], b"MC02");
                    assert_ne!(&frame.payload[..], &f.packets[n]);
                    let (tx, rx) = &mut connections[taker];
                    let wire = tcp_copy(tx, rx, &frame.payload);
                    let opened = receivers[taker].open(&wire, &id, 11_000, &mut outputs[taker]);
                    if taker == 0 && phase == 1 {
                        assert!(opened.is_err());
                        continue;
                    }
                    let plain = opened.unwrap();
                    let hang = hang::container::Frame::decode(Bytes::from(plain)).unwrap();
                    assert_eq!(&hang.payload[..], &f.packets[n]);
                    received[taker][t].extend_from_slice(&hang.payload);
                    outputs[taker].enqueue_encoded(n as u64);
                }
            }
            for taker in 0..2 {
                if phase == 1 && taker == 0 {
                    continue;
                }
                for pts in decode(dir.path(), f, taker, &received[taker][t]) {
                    if let Some((start, end)) = receivers[taker]
                        .decoded(&f.name, 10_000 + pts, 11_000, &mut outputs[taker])
                        .unwrap()
                    {
                        if taker == 1 && t == 0 && !recorded {
                            receivers[taker]
                                .record_decoded(
                                    [8; 16],
                                    DecodedObservation {
                                        media_start_ms: start,
                                        media_end_ms: end,
                                        observed_at_ms: 11_000,
                                        decoded_fresh_advancing: true,
                                        queue_age_ms: Some(0),
                                        bitrate_bps: Some(0),
                                        width: 64,
                                        height: 64,
                                        fps_milli: 10_000,
                                        jitter_ms: Some(0),
                                        backpressure: Some(false),
                                    },
                                    &mut adapter,
                                    &mut spool,
                                    &mut outputs[taker],
                                )
                                .unwrap();
                            assert_eq!(spool.pending(b.booking, 8).unwrap().len(), 1);
                            recorded = true;
                        }
                        intervals[taker][t] += 1;
                        // Permission is fenced immediately before decoder output enters queues.
                        receivers[taker].fence(11_000, &mut outputs[taker]).unwrap();
                        outputs[taker].enqueue_decoded(pts, f.format == "aac");
                    }
                }
            }
        }
        if phase == 0 {
            assert!(intervals
                .iter()
                .all(|tracks| tracks.iter().all(|&n| n >= 2)));
            assert_eq!(outputs[0].pending(), 4 * CAP);
            assert!(outputs[0].dropped > 0);
            assert_eq!(outputs[0].high_water, CAP);
            let other_pending = outputs[1].pending();
            let start = Instant::now();
            receivers[0].deny(&mut outputs[0]).unwrap();
            let cutoff = start.elapsed();
            assert_eq!(outputs[0].pending(), 0);
            assert_eq!(outputs[0].flushed, 4 * CAP);
            assert_eq!(outputs[1].pending(), other_pending);
            assert!(
                cutoff < Duration::from_millis(500),
                "deny cutoff {cutoff:?}"
            );
            assert!(receivers[0]
                .decoded("video", 11_001, 11_000, &mut outputs[0])
                .is_err());
            eprintln!(
                "deny cutoff_us={} flushed={} max_queue={} dropped={} remaining=0",
                cutoff.as_micros(),
                outputs[0].flushed,
                outputs[0].high_water,
                outputs[0].dropped
            );
        }
    }
    assert_eq!(outputs[0].pending(), 0);
    assert_eq!(outputs[1].flushed, 0);
    for t in 0..6 {
        assert!(
            intervals[1][t] > intervals[0][t],
            "other taker must keep advancing track {t}"
        );
        assert_eq!(received[1][t], fixtures[t].packets.concat());
        assert!(received[0][t].len() < received[1][t].len());
    }
    eprintln!(
        "decoded advancing intervals denied={:?} continuing={:?}",
        intervals[0], intervals[1]
    );
}

#[tokio::test]
async fn durable_reconnect_replay_deny_and_independent_monotonic_expiry_cutoff() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let signer = SigningKey::from_bytes(&[42; 32]);
    let b = binding(2);
    let mut publisher = Publisher::new(
        b.feed,
        b.version,
        b.publisher,
        b.publisher_generation,
        10_000,
        Box::new(Counter(0)),
    )
    .unwrap();
    let grant = publisher.grant(&signer, b, true, 15_000, 10_000).unwrap();
    let path = dir.path().join("reconnect");
    let mut receiver =
        ReceiverRuntime::accept(&signer.verifying_key(), b, &grant, 10_000, 0, path.clone())
            .unwrap();
    let mut output = Output::default();
    let id = conspect_media_crypto::ObjectId {
        track: "video".into(),
        group: 0,
        object: 0,
        captured_ms: 10_000,
    };
    let wire = publisher
        .encrypt(&id, 10_000, b"restart replay object")
        .unwrap();
    assert_eq!(
        receiver.open(&wire, &id, 10_000, &mut output).unwrap(),
        b"restart replay object"
    );
    drop(receiver);
    let mut receiver =
        ReceiverRuntime::accept(&signer.verifying_key(), b, &grant, 10_000, 0, path.clone())
            .unwrap();
    assert!(matches!(
        receiver.open(&wire, &id, 10_000, &mut output),
        Err(conspect_media_crypto::Error::Replay)
    ));
    receiver.deny(&mut output).unwrap();
    drop(receiver);
    assert!(ReceiverRuntime::accept(&signer.verifying_key(), b, &grant, 10_000, 0, path).is_err());

    // No packets arrive and the supplied wall clock is frozen. The independent
    // expiry timer must still flush every output queue on its monotonic deadline.
    let now = 14_900;
    let path = dir.path().join("expiry");
    let mut receiver =
        ReceiverRuntime::accept(&signer.verifying_key(), b, &grant, now, 0, path).unwrap();
    for pts in 0..CAP as u64 {
        output.enqueue_encoded(pts);
        output.enqueue_decoded(pts, false);
        output.enqueue_decoded(pts, true);
    }
    assert_eq!(output.pending(), 4 * CAP);
    let before = Instant::now();
    tokio::time::sleep(Duration::from_millis(60)).await;
    // Refreshing the exact same signed expiry at a frozen wall clock cannot
    // restart its monotonic TTL. It must still expire at the original deadline.
    receiver
        .refresh(&signer.verifying_key(), &grant, now, 0, &mut output)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(receiver.fence(now, &mut output).is_err());
    let elapsed = before.elapsed();
    assert_eq!(output.pending(), 0);
    assert_eq!(output.flushed, 4 * CAP);
    assert!(elapsed >= Duration::from_millis(100));
    assert!(
        elapsed < Duration::from_millis(500),
        "expiry cutoff {elapsed:?}"
    );
    eprintln!(
        "independent expiry cutoff_us={} flushed={} remaining=0",
        elapsed.as_micros(),
        output.flushed
    );

    // The wall-clock boundary also fails closed without waiting for real time.
    let mut receiver = ReceiverRuntime::accept(
        &signer.verifying_key(),
        b,
        &grant,
        10_000,
        0,
        dir.path().join("wall-expiry"),
    )
    .unwrap();
    output.enqueue_encoded(1);
    assert!(receiver.fence(15_000, &mut output).is_err());
    assert_eq!(output.pending(), 0);
}

#[test]
fn restart_rejects_generation_epoch_rollback_and_incomplete_transactions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let signer = SigningKey::from_bytes(&[42; 32]);
    let grant_for = |generation, now| {
        let mut b = binding(2);
        b.publisher_generation = generation;
        let mut publisher = Publisher::new(
            b.feed,
            b.version,
            b.publisher,
            generation,
            now,
            Box::new(Counter(0)),
        )
        .unwrap();
        let grant = publisher.grant(&signer, b, true, now + 5_000, now).unwrap();
        (b, grant)
    };
    let root = signer.verifying_key();
    let (b9, g9) = grant_for(9, 10_000);
    let (b10, g10) = grant_for(10, 10_000);
    let path = dir.path().join("generation");
    drop(ReceiverRuntime::accept(&root, b9, &g9, 10_000, 0, path.clone()).unwrap());
    drop(ReceiverRuntime::accept(&root, b10, &g10, 10_000, 0, path.clone()).unwrap());
    assert!(
        ReceiverRuntime::accept(&root, b9, &g9, 10_000, 0, path).is_err(),
        "numeric generation 10 must reject rollback to 9"
    );

    let path = dir.path().join("epoch");
    let (early_b, early) = grant_for(10, 10_000);
    let (late_b, late) = grant_for(10, 30_000);
    drop(ReceiverRuntime::accept(&root, early_b, &early, 10_000, 0, path.clone()).unwrap());
    drop(ReceiverRuntime::accept(&root, late_b, &late, 30_000, 0, path.clone()).unwrap());
    assert!(
        ReceiverRuntime::accept(&root, early_b, &early, 10_000, 0, path).is_err(),
        "clock rollback cannot reauthorize an old epoch"
    );

    for suffix in ["poison", "stage"] {
        let path = dir.path().join(format!("incomplete-{suffix}.json"));
        drop(ReceiverRuntime::accept(&root, b10, &g10, 10_000, 0, path.clone()).unwrap());
        // Append the suffix to the entire filename, never replace its extension.
        let marker = dir
            .path()
            .join(format!("incomplete-{suffix}.json.{suffix}"));
        fs::write(marker, b"incomplete durable transaction").unwrap();
        assert!(
            ReceiverRuntime::accept(&root, b10, &g10, 10_000, 0, path).is_err(),
            "preexisting {suffix} must fail closed"
        );
    }

    // Both live owners share a stem but NOT a state path; neither lock may alias.
    let first_path = dir.path().join("same.json");
    let second_path = dir.path().join("same.bin");
    let first = ReceiverRuntime::accept(&root, b10, &g10, 10_000, 0, first_path.clone()).unwrap();
    let second = ReceiverRuntime::accept(&root, b10, &g10, 10_000, 0, second_path.clone()).unwrap();
    assert!(dir.path().join("same.json.lock").exists());
    assert!(dir.path().join("same.bin.lock").exists());
    assert!(ReceiverRuntime::accept(&root, b10, &g10, 10_000, 0, first_path).is_err());
    assert!(ReceiverRuntime::accept(&root, b10, &g10, 10_000, 0, second_path).is_err());
    drop((first, second));
}

#[test]
fn verified_epoch_is_durable_before_open_and_faults_fail_closed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let signer = SigningKey::from_bytes(&[42; 32]);
    let b = binding(2);
    let make = |now| {
        let mut p = Publisher::new(
            b.feed,
            b.version,
            b.publisher,
            b.publisher_generation,
            now,
            Box::new(Counter(0)),
        )
        .unwrap();
        let g = p.grant(&signer, b, true, now + 5000, now).unwrap();
        (p, g)
    };
    let (mut old_p, old_g) = make(10_000);
    let (mut new_p, new_g) = make(15_000);
    let old_id = conspect_media_crypto::ObjectId {
        track: "video".into(),
        group: 1,
        object: 0,
        captured_ms: 10_000,
    };
    let new_id = conspect_media_crypto::ObjectId {
        captured_ms: 15_000,
        ..old_id.clone()
    };
    let old_wire = old_p.encrypt(&old_id, 10_000, b"old").unwrap();
    let new_wire = new_p.encrypt(&new_id, 15_000, b"new").unwrap();
    let path = dir.path().join("transition");
    let mut output = Output::default();
    let mut r =
        ReceiverRuntime::accept(&signer.verifying_key(), b, &old_g, 10_000, 0, path.clone())
            .unwrap();
    assert_eq!(
        r.open(&old_wire, &old_id, 10_000, &mut output).unwrap(),
        b"old"
    );
    r.refresh(&signer.verifying_key(), &new_g, 15_000, 0, &mut output)
        .unwrap();
    let state: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        state["epoch"], 3,
        "epoch committed before first new decrypt"
    );
    assert_eq!(
        r.open(&new_wire, &new_id, 15_000, &mut output).unwrap(),
        b"new"
    );
    drop(r);
    assert!(
        ReceiverRuntime::accept(&signer.verifying_key(), b, &old_g, 10_000, 0, path.clone())
            .is_err()
    );
    let mut r =
        ReceiverRuntime::accept(&signer.verifying_key(), b, &new_g, 15_000, 0, path).unwrap();
    assert!(r.open(&new_wire, &new_id, 15_000, &mut output).is_err());
    drop(r);
    for transition in [true, false] {
        let path = dir.path().join(format!("fault-{transition}"));
        let mut r =
            ReceiverRuntime::accept(&signer.verifying_key(), b, &old_g, 10_000, 0, path.clone())
                .unwrap();
        // Exclusive create fails after the durable poison marker is committed.
        fs::write(
            dir.path().join(format!("fault-{transition}.stage")),
            b"injected",
        )
        .unwrap();
        if transition {
            assert!(r
                .refresh(&signer.verifying_key(), &new_g, 15_000, 0, &mut output)
                .is_err());
        } else {
            assert!(r.open(&old_wire, &old_id, 10_000, &mut output).is_err());
        }
        assert!(r.open(&new_wire, &new_id, 15_000, &mut output).is_err());
        drop(r);
        assert!(
            ReceiverRuntime::accept(&signer.verifying_key(), b, &new_g, 15_000, 0, path).is_err()
        );
    }
}

#[test]
fn malformed_native_decode_returns_error_without_unwind() {
    let dir = tempfile::tempdir().unwrap();
    let f = conspect_native_exchange_runtime::media::Fixture {
        name: "fault".into(),
        format: "hevc",
        packets: vec![],
    };
    let result = std::panic::catch_unwind(|| {
        conspect_native_exchange_runtime::media::decode(dir.path(), &f, 0, &[0, 1])
    });
    assert!(result.unwrap().is_err());
}
