//! Private connected authority/CPU decoder proof. Not a QUIC/device proof.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bytes::Bytes;
use conspect_media_crypto::{
    authority_http::VerifiedHttpsTransport,
    bootstrap::{private_json, Bootstrap},
    journal::DurableCounterJournal,
    AuthorityRootGuard, Binding, Publisher,
};
use conspect_media_crypto::{
    receipt::{DecodedObservation, DurableReceiptSequence, ReceiptAdapter},
    spool::ReceiptSpool,
};
use conspect_native_exchange_runtime::{
    media::{decode, fixture, Output},
    publish, ReceiverRuntime,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, &'static str>;
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn raw(v: &Value) -> Result<[u8; 16]> {
    let s = v.as_str().ok_or("identity missing")?;
    let b = URL_SAFE_NO_PAD.decode(s).map_err(|_| "identity encoding")?;
    if URL_SAFE_NO_PAD.encode(&b) != s {
        return Err("identity canonical");
    }
    b.try_into().map_err(|_| "identity size")
}
fn binding(v: &Value, i: usize, g: u32) -> Result<Binding> {
    let b = &v[if i == 0 { "bookingA" } else { "bookingB" }];
    Ok(Binding {
        booking: raw(&b["booking"])?,
        taker: raw(&b["takerOrg"])?,
        feed: raw(&v["shared"]["feed"])?,
        version: raw(&v["shared"]["version"])?,
        engine: raw(&b["receiverMachine"])?,
        publisher: raw(&v["shared"]["publisherMachine"])?,
        generation: 0,
        publisher_generation: g,
    })
}
fn copy(tx: &mut TcpStream, rx: &mut TcpStream, b: &[u8]) -> Result<Vec<u8>> {
    tx.write_all(&(b.len() as u32).to_be_bytes())
        .and_then(|_| tx.write_all(b))
        .map_err(|_| "TCP write")?;
    let mut size = [0; 4];
    rx.read_exact(&mut size).map_err(|_| "TCP size")?;
    let n = u32::from_be_bytes(size) as usize;
    if n > 1024 * 1024 {
        return Err("TCP bound");
    }
    let mut out = vec![0; n];
    rx.read_exact(&mut out).map_err(|_| "TCP read")?;
    Ok(out)
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1)
    }
}
fn run() -> Result<()> {
    let a: Vec<_> = std::env::args_os().skip(1).collect();
    if a.len() != 8
        || a[0] != "--bootstrap"
        || a[2] != "--marker"
        || a[4] != "--expected"
        || a[6] != "--scenario"
    {
        return Err("usage: --bootstrap MANIFEST --marker MARKER --expected EXPECTED --scenario release|withdraw|exhaustion|credit-cutoff");
    }
    let scenario = a[7].to_str().ok_or("scenario")?;
    if !["release", "withdraw", "exhaustion", "credit-cutoff"].contains(&scenario) {
        return Err("scenario");
    }
    let manifest = private_json(Path::new(&a[1]))?;
    let mut state = Bootstrap::parse(&manifest)?;
    let t = VerifiedHttpsTransport::from_harness_ca_pem(
        manifest["caPublic"].as_str().ok_or("CA")?.as_bytes(),
    )
    .map_err(|_| "TLS CA")?;
    // Generate actual HEVC plus five separate AAC elementary streams before acquiring a short lease.
    let dir = tempfile::tempdir().map_err(|_| "private workdir")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "private workdir mode")?;
    let mut fixtures = vec![fixture(dir.path(), None)?];
    for i in 0..5 {
        fixtures.push(fixture(dir.path(), Some(i))?);
    }
    let durable = Path::new(&a[1]).with_extension("runtime-state");
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(&durable) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(_) => return Err("durable state directory"),
    }
    let _durable_directory = conspect_media_crypto::private_state::PrivateDir::open(&durable)
        .map_err(|_| "private durable state required")?;
    state.acquire(Arc::new(t.clone()))?;
    let mut release = LeaseRelease {
        state: Some(state.clone()),
        transport: t.clone(),
    };
    let result = connected(
        &mut state,
        &t,
        &manifest,
        &a,
        &fixtures,
        (dir.path(), &durable),
        scenario,
    );
    let released = release.release();
    if scenario == "withdraw" && result.is_ok() {
        // The lease was proven revoked by the withdrawal; the release answer for
        // a revoked lease is a refusal by contract and confers nothing.
        return result;
    }
    result.and(released)
}
// The connected scope drops its control coordinator before this guard releases
// the shared lease, including early errors and unwind. Transport timeouts bound I/O.
struct LeaseRelease {
    state: Option<Bootstrap>,
    transport: VerifiedHttpsTransport,
}
impl LeaseRelease {
    fn release(&mut self) -> Result<()> {
        self.state
            .take()
            .map_or(Ok(()), |mut state| state.release(&self.transport))
    }
}
impl Drop for LeaseRelease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
fn connected(
    state: &mut Bootstrap,
    t: &VerifiedHttpsTransport,
    m: &Value,
    a: &[std::ffi::OsString],
    fixtures: &[conspect_native_exchange_runtime::media::Fixture],
    paths: (&Path, &Path),
    scenario: &str,
) -> Result<()> {
    let (dir, durable) = paths;
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "media runtime")?;
    if (scenario == "credit-cutoff") != state.funded_expiry() {
        return Err("credit-cutoff requires funded-expiry profile exclusively");
    }
    state.book(t)?;
    state.renew(t)?;
    state.mint_mapping(t)?;
    state.write_marker(Path::new(&a[3]))?;
    let marker_at = now();
    let expected = state.wait_expected(t, Path::new(&a[5]))?;
    state.validate_expected(&expected)?;
    if scenario == "credit-cutoff" {
        // A's paid window is short and started before the coordinator handoff.
        // Never fabricate media for it: if the window is (nearly) over, say so.
        let expiry = state.booking_expiry()?;
        if now().saturating_add(2000) >= expiry {
            eprintln!(
                "credit-cutoff: booking A paid window ends at {expiry}; fragment handoff took {} ms after the marker, leaving {} ms",
                now().saturating_sub(marker_at),
                expiry.saturating_sub(now())
            );
            return Err("credit-cutoff: booking A paid window elapsed during fragment handoff");
        }
    }
    let key = URL_SAFE_NO_PAD
        .decode(
            m["mg02Verification"]["jwk"]["x"]
                .as_str()
                .ok_or("verification key")?,
        )
        .map_err(|_| "verification key")?;
    let key =
        ed25519_dalek::VerifyingKey::from_bytes(&key.try_into().map_err(|_| "verification key")?)
            .map_err(|_| "verification key")?;
    let bindings = [
        binding(&expected, 0, state.generation()?)?,
        binding(&expected, 1, state.generation()?)?,
    ];
    // Bounded near-boundary wait on this control thread, never an output clock.
    if scenario != "credit-cutoff" && now() % 5000 > 3000 {
        state.renew(t)?;
        std::thread::sleep(Duration::from_millis(5000 - now() % 5000));
    }
    state.renew(t)?;
    let deadline = Arc::new(Mutex::new(state.deadline()));
    let mut guard = AuthorityRootGuard::new().with_lease_fence(state.publisher_lease()?);
    let provider = state.root_provider()?;
    let b = bindings[0];
    let (publisher_binding, _, publisher_instance) = state.publisher_context();
    let contributor = publisher_binding.org;
    let scenario_end = Instant::now() + Duration::from_secs(90);
    let mut publisher = Publisher::from_authority(
        provider.as_ref(),
        &mut guard,
        contributor,
        b.version,
        b.feed,
        b.publisher,
        publisher_instance,
        now(),
        Box::new(
            DurableCounterJournal::open(&durable.join("nonces")).map_err(|_| "nonce journal")?,
        ),
    )
    .map_err(|_| "authority publisher")?;
    let mut receivers = Vec::new();
    for (i, b) in bindings.iter().enumerate() {
        let grant = state.grant(t, i)?;
        receivers.push(
            ReceiverRuntime::accept(
                &key,
                *b,
                &grant,
                now(),
                0,
                durable.join(format!("receiver-{i}")),
            )
            .map_err(|_| "signed independent binding")?,
        );
    }
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
        let mut s = Vec::new();
        for f in fixtures {
            s.push(
                runtime
                    .block_on(consumer.track(&f.name).unwrap().subscribe(None))
                    .map_err(|_| "subscribe")?,
            );
        }
        subscriptions.push(s);
    }
    let listener = TcpListener::bind("[::1]:0").map_err(|_| "TCP listen")?;
    let mut links = Vec::new();
    for _ in 0..2 {
        let tx = TcpStream::connect(listener.local_addr().unwrap()).map_err(|_| "TCP connect")?;
        let (rx, _) = listener.accept().map_err(|_| "TCP accept")?;
        tx.set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|_| "TCP timeout")?;
        rx.set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|_| "TCP timeout")?;
        links.push((tx, rx));
    }
    let mut adapters = Vec::new();
    let mut spools = Vec::new();
    for (i, b) in bindings.iter().enumerate() {
        adapters.push(ReceiptAdapter::new(
            b.booking,
            state.generation()? as u64,
            DurableReceiptSequence(
                DurableCounterJournal::open(&durable.join(format!("receipt-sequence-{i}")))
                    .map_err(|_| "receipt sequence")?,
            ),
        ));
        spools.push(
            ReceiptSpool::open(&durable.join(format!("spool-{i}"))).map_err(|_| "receipt spool")?,
        );
    }
    let slots: Vec<_> = receivers
        .drain(..)
        .map(|r| Arc::new(Mutex::new((r, Output::default()))))
        .collect();
    let control = conspect_native_exchange_runtime::control::Coordinator::start(
        state.clone(),
        Arc::new(t.clone()),
        slots.clone(),
        deadline.clone(),
        key,
    );

    let mut received = vec![vec![Vec::new(); 6]; 2];
    let mut intervals = [[0; 6]; 2];
    let mut last_media = [[None; 6]; 2];
    let media_started = Instant::now();
    let mut exhausted = false;
    let mut cutoff = false;
    let mut cutoff_phase = 0;
    let mut b_at_expiry = [0; 6];
    let mut cutoff_at = 0;
    let mut accepted = [0u64; 2];
    let mut captured = now().saturating_sub(1000).max((now() / 5000) * 5000);
    for phase in 0..if scenario == "credit-cutoff" {
        600
    } else if scenario == "exhaustion" {
        60
    } else {
        2
    } {
        if scenario == "credit-cutoff" && phase > 0 && phase % 2 == 0 {
            captured = now();
            received = vec![vec![Vec::new(); 6]; 2];
        }
        for (track, f) in fixtures.iter().enumerate() {
            if Instant::now() >= scenario_end {
                return Err("scenario deadline");
            }
            if !control.healthy() {
                return Err("control coordinator denied");
            }
            if scenario == "credit-cutoff" && !cutoff && now() >= state.booking_expiry()? {
                if !state.extension_observed() {
                    return Err("early credit refusal missing");
                }
                state.assert_expired_grant(t)?;
                b_at_expiry = intervals[1];
                cutoff_at = now();
                cutoff = true;
                cutoff_phase = phase;
                control.deactivate(0);
                let mut slot = slots[0].lock().map_err(|_| "receiver lock")?;
                let (receiver, output) = &mut *slot;
                receiver.deny(output).map_err(|_| "expiry durable deny")?;
                if output.pending() != 0 {
                    return Err("expiry queues not empty");
                }
            }
            if now() % 5000 > 3500 {
                std::thread::sleep(Duration::from_millis(5000 - now() % 5000));
            }
            if publisher.bound_scope().epoch != now() / 5000 {
                publisher = Publisher::from_authority(
                    provider.as_ref(),
                    &mut guard,
                    contributor,
                    b.version,
                    b.feed,
                    b.publisher,
                    publisher_instance,
                    now(),
                    Box::new(
                        DurableCounterJournal::open(
                            &durable.join(format!("nonces-{}", now() / 5000)),
                        )
                        .map_err(|_| "epoch journal")?,
                    ),
                )
                .map_err(|_| "epoch authority publisher")?;
                captured = now();
            }
            // The independent bounded grant worker installs the current epoch before media resumes.
            let ready_until = Instant::now() + Duration::from_secs(3);
            loop {
                let mut ready = true;
                for (i, slot) in slots.iter().enumerate() {
                    if (cutoff && (i == 0 || scenario == "withdraw")) || (exhausted && i == 0) {
                        continue;
                    }
                    let mut slot = slot.lock().map_err(|_| "receiver lock")?;
                    let (receiver, output) = &mut *slot;
                    let permitted = receiver.fence(now(), output).is_ok();
                    if !(scenario == "credit-cutoff" && i == 0) {
                        ready &= permitted;
                    }
                }
                if ready {
                    break;
                }
                if !control.healthy() || Instant::now() >= ready_until {
                    return Err("current epoch grant unavailable");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let split = f.packets.len() / 2;
            let range = if phase % 2 == 0 {
                0..split
            } else {
                split..f.packets.len()
            };
            for n in range {
                let id = publish(
                    &mut publisher,
                    &mut tracks[track],
                    &f.name,
                    n as u64 * 20_000,
                    captured,
                    now(),
                    Bytes::copy_from_slice(&f.packets[n]),
                )
                .map_err(|_| "seal lease/epoch")?;
                for i in 0..2 {
                    let mut group = runtime
                        .block_on(async {
                            tokio::time::timeout(
                                Duration::from_secs(2),
                                subscriptions[i][track].recv_group(),
                            )
                            .await
                        })
                        .map_err(|_| "group timeout")?
                        .map_err(|_| "group")?
                        .ok_or("group EOF")?;
                    let frame = runtime
                        .block_on(group.read_frame())
                        .map_err(|_| "frame")?
                        .ok_or("frame EOF")?;
                    let (tx, rx) = &mut links[i];
                    let wire = copy(tx, rx, &frame.payload)?;
                    let mut slot = slots[i].lock().map_err(|_| "receiver lock")?;
                    let (receiver, output) = &mut *slot;
                    let plain = receiver.open(&wire, &id, now(), output);
                    if (cutoff && (i == 0 || scenario == "withdraw")) || (exhausted && i == 0) {
                        if plain.is_ok() {
                            return Err("denied receiver decrypted");
                        }
                        continue;
                    }
                    if scenario == "credit-cutoff" && i == 0 && plain.is_err() {
                        continue;
                    }
                    let frame =
                        hang::container::Frame::decode(Bytes::from(plain.map_err(|_| "decrypt")?))
                            .map_err(|_| "hang decode")?;
                    received[i][track].extend_from_slice(&frame.payload);
                    output.enqueue_encoded(n as u64);
                }
            }
            for (i, _slot) in slots.iter().enumerate() {
                if (cutoff && (i == 0 || scenario == "withdraw")) || (exhausted && i == 0) {
                    continue;
                }
                if scenario == "credit-cutoff" && i == 0 {
                    let mut slot = slots[i].lock().map_err(|_| "receiver lock")?;
                    let (receiver, output) = &mut *slot;
                    if now() >= state.booking_expiry()? || receiver.fence(now(), output).is_err() {
                        continue;
                    }
                }
                for decoded in decode(dir, f, i, &received[i][track])? {
                    let pts = decoded.pts_ms;
                    if last_media[i][track].is_some_and(|old| captured + pts <= old) {
                        continue;
                    }
                    if captured + pts > now() {
                        std::thread::sleep(Duration::from_millis(captured + pts - now()));
                    }
                    let mut slot = slots[i].lock().map_err(|_| "receiver lock")?;
                    let (receiver, output) = &mut *slot;
                    if scenario == "credit-cutoff"
                        && i == 0
                        && (now() >= state.booking_expiry()?
                            || receiver.fence(now(), output).is_err())
                    {
                        break;
                    }
                    last_media[i][track] = Some(captured + pts);
                    if let Some((start, end)) = receiver
                        .decoded(&f.name, captured + pts, now(), output)
                        .map_err(|_| "decoded callback")?
                    {
                        if scenario == "credit-cutoff" && i == 0 && end > state.booking_expiry()? {
                            return Err("decoded media beyond paid boundary");
                        }
                        intervals[i][track] += 1;
                        drop(slot);
                        if track == 0
                            && !(i == 0 && exhausted)
                            && (scenario != "exhaustion" || phase > 0)
                        {
                            let mut id = [0; 16];
                            std::fs::File::open("/dev/urandom")
                                .and_then(|mut f| f.read_exact(&mut id))
                                .map_err(|_| "receipt entropy")?;
                            let mut observation_slot =
                                slots[i].lock().map_err(|_| "receiver lock")?;
                            let (receiver, output) = &mut *observation_slot;
                            receiver
                                .record_decoded(
                                    id,
                                    DecodedObservation {
                                        media_start_ms: start,
                                        media_end_ms: end,
                                        observed_at_ms: now(),
                                        decoded_fresh_advancing: end > start
                                            && now().saturating_sub(end) <= 15000,
                                        queue_age_ms: None,
                                        bitrate_bps: None,
                                        width: decoded.width.ok_or("decoded width missing")?,
                                        height: decoded.height.ok_or("decoded height missing")?,
                                        fps_milli: decoded
                                            .fps
                                            .and_then(|(n, d)| n.checked_mul(1000).map(|n| n / d))
                                            .ok_or("decoded rational fps missing")?,
                                        jitter_ms: None,
                                        backpressure: None,
                                    },
                                    &mut adapters[i],
                                    &mut spools[i],
                                    output,
                                )
                                .map_err(|_| "durable receipt queue")?;
                            drop(observation_slot);
                            for receipt in spools[i]
                                .pending(bindings[i].booking, state.generation()? as u64)
                                .map_err(|_| "pending receipt")?
                            {
                                let o = receipt.observation;
                                let ack=state.receipt(t,i,json!({"receiptId":URL_SAFE_NO_PAD.encode(receipt.receipt_id),"sequence":receipt.sequence,"mediaStartMs":o.media_start_ms,"mediaEndMs":o.media_end_ms,"observedAt":o.observed_at_ms,"usable":receipt.usable,"quality":{"width":o.width,"height":o.height,"fps":o.fps_milli as f64/1000.0}}))?;
                                if ack["sequence"].as_u64() != Some(receipt.sequence)
                                    || ack["chargedMicro"].as_u64().is_none()
                                    || ack["chargedMicroTotal"].as_u64().is_none()
                                    || ack["reservedMicro"].as_u64().is_none()
                                {
                                    return Err("receipt acknowledgement identity");
                                }
                                let total =
                                    ack["chargedMicroTotal"].as_u64().ok_or("charged total")?;
                                let charged =
                                    ack["chargedMicro"].as_u64().ok_or("charged delta")?;
                                let reserved =
                                    ack["reservedMicro"].as_u64().ok_or("reserved balance")?;
                                spools[i]
                                    .acknowledge_accounting(
                                        receipt,
                                        bindings[i].booking,
                                        total,
                                        charged,
                                        reserved,
                                    )
                                    .map_err(|_| "receipt acknowledge")?;
                                accepted[i] += 1;
                                if scenario == "exhaustion"
                                    && i == 0
                                    && ack["stopReason"] == "reservation_exhausted"
                                {
                                    exhausted = true;
                                    if state.grant(t, 0).is_ok() {
                                        return Err("exhaustion failed to deny");
                                    }
                                    control.deactivate(0);
                                    let mut denied_slot =
                                        slots[i].lock().map_err(|_| "receiver lock")?;
                                    let (receiver, output) = &mut *denied_slot;
                                    receiver
                                        .deny(output)
                                        .map_err(|_| "exhaustion durable deny")?;
                                }
                            }
                        }
                        if i == 0 && exhausted {
                            break;
                        }
                        let mut presentation = slots[i].lock().map_err(|_| "receiver lock")?;
                        let (receiver, output) = &mut *presentation;
                        receiver
                            .fence(now(), output)
                            .map_err(|_| "presentation fence")?;
                        output.enqueue_decoded(pts, f.format == "aac");
                    }
                }
            }
        }
        if !cutoff && scenario != "credit-cutoff" && (scenario != "exhaustion" || exhausted) {
            if intervals.iter().any(|t| t.iter().any(|n| *n < 2)) {
                return Err("six-track decode evidence");
            }
            match scenario {
                "release" => state.booking_release(t, 0)?,
                "withdraw" => {
                    state.withdraw(t, m["scenario"]["storyId"].as_str().ok_or("storyId")?)?;
                    cutoff_at = now();
                }
                _ => {}
            };
            if state.grant(t, 0).is_ok() {
                return Err("authority failed to deny");
            }
            cutoff = true;
            cutoff_phase = phase;
            control.deactivate(0);
            let mut slot = slots[0].lock().map_err(|_| "receiver lock")?;
            let (receiver, output) = &mut *slot;
            receiver.deny(output).map_err(|_| "durable deny")?;
            if scenario == "withdraw" {
                if state.grant(t, 1).is_ok() {
                    return Err("withdrawal did not deny second taker");
                }
                control.deactivate(1);
                let mut second = slots[1].lock().map_err(|_| "receiver lock")?;
                let (receiver, output) = &mut *second;
                receiver.deny(output).map_err(|_| "second durable deny")?;
            }
            if output.pending() != 0 {
                return Err("pending media after deny");
            }
        }
        if scenario == "withdraw" && cutoff {
            // A global withdrawal also revokes the publisher lease route by
            // contract: stop sealing at once; the lease must now die (below).
            break;
        }
        if scenario == "credit-cutoff" {
            std::thread::sleep(Duration::from_millis(100));
        }
        if cutoff
            && phase > cutoff_phase
            && (scenario != "credit-cutoff" || media_started.elapsed() >= Duration::from_secs(17))
        {
            break;
        }
    }
    if !cutoff {
        return Err("decoded receipt exhaustion timeout");
    }
    if scenario == "credit-cutoff"
        && intervals[1]
            .iter()
            .zip(b_at_expiry)
            .any(|(after, before)| *after <= before)
    {
        return Err("second receiver did not advance after paid expiry");
    }
    if scenario == "credit-cutoff" && intervals[0].iter().any(|n| *n < 2) {
        return Err("paid-window six-track decode evidence missing");
    }
    if scenario != "withdraw" && intervals[1].iter().any(|n| *n < 4) {
        return Err("second receiver did not continue");
    }
    if scenario == "withdraw" {
        // Terminal publisher cutoff: the next renew must be refused with exactly
        // 404 (the withdrawn story's lease route). Any other denial, or a lease
        // that stays alive, fails the run; arbitrary renew denials are not ignored.
        let withdrawn_at = cutoff_at;
        let until = Instant::now() + Duration::from_secs(6);
        while control.healthy() {
            if Instant::now() >= until {
                return Err("withdrawal did not revoke the publisher lease");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (_, status) = state.publisher_lease()?.last_failure();
        if status != 404 {
            return Err("publisher lease ended for a reason other than the withdrawal 404");
        }
        println!(
            "withdraw report: publisher lease revoked (renew 404) {} ms after the withdrawal was acknowledged",
            now().saturating_sub(withdrawn_at)
        );
    }
    if scenario == "credit-cutoff" {
        // Measured, non-secret report: Unix ms and counts only.
        println!(
            "credit-cutoff report: bookingA expires_at={} extension_409_at={} cutoff_observed_at={} (+{} ms) receipts_accepted A={} B={} B_tracks_advanced_after_cutoff=yes",
            state.booking_expiry()?,
            state.extension_refused_at().unwrap_or(0),
            cutoff_at,
            cutoff_at.saturating_sub(state.booking_expiry()?),
            accepted[0],
            accepted[1]
        );
    }
    println!("connected HTTPS authority / HEVC + five AAC / two takers / TCP CPU observer passed; not QUIC/device");
    Ok(())
}
