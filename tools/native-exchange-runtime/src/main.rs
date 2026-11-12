//! Private connected authority/CPU decoder proof. Not a QUIC/device proof.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bytes::Bytes;
use conspect_media_crypto::{
    authority_http::VerifiedHttpsTransport,
    bootstrap::{private_json, Bootstrap, ROOT_EXPIRED_IN_TRANSIT},
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
/// An authenticated-encrypted object whose epoch grant is not accepted yet.
struct Parked {
    wire: Vec<u8>,
    id: conspect_media_crypto::ObjectId,
    track: usize,
    n: u64,
}
fn accept_plain(
    received: &mut [Vec<u8>],
    track: usize,
    n: u64,
    plain: Vec<u8>,
    output: &mut Output,
) -> Result<()> {
    let frame = hang::container::Frame::decode(Bytes::from(plain)).map_err(|_| "hang decode")?;
    received[track].extend_from_slice(&frame.payload);
    output.enqueue_encoded(n);
    Ok(())
}
/// A parked object can never open once its epoch is behind the receiver's, its
/// grant lapsed without a successor, or it is too old for the replay window.
fn wire_epoch(wire: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(wire.get(52..60)?.try_into().ok()?))
}
fn stale(receiver: &ReceiverRuntime, p: &Parked, now_ms: u64) -> bool {
    let Some(epoch) = wire_epoch(&p.wire) else {
        return false;
    };
    epoch < receiver.epoch()
        || (epoch == receiver.epoch() && receiver.grant_lapsed(now_ms))
        || now_ms.saturating_sub(p.id.captured_ms) >= 4500
}
/// Open parked objects in order. Returns an error only for a denied receiver or a
/// non-timing open failure; stale objects are dropped and counted.
fn drain_parked(
    slot: &Mutex<(ReceiverRuntime, Output)>,
    queue: &mut conspect_native_exchange_runtime::media::ParkQueue<Parked>,
    received: &mut [Vec<u8>],
    lost: &mut [u64; 6],
) -> Result<()> {
    let mut slot = slot.lock().map_err(|_| "receiver lock")?;
    let (receiver, output) = &mut *slot;
    let mut items = queue.take_all().into_iter();
    while let Some(p) = items.next() {
        let at = now();
        if stale(receiver, &p, at) {
            queue.stale += 1;
            lost[p.track] += 1;
            continue;
        }
        let Some(epoch) = wire_epoch(&p.wire) else {
            return Err("decrypt");
        };
        if epoch > receiver.epoch() {
            // Grant still not accepted: keep this and everything after it, in order.
            queue.requeue(p);
            for rest in items.by_ref() {
                queue.requeue(rest);
            }
            return Ok(());
        }
        match receiver.open(&p.wire, &p.id, at, output) {
            Ok(plain) => {
                queue.drained += 1;
                accept_plain(received, p.track, p.n, plain, output)?
            }
            Err(_) if receiver.is_denied() => return Err("decrypt denied"),
            Err(_) if stale(receiver, &p, now()) => {
                queue.stale += 1;
                lost[p.track] += 1;
            }
            Err(_) => return Err("decrypt"),
        }
    }
    Ok(())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
/// Startup retry budget. Each startup phase that may meet a late answer (the
/// mapping mint before the marker, the two receiver grants after the expected
/// fragment) gets at most 3 seconds in which a retry may START. The first
/// attempt always runs; only retries are gated. One attempt lasts at most
/// `ATTEMPT_MAX_MS`: the epoch-tail admission wait (at most 1500 + 250 + 1 ms,
/// bootstrap `admitted_wall`) plus one 1-second transport timeout. No retry
/// starts later than `STARTUP_BOUNDARY_RESERVE_MS` before booking A's
/// authoritative expires_at, so the last retry ends at least 249 ms before A's
/// paid boundary in every fixture:
/// * 4 000 ms window: deadline = booked + 1 000; last retry ends <= booked + 3 751.
/// * 10 000 ms window: deadline = booked + 3 000; ends <= booked + 5 751.
/// * 30 000 ms window: deadline = booked + 3 000; ends <= booked + 5 751.
///
/// (The receiver-grant phase starts its own 3-second budget after the fragment
/// handoff, under the same reserve.) The pre-existing single 404 retry inside a
/// grant (a tail denial, never a refusal) is outside this budget, as on main.
const STARTUP_GRANT_BUDGET_MS: u64 = 3000;
const STARTUP_BOUNDARY_RESERVE_MS: u64 = 3000;
#[cfg(test)]
const ATTEMPT_MAX_MS: u64 = 1500 + 250 + 1 + 1000;
const LATE_GRANT_SETTLE: Duration = Duration::from_millis(250);
fn startup_grant_deadline(expiry: u64, at: u64) -> u64 {
    at.saturating_add(STARTUP_GRANT_BUDGET_MS)
        .min(expiry.saturating_sub(STARTUP_BOUNDARY_RESERVE_MS))
}
/// The only startup failures that are timing, not refusal: a grant answer that
/// did not arrive inside the transport timeout, a 200 grant that arrived too
/// close to its epoch end, and a canonical root answer that the epoch boundary
/// overtook in transit. Everything else (an answered
/// refusal, a malformed answer, an equivocating or rolled-back root, a lease
/// or booking that is gone) is returned at once and never retried.
fn retry_late_grant<T>(deadline: u64, mut attempt: impl FnMut() -> Result<T>) -> Result<T> {
    let mut root_retried = false;
    loop {
        let result = attempt();
        match result {
            Err("grant transport" | "mapping grant transport") if now() < deadline => {
                std::thread::sleep(LATE_GRANT_SETTLE);
            }
            // A 200 grant that arrived so late in its epoch that it expires within
            // the clock uncertainty: a local timing check, not a refusal. The next
            // attempt is admitted out of the tail, in the next epoch.
            Err("grant near boundary") if now() < deadline => {}
            // One retry only: the root fetch itself is admitted out of the epoch
            // tail, so the retry is sent early in the new epoch.
            Err(ROOT_EXPIRED_IN_TRANSIT) if !root_retried && now() < deadline => {
                root_retried = true;
            }
            other => return other,
        }
    }
}
/// The startup publisher is built from the live current-epoch root. Like every
/// later epoch's publisher, it is never requested in the epoch tail, and an
/// answer that the boundary overtook (Expired, with the wall clock now in a
/// later epoch) is asked for exactly once more. Any other provider error
/// (refusal, equivocation, rollback, identity mismatch) stays fatal.
fn startup_publisher(
    mut build: impl FnMut(u64) -> std::result::Result<Publisher, conspect_media_crypto::Error>,
) -> Result<Publisher> {
    for attempt in 0..2 {
        if now() % 5000 > 3500 {
            std::thread::sleep(Duration::from_millis(5000 - now() % 5000));
        }
        let at = now();
        match build(at) {
            Ok(publisher) => return Ok(publisher),
            Err(conspect_media_crypto::Error::Expired)
                if attempt == 0 && now() / 5000 > at / 5000 => {}
            Err(_) => return Err("authority publisher"),
        }
    }
    Err("authority publisher")
}
/// What one taker's accumulated input for one track yields.
#[derive(Debug)]
enum DecoderInput {
    Frames(Vec<conspect_native_exchange_runtime::media::DecodedFrame>),
    /// No accepted bytes at all: nothing to decode, no observation emitted.
    Empty,
    /// The input failed to decode AND objects of this taker/track were lost in
    /// this window to the counted delivery policy (stale or overflowing park
    /// queue, a range cut at the epoch boundary, an authorization fence), or are
    /// still parked: the stream is incomplete by design, not malformed.
    Incomplete,
}
/// The rule for decoder input, for every scenario. An empty input is skipped
/// and counted. A non-empty input that fails to decode is a hard error unless
/// objects of this taker/track were lost or are still parked in this window
/// (`missing > 0`): only then is it counted as incomplete and skipped. A
/// complete, malformed input therefore always fails. Skips never emit a
/// decoded observation or receipt, so a taker that silently has no media
/// still fails `decode_evidence` at the end of the run.
fn decoder_input(
    bytes: &[u8],
    missing: u64,
    decode: impl FnOnce(&[u8]) -> Result<Vec<conspect_native_exchange_runtime::media::DecodedFrame>>,
) -> Result<DecoderInput> {
    if bytes.is_empty() {
        return Ok(DecoderInput::Empty);
    }
    match decode(bytes) {
        Ok(frames) => Ok(DecoderInput::Frames(frames)),
        Err(_) if missing > 0 => Ok(DecoderInput::Incomplete),
        Err(e) => Err(e),
    }
}
/// Per-taker decode evidence at the end of a run. A skipped empty decoder input
/// produces no decoded interval, so a taker that silently received no media
/// for the whole run (or for every phase after A's boundary) fails here.
fn decode_evidence(
    scenario: &str,
    intervals: &[[u64; 6]; 2],
    b_at_expiry: &[u64; 6],
) -> Result<()> {
    if scenario == "credit-cutoff"
        && intervals[1]
            .iter()
            .zip(b_at_expiry)
            .any(|(after, before)| after <= before)
    {
        return Err("second receiver did not advance after paid expiry");
    }
    if scenario == "credit-cutoff" && intervals[0].iter().any(|n| *n < 2) {
        return Err("paid-window six-track decode evidence missing");
    }
    if scenario != "withdraw" && intervals[1].iter().any(|n| *n < 4) {
        return Err("second receiver did not continue");
    }
    Ok(())
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
    conspect_media_crypto::authority_http::status_log_start();
    if let Err(e) = run() {
        eprintln!("{e}");
        // Redacted exchange record: method, route template, status, ms since start.
        for line in conspect_media_crypto::authority_http::status_sequence() {
            eprintln!("http {line}");
        }
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
    let grant_deadline = startup_grant_deadline(state.booking_expiry()?, now());
    state.renew(t)?;
    // A delayed answer here is as legitimate as one during media: without the
    // retry a single late mapping grant, or a root answer overtaken by the
    // epoch boundary, would end the run before A's paid boundary is probed.
    retry_late_grant(grant_deadline, || state.mint_mapping(t))?;
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
    // Derived from the booking row, not a fixed window: A's paid window plus the
    // post-boundary evidence period and slack.
    let scenario_end = Instant::now()
        + Duration::from_secs(90)
        + Duration::from_millis(if scenario == "credit-cutoff" {
            state.booking_expiry()?.saturating_sub(now())
        } else {
            0
        });
    let mut publisher = startup_publisher(|at| {
        Publisher::from_authority(
            provider.as_ref(),
            &mut guard,
            contributor,
            b.version,
            b.feed,
            b.publisher,
            publisher_instance,
            at,
            Box::new(DurableCounterJournal::open(&durable.join("nonces"))?),
        )
    })?;
    // The receivers' first grants get their own startup budget, under the same
    // reserve before A's expires_at: the fragment handoff above may legitimately
    // have used the mapping phase's budget already.
    let grant_deadline = startup_grant_deadline(state.booking_expiry()?, now());
    let mut receivers = Vec::new();
    for (i, b) in bindings.iter().enumerate() {
        let receiver = retry_late_grant(grant_deadline, || {
            let grant = state.grant(t, i)?;
            match ReceiverRuntime::accept(
                &key,
                *b,
                &grant,
                now(),
                0,
                durable.join(format!("receiver-{i}")),
            ) {
                Ok(receiver) => Ok(receiver),
                // A valid grant whose epoch ended between its check and this
                // install (a stalled host): timing, retried like a late grant.
                Err(conspect_media_crypto::Error::Expired) => Err("grant near boundary"),
                Err(_) => Err("signed independent binding"),
            }
        })?;
        receivers.push(receiver);
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
    let mut intervals = [[0u64; 6]; 2];
    let mut last_media = [[None; 6]; 2];
    let media_started = Instant::now();
    let mut exhausted = false;
    let mut cutoff = false;
    let mut cutoff_phase = 0;
    let mut b_at_expiry = [0u64; 6];
    let mut cutoff_at = 0;
    let mut accepted = [0u64; 2];
    let mut late_drops = 0u64;
    let mut sealing_skipped = 0u64;
    let mut empty_decode_skips = [0u64; 2];
    let mut incomplete_decode_skips = [0u64; 2];
    // Objects of each taker/track not delivered in the current input window.
    let mut lost = [[0u64; 6]; 2];
    let mut parked: Vec<conspect_native_exchange_runtime::media::ParkQueue<Parked>> = (0..2)
        .map(|_| conspect_native_exchange_runtime::media::ParkQueue::new(256))
        .collect();
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
            lost = [[0u64; 6]; 2];
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
            // The grant-ready wait can outlast the epoch tail under load: re-check
            // after it and redo the bounded cycle, so a track is never sealed
            // across an epoch boundary (bounded: at most three cycles).
            let mut admitted = false;
            for _ in 0..3 {
                if now() % 5000 > 3500 {
                    std::thread::sleep(Duration::from_millis(5000 - now() % 5000));
                }
                if publisher.bound_scope().epoch != now() / 5000 {
                    let at = now();
                    match Publisher::from_authority(
                        provider.as_ref(),
                        &mut guard,
                        contributor,
                        b.version,
                        b.feed,
                        b.publisher,
                        publisher_instance,
                        at,
                        Box::new(
                            DurableCounterJournal::open(
                                &durable.join(format!("nonces-{}", at / 5000)),
                            )
                            .map_err(|_| "epoch journal")?,
                        ),
                    ) {
                        Ok(next) => publisher = next,
                        // The boundary overtook the root answer in transit: the
                        // bounded admission cycle asks again in the new epoch.
                        Err(conspect_media_crypto::Error::Expired) if now() / 5000 > at / 5000 => {
                            continue;
                        }
                        Err(_) => return Err("epoch authority publisher"),
                    }
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
                        if scenario == "credit-cutoff" && i == 1 {
                            // A late grant is not fatal while the booking is live and the
                            // lease valid: B's objects park until it lands. Only a sticky
                            // deny (authoritative refusal) is fatal.
                            if receiver.is_denied() {
                                return Err("receiver B grant denied");
                            }
                        } else if !(scenario == "credit-cutoff" && i == 0) {
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
                if publisher.bound_scope().epoch == now() / 5000 && now() % 5000 <= 3500 {
                    admitted = true;
                    break;
                }
            }
            if !admitted {
                return Err("epoch admission for sealing not reached");
            }
            let split = f.packets.len() / 2;
            let range = if phase % 2 == 0 {
                0..split
            } else {
                split..f.packets.len()
            };
            let range_end = range.end;
            for n in range {
                // One wall sample decides the epoch check AND the seal, so a
                // boundary cannot fall between them.
                let seal_at = now();
                if scenario == "credit-cutoff" && publisher.bound_scope().epoch != seal_at / 5000 {
                    // The range outlasted its epoch on a loaded host: never seal across
                    // the boundary. The next phase re-admits into the new epoch. The
                    // unsealed remainder is a counted loss for both takers.
                    sealing_skipped += 1;
                    for taker_lost in &mut lost {
                        taker_lost[track] += (range_end - n) as u64;
                    }
                    break;
                }
                let id = publish(
                    &mut publisher,
                    &mut tracks[track],
                    &f.name,
                    n as u64 * 20_000,
                    captured,
                    seal_at,
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
                    let object_epoch = wire_epoch(&wire).ok_or("decrypt")?;
                    let denied_here = (cutoff && (i == 0 || scenario == "withdraw"))
                        || (exhausted && i == 0)
                        || (scenario == "credit-cutoff"
                            && i == 0
                            && now() >= state.booking_expiry()?);
                    if !denied_here
                        && !receiver.is_denied()
                        && (!parked[i].is_empty()
                            || object_epoch > receiver.epoch()
                            || receiver.grant_lapsed(now()))
                    {
                        // Grant for this epoch not accepted yet: hold in order, bounded.
                        if let Some(old) = parked[i].park(Parked {
                            wire,
                            id: id.clone(),
                            track,
                            n: n as u64,
                        }) {
                            lost[i][old.track] += 1;
                        }
                        continue;
                    }
                    let plain = receiver.open(&wire, &id, now(), output);
                    if matches!(plain, Err(conspect_media_crypto::Error::Expired))
                        && !denied_here
                        && !receiver.is_denied()
                        && receiver.grant_lapsed(now())
                    {
                        // The grant lapsed between the check and the open (epoch
                        // boundary crossed mid-loop): not a denial, hold it.
                        if let Some(old) = parked[i].park(Parked {
                            wire,
                            id: id.clone(),
                            track,
                            n: n as u64,
                        }) {
                            lost[i][old.track] += 1;
                        }
                        continue;
                    }
                    if (cutoff && (i == 0 || scenario == "withdraw")) || (exhausted && i == 0) {
                        if plain.is_ok() {
                            return Err("denied receiver decrypted");
                        }
                        continue;
                    }
                    if scenario == "credit-cutoff" && i == 0 && plain.is_err() {
                        // A's authorization fence: not delivered, counted as lost.
                        lost[0][track] += 1;
                        continue;
                    }
                    let frame =
                        hang::container::Frame::decode(Bytes::from(plain.map_err(|_| "decrypt")?))
                            .map_err(|_| "hang decode")?;
                    received[i][track].extend_from_slice(&frame.payload);
                    output.enqueue_encoded(n as u64);
                }
            }
            // Late grant: wait (bounded) for it to land, then open the parked
            // objects in order. A denied receiver is fatal; a late one is not.
            for i in 0..2 {
                if cutoff && (i == 0 || scenario == "withdraw") {
                    parked[i].clear();
                    continue;
                }
                let until = Instant::now() + Duration::from_secs(4);
                while !parked[i].is_empty() {
                    drain_parked(&slots[i], &mut parked[i], &mut received[i], &mut lost[i])?;
                    if parked[i].is_empty() || Instant::now() >= until || !control.healthy() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
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
                let missing =
                    lost[i][track] + parked[i].iter().filter(|p| p.track == track).count() as u64;
                let frames = match decoder_input(&received[i][track], missing, |bytes| {
                    decode(dir, f, i, bytes)
                })? {
                    DecoderInput::Frames(frames) => frames,
                    DecoderInput::Empty => {
                        empty_decode_skips[i] += 1;
                        continue;
                    }
                    DecoderInput::Incomplete => {
                        incomplete_decode_skips[i] += 1;
                        continue;
                    }
                };
                for decoded in frames {
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
                    if scenario == "credit-cutoff"
                        && i == 1
                        && !receiver.is_denied()
                        && receiver.grant_lapsed(now())
                    {
                        // Grant for the new epoch not accepted yet: drop this decoded
                        // frame (counted), never fail the run while the booking is live.
                        late_drops += 1;
                        continue;
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
                        if let Err(_e) = receiver.fence(now(), output) {
                            // B's grant lapsed at an epoch boundary while this decoded
                            // frame waited to be presented: the frame is dropped (the
                            // fence already flushed the queues). Only a sticky deny is fatal.
                            if scenario == "credit-cutoff" && !receiver.is_denied() {
                                late_drops += 1;
                                continue;
                            }
                            return Err("presentation fence");
                        }
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
    // Skipped empty decoder inputs are counted and reported for every scenario.
    println!(
        "decoder input report: empty_inputs_skipped A={} B={} incomplete_inputs_skipped A={} B={}",
        empty_decode_skips[0],
        empty_decode_skips[1],
        incomplete_decode_skips[0],
        incomplete_decode_skips[1]
    );
    if let Err(e) = decode_evidence(scenario, &intervals, &b_at_expiry) {
        // Counts only (no ids, keys or media): why the evidence is missing.
        eprintln!(
            "decode evidence: intervals A={:?} B={:?} B_at_expiry={b_at_expiry:?} empty={empty_decode_skips:?} incomplete={incomplete_decode_skips:?} late_drops={late_drops} ranges_cut={sealing_skipped} B_parked={} B_stale={} B_drained={} B_overflow={}",
            intervals[0], intervals[1], parked[1].parked_total, parked[1].stale, parked[1].drained, parked[1].dropped
        );
        return Err(e);
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
        // The withdrawn story's lease ends with exactly a 404: either the next
        // renew answer, or the root route's 404 that latched the lease first
        // (the grant worker may reach the revoked route before the renew thread).
        let lease = state.publisher_lease()?;
        let route = match (lease.last_failure().1, lease.revoked_by()) {
            (404, _) => "renew 404",
            (_, 404) => "root-route 404",
            _ => return Err("publisher lease ended for a reason other than the withdrawal 404"),
        };
        println!(
            "withdraw report: publisher lease revoked ({route}) {} ms after the withdrawal was acknowledged",
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
        println!(
            "credit-cutoff B late-grant report: parked={} dropped_on_overflow={} stale={} drained={} decoded_frames_dropped_at_boundary={late_drops} ranges_cut_at_epoch_boundary={sealing_skipped} empty_decode_skips={}",
            parked[1].parked_total, parked[1].dropped, parked[1].stale, parked[1].drained, empty_decode_skips[1]
        );
    }
    println!("connected HTTPS authority / HEVC + five AAC / two takers / TCP CPU observer passed; not QUIC/device");
    Ok(())
}

#[cfg(test)]
mod startup_grant_tests {
    use super::*;
    #[test]
    fn shared_deadline_reserves_the_paid_boundary_in_every_fixture() {
        for window in [4_000, 10_000, 30_000] {
            let booked_at = 100_000;
            let expiry = booked_at + window;
            let deadline = startup_grant_deadline(expiry, booked_at);
            // The last retry starts before the deadline and ends before expiry.
            assert!(deadline + ATTEMPT_MAX_MS < expiry, "window {window}");
            assert_eq!(deadline, booked_at + 3_000.min(window - 3_000));
        }
        assert_eq!(startup_grant_deadline(110_000, 109_000), 107_000);
    }
    #[test]
    fn first_attempt_runs_even_after_the_deadline() {
        let mut calls = 0;
        let r: Result<()> = retry_late_grant(0, || {
            calls += 1;
            Err("grant transport")
        });
        assert_eq!((r, calls), (Err("grant transport"), 1));
    }
    #[test]
    fn timing_failures_are_retried_and_refusals_are_not() {
        for timing in [
            "grant transport",
            "mapping grant transport",
            "grant near boundary",
        ] {
            let mut calls = 0;
            let r = retry_late_grant(now() + 60_000, || {
                calls += 1;
                if calls < 3 {
                    Err(timing)
                } else {
                    Ok(calls)
                }
            });
            assert_eq!(r, Ok(3), "{timing}");
        }
        for refusal in [
            "mapping grant rejected",
            "grant denied",
            "canonical root rejected",
            "lease expired",
            "booking expired",
            "signed independent binding",
            "grant metadata rejected",
        ] {
            let mut calls = 0;
            let r: Result<()> = retry_late_grant(now() + 60_000, || {
                calls += 1;
                Err(refusal)
            });
            assert_eq!((r, calls), (Err(refusal), 1), "{refusal}");
        }
    }
    #[test]
    fn root_overtaken_in_transit_is_retried_exactly_once() {
        let mut calls = 0;
        let r: Result<()> = retry_late_grant(now() + 60_000, || {
            calls += 1;
            Err(ROOT_EXPIRED_IN_TRANSIT)
        });
        assert_eq!((r, calls), (Err(ROOT_EXPIRED_IN_TRANSIT), 2));
    }
    #[test]
    fn startup_publisher_retries_only_a_boundary_crossing_expiry() {
        use conspect_media_crypto::Error;
        let fatal: [fn() -> Error; 4] = [
            || Error::Denied,
            || Error::Replay,
            || Error::Invalid,
            || Error::Crypto,
        ];
        for (n, e) in fatal.iter().enumerate() {
            let mut calls = 0;
            assert!(startup_publisher(|_| {
                calls += 1;
                Err(e())
            })
            .is_err());
            assert_eq!(calls, 1, "fatal error #{n}");
        }
        // Expired without an epoch crossing (a wrong-epoch answer): fatal, once.
        let mut calls = 0;
        assert!(startup_publisher(|_| {
            calls += 1;
            Err(Error::Expired)
        })
        .is_err());
        assert_eq!(calls, 1);
    }
    #[test]
    fn decoder_input_rule() {
        use conspect_native_exchange_runtime::media::{decode, Fixture};
        let dir = tempfile::tempdir().unwrap();
        let f = Fixture {
            name: "garbage".into(),
            format: "hevc",
            packets: vec![],
        };
        let garbage = [0x5au8; 4096];
        let real = |b: &[u8]| decode(dir.path(), &f, 1, b);
        // Empty: skipped, decoder never invoked.
        assert!(matches!(
            decoder_input(&[], 0, |_| panic!("decoder ran on empty input")),
            Ok(DecoderInput::Empty)
        ));
        // Complete but malformed: a hard failure.
        assert_eq!(
            decoder_input(&garbage, 0, real).unwrap_err(),
            "FFmpeg command failed"
        );
        // Undecodable with counted losses in this window: incomplete, skipped.
        assert!(matches!(
            decoder_input(&garbage, 3, real),
            Ok(DecoderInput::Incomplete)
        ));
        // Decodable input is decoded whatever was lost.
        assert!(matches!(
            decoder_input(&garbage, 3, |_| Ok(vec![])),
            Ok(DecoderInput::Frames(v)) if v.is_empty()
        ));
    }
    #[test]
    fn a_taker_without_media_fails_the_run_evidence() {
        let full = [[5u64; 6]; 2];
        assert_eq!(decode_evidence("credit-cutoff", &full, &[1; 6]), Ok(()));
        assert_eq!(decode_evidence("release", &full, &[0; 6]), Ok(()));
        // B silently skipped every input: no interval at all.
        let no_b = [[5; 6], [0; 6]];
        assert!(decode_evidence("credit-cutoff", &no_b, &[0; 6]).is_err());
        assert!(decode_evidence("release", &no_b, &[0; 6]).is_err());
        // A silently skipped every input in its paid window.
        let no_a = [[0; 6], [5; 6]];
        assert_eq!(
            decode_evidence("credit-cutoff", &no_a, &[1; 6]),
            Err("paid-window six-track decode evidence missing")
        );
        // B decoded before A's boundary but nothing after it.
        assert_eq!(
            decode_evidence("credit-cutoff", &full, &[5; 6]),
            Err("second receiver did not advance after paid expiry")
        );
    }
}
