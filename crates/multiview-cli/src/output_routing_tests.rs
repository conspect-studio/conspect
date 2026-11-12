#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Unit harness: a real `RenditionDispatcher`, fake encoders.

use super::*;
use crate::rendition_dispatch::{BindingConfig, DispatchConfig};
use multiview_compositor::pipeline::CanvasColor;
use std::sync::atomic::AtomicUsize;

const CADENCE: Rational = Rational::new(5, 1);

/// A fake live encoder: one packet per tick, keyframe when forced or first.
struct FakeEnc {
    forced: bool,
    encoded: Arc<AtomicUsize>,
}

fn pkt(tick: u64, key: bool) -> EncodedPacket {
    use ffmpeg_next::codec::packet::{Flags, Packet};
    let mut raw = if key {
        let mut b = vec![0_u8; 77];
        b[..6].copy_from_slice(&[0, 0, 0, 1, 0x65, 0x88]);
        Packet::copy(&b)
    } else {
        Packet::new(8)
    };
    let t = i64::try_from(tick).unwrap();
    raw.set_pts(Some(t));
    raw.set_dts(Some(t));
    if key {
        raw.set_flags(Flags::KEY);
    }
    EncodedPacket::with_kind(raw, multiview_ffmpeg::StreamKind::Video)
}

impl VideoEncodeWorker for FakeEnc {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.encoded.fetch_add(1, Ordering::Relaxed);
        let key = std::mem::take(&mut self.forced);
        Ok(vec![pkt(item.tick, key)])
    }
    fn reorder_bound(&self) -> ReorderBound {
        ReorderBound::NoReorder
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        Ok(Vec::new())
    }
    fn force_keyframe(&mut self) {
        self.forced = true;
    }
}

impl LiveEncoder for FakeEnc {
    fn start_at(&mut self, _t0: u64) {
        self.forced = true;
    }
    fn time_base(&self) -> Rational {
        Rational::new(1, 5)
    }
    fn move_gate(&self) -> MoveGate {
        Box::new(|p| p.is_keyframe() && p.len() == 77)
    }
}

fn canvas() -> Arc<Nv12Image> {
    Arc::new(Nv12Image::solid(16, 16, 16, 128, 128, CanvasColor::default().output_tag()).unwrap())
}

#[test]
fn gap_fill_encodes_missing_ticks_and_fails_beyond_the_bound() {
    let encoded = Arc::new(AtomicUsize::new(0));
    let mut w = GapFill {
        inner: Box::new(FakeEnc {
            forced: false,
            encoded: Arc::clone(&encoded),
        }),
        force_at: Arc::new(AtomicU64::new(NO_FORCE)),
        next: None,
        last: None,
    };
    let out = w
        .encode(BakedTick {
            tick: 10,
            canvas: canvas(),
        })
        .unwrap();
    assert_eq!(out.len(), 1);
    let out = w
        .encode(BakedTick {
            tick: 14,
            canvas: canvas(),
        })
        .unwrap();
    let ticks: Vec<_> = out.iter().filter_map(EncodedPacket::pts).collect();
    assert_eq!(
        ticks,
        vec![11, 12, 13, 14],
        "consecutive ticks to the encoder"
    );
    assert!(w
        .encode(BakedTick {
            tick: 14 + MAX_GAP_FILL_TICKS + 2,
            canvas: canvas()
        })
        .is_err());
}

#[test]
fn force_at_requests_one_keyframe_at_or_after_the_tick() {
    let encoded = Arc::new(AtomicUsize::new(0));
    let (mut w, cell) = ForceAt::new(Box::new(FakeEnc {
        forced: false,
        encoded,
    }));
    cell.store(3, Ordering::Release);
    let keys: Vec<bool> = (0..6)
        .map(|t| {
            w.encode(BakedTick {
                tick: t,
                canvas: canvas(),
            })
            .unwrap()[0]
                .is_keyframe()
        })
        .collect();
    assert_eq!(keys, vec![false, false, false, true, false, false]);
}

fn dispatcher(outputs: usize) -> RenditionDispatcher {
    RenditionDispatcher::new(DispatchConfig {
        video_count: 1,
        audio_count: 0,
        video_time_base: vec![DtsTimeBase { num: 1, den: 5 }],
        audio_time_base: Vec::new(),
        bindings: (0..outputs)
            .map(|i| BindingConfig {
                output_index: i,
                video_key: Some(0),
                audio_tracks: Vec::new(),
                inband_parameter_changes_ok: true,
            })
            .collect(),
        first_tick: 0,
        cadence: CADENCE,
        video_move_gates: vec![Some(Box::new(|p: &EncodedPacket| p.is_keyframe()))],
    })
    .unwrap()
}

fn router(
    outputs: usize,
    max: usize,
    open: OpenFn,
) -> (OutputRouter, OutputRoutingHandle, AuxWiring) {
    let wiring = || AuxWiring {
        feeds: Arc::new(SwapCell::from_pointee(Vec::new())),
        active: Arc::new(AtomicBool::new(false)),
        failed: Arc::new(AtomicBool::new(false)),
        dropped: Arc::new(AtomicU64::new(0)),
        sent: Arc::new(AtomicU64::new(0)),
        stores: Arc::new(|_: &str| None),
        preview_selection: None,
    };
    let aux = wiring();
    let mirror = AuxWiring {
        feeds: Arc::clone(&aux.feeds),
        active: Arc::clone(&aux.active),
        failed: Arc::clone(&aux.failed),
        dropped: Arc::clone(&aux.dropped),
        sent: Arc::clone(&aux.sent),
        stores: Arc::clone(&aux.stores),
        preview_selection: None,
    };
    let (r, h) = OutputRouter::new(
        RouterConfig {
            cadence: CADENCE,
            gop: 5,
            base_keys: vec![0; outputs],
            base_force: vec![Arc::new(AtomicU64::new(NO_FORCE))],
            max_encoders: max,
            max_per_principal: DEFAULT_MAX_PER_PRINCIPAL,
            has_preview: false,
        },
        aux,
        open,
    )
    .unwrap();
    (r, h, mirror)
}

struct NoDelivery;
impl WindowDelivery for NoDelivery {
    fn offer_window(
        &mut self,
        _o: usize,
        _g: u64,
        _p: Vec<EncodedPacket>,
        _s: std::time::Duration,
        _n: Instant,
    ) -> Result<(), multiview_output::batch::EmissionError> {
        Ok(())
    }
    fn report_loss(&mut self, _o: usize, _r: multiview_output::batch::OfferError) {}
    fn mark_discontinuity(&mut self, _o: usize, _r: multiview_output::batch::OfferError) {}
    fn take_discontinuity(&mut self, _o: usize) -> Option<multiview_output::batch::Discontinuity> {
        None
    }
    fn generation(&self, _o: usize) -> u64 {
        0
    }
    fn statuses(&self) -> Vec<crate::binding_delivery::BindingStatus> {
        Vec::new()
    }
    fn finish(&mut self) {}
    fn abort(&mut self) {}
}

fn take(op: &str, output: usize, bus: PictureBus) -> OutputTake {
    OutputTake {
        operation_id: op.to_owned(),
        output_index: output,
        bus,
        principal: "p".to_owned(),
    }
}

#[test]
fn no_take_means_no_aux_activity_and_no_outcomes() {
    let (mut r, h, aux) = router(2, 6, Box::new(|_| Err("never".to_owned())));
    let mut d = dispatcher(2);
    for t in 0..10 {
        r.on_tick(t, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
            .unwrap();
    }
    assert!(
        !aux.active.load(Ordering::Acquire),
        "clock gate stays closed"
    );
    assert!(h.outcomes().unwrap().try_recv().is_err());
    assert_eq!(d.video_key_count(), 1, "no encoder opened");
}

#[test]
fn unroutable_and_unfresh_takes_are_refused_and_the_mailbox_bounds_floods() {
    let (mut r, h, aux) = router(2, 6, Box::new(|_| Err("never".to_owned())));
    let outcomes = h.outcomes().unwrap();
    let mut d = dispatcher(2);
    h.try_take(take("bad", 9, PictureBus::Program)).unwrap();
    h.try_take(take("pvw", 0, PictureBus::Preview)).unwrap();
    h.try_take(take("src", 1, PictureBus::Source("cam".to_owned())))
        .unwrap();
    r.on_tick(0, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
        .unwrap();
    let first: Vec<_> = outcomes
        .try_iter()
        .map(|o| (o.operation_id, o.outcome))
        .collect();
    assert_eq!(
        first,
        vec![
            (
                "bad".to_owned(),
                TakeResult::Refused(RefuseReason::Unroutable)
            ),
            (
                "pvw".to_owned(),
                TakeResult::Refused(RefuseReason::Unroutable)
            ),
        ]
    );
    assert!(
        aux.active.load(Ordering::Acquire),
        "a pending source take activates aux"
    );
    for t in 1..=TAKE_PRIME_DEADLINE_TICKS {
        r.on_tick(t, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
            .unwrap();
    }
    let late: Vec<_> = outcomes.try_iter().map(|o| (o.tick, o.outcome)).collect();
    assert_eq!(
        late,
        vec![(
            TAKE_PRIME_DEADLINE_TICKS,
            TakeResult::Refused(RefuseReason::FeedNotFresh)
        )]
    );
    assert_eq!(d.binding_video_key(1), Some(0), "output kept its feed");
    assert!(
        !aux.active.load(Ordering::Acquire),
        "gate closes when nothing is routed"
    );
    // Flood: the mailbox holds 64; the 65th is busy.
    let accepted = (0..1000)
        .filter(|i| {
            h.try_take(take(&i.to_string(), i % 2, PictureBus::Program))
                .is_ok()
        })
        .count();
    assert_eq!(accepted, TAKE_MAILBOX);
    r.on_tick(100, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
        .unwrap();
    let applied = outcomes
        .try_iter()
        .filter(|o| o.outcome == TakeResult::Applied)
        .count();
    assert_eq!(applied, 2, "at most one effective take per output per tick");
}

/// Records each delivered window: (output, [(pts, keyframe-with-params)]).
#[derive(Default)]
struct Rec {
    windows: Vec<(usize, Vec<(i64, bool)>)>,
}
impl WindowDelivery for Rec {
    fn offer_window(
        &mut self,
        o: usize,
        _g: u64,
        p: Vec<EncodedPacket>,
        _s: std::time::Duration,
        _n: Instant,
    ) -> Result<(), multiview_output::batch::EmissionError> {
        let v = p
            .iter()
            .map(|p| (p.pts().unwrap_or(-1), p.is_keyframe() && p.len() == 77))
            .collect();
        self.windows.push((o, v));
        Ok(())
    }
    fn report_loss(&mut self, _o: usize, _r: multiview_output::batch::OfferError) {}
    fn mark_discontinuity(&mut self, _o: usize, _r: multiview_output::batch::OfferError) {}
    fn take_discontinuity(&mut self, _o: usize) -> Option<multiview_output::batch::Discontinuity> {
        None
    }
    fn generation(&self, _o: usize) -> u64 {
        0
    }
    fn statuses(&self) -> Vec<crate::binding_delivery::BindingStatus> {
        Vec::new()
    }
    fn finish(&mut self) {}
    fn abort(&mut self) {}
}

const TICK_NS: i128 = 200_000_000;

fn pts(t: u64) -> MediaTime {
    MediaTime::from_nanos(i64::try_from(t).unwrap() * 200_000_000)
}

struct Harness {
    r: OutputRouter,
    h: OutputRoutingHandle,
    d: RenditionDispatcher,
    rec: Rec,
    store: Arc<TileStore<Nv12Image>>,
    failed: Arc<AtomicBool>,
    outcomes: Receiver<TakeOutcome>,
    aux_publishes: bool,
    encoded: Arc<AtomicUsize>,
    store2: Arc<TileStore<Nv12Image>>,
    base_force: Arc<AtomicU64>,
    live: bool,
    max_router: std::time::Duration,
}

fn harness(outputs: usize, max: usize, open: Option<OpenFn>) -> Harness {
    let store = Arc::new(TileStore::<Nv12Image>::with_defaults("cam"));
    let store2 = Arc::new(TileStore::<Nv12Image>::with_defaults("cam2"));
    let lookup = Arc::clone(&store);
    let lookup2 = Arc::clone(&store2);
    let base_force = Arc::new(AtomicU64::new(NO_FORCE));
    let failed = Arc::new(AtomicBool::new(false));
    let encoded = Arc::new(AtomicUsize::new(0));
    let enc2 = Arc::clone(&encoded);
    let open = open.unwrap_or_else(|| {
        Box::new(move |_| {
            let enc: Box<dyn LiveEncoder> = Box::new(FakeEnc {
                forced: false,
                encoded: Arc::clone(&enc2),
            });
            Ok(enc)
        })
    });
    let (r, h) = OutputRouter::new(
        RouterConfig {
            cadence: CADENCE,
            gop: 5,
            base_keys: vec![0; outputs],
            base_force: vec![Arc::clone(&base_force)],
            max_encoders: max,
            max_per_principal: DEFAULT_MAX_PER_PRINCIPAL,
            has_preview: false,
        },
        AuxWiring {
            feeds: Arc::new(SwapCell::from_pointee(Vec::new())),
            active: Arc::new(AtomicBool::new(false)),
            failed: Arc::clone(&failed),
            dropped: Arc::new(AtomicU64::new(0)),
            sent: Arc::new(AtomicU64::new(0)),
            stores: Arc::new(move |id: &str| match id {
                "cam" => Some(Arc::clone(&lookup)),
                "cam2" => Some(Arc::clone(&lookup2)),
                _ => None,
            }),
            preview_selection: None,
        },
        open,
    )
    .unwrap();
    let outcomes = h.outcomes().unwrap();
    Harness {
        r,
        h,
        d: dispatcher(outputs),
        rec: Rec::default(),
        store,
        failed,
        outcomes,
        aux_publishes: true,
        encoded,
        store2,
        base_force,
        live: true,
        max_router: std::time::Duration::ZERO,
    }
}

impl Harness {
    /// One Program tick, in the pipeline's order: live events, base events
    /// (the base encoder emits tick t-1's packets one iteration later),
    /// router, then the base workers are offered frame t.
    fn tick(&mut self, t: u64) {
        self.r
            .drain_live_events(&mut self.d, &mut self.rec)
            .unwrap();
        if let Some(prev) = t.checked_sub(1) {
            let ns = i128::from(prev + 1) * TICK_NS;
            let forced = take_force(&self.base_force, prev);
            self.d
                .dispatch(
                    DispatchEvent::Video {
                        key_index: 0,
                        tick: prev,
                        timebase: Rational::new(1, 5),
                        packets: vec![pkt(prev, forced || prev % 5 == 0)],
                        watermark: Some(crate::rendition_workers::DtsWatermark {
                            complete_before_ns: ns,
                        }),
                    },
                    Instant::now(),
                    &mut self.rec,
                )
                .unwrap();
        }
        let frame = Nv12Image::solid(16, 16, 16, 128, 128, CanvasColor::default().output_tag());
        for store in [&self.store, &self.store2] {
            if self.live {
                store.publish(frame.clone().unwrap(), pts(t));
            }
        }
        if self.aux_publishes {
            for bus in [src(), PictureBus::Source("cam2".to_owned())] {
                if let Some(slot) = self.r.slot(&bus) {
                    slot.latest
                        .store(Some(Arc::new(crate::routing_aux::AuxFrame {
                            tick: t,
                            generation: 1,
                            frame: canvas(),
                        })));
                }
            }
        }
        let started = Instant::now();
        self.r
            .on_tick(t, pts(t), &mut self.d, &mut self.rec)
            .unwrap();
        self.max_router = self.max_router.max(started.elapsed());
        // A live worker encodes within the tick period.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    fn windows_of(&self, o: usize) -> Vec<&Vec<(i64, bool)>> {
        self.rec
            .windows
            .iter()
            .filter(|w| w.0 == o)
            .map(|w| &w.1)
            .collect()
    }
    fn results(&self) -> Vec<TakeResult> {
        self.outcomes.try_iter().map(|o| o.outcome).collect()
    }
}

fn src() -> PictureBus {
    PictureBus::Source("cam".to_owned())
}

#[test]
fn a_take_opens_one_encoder_moves_at_a_rap_and_lingers_out() {
    let mut x = harness(2, 6, None);
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("a", 1, src())).unwrap();
    let mut applied_at = None;
    let mut now = 5;
    while now < 5 + OPEN_DEADLINE_TICKS && applied_at.is_none() {
        x.tick(now);
        applied_at = x
            .outcomes
            .try_iter()
            .find(|o| o.outcome == TakeResult::Applied)
            .map(|o| o.tick);
        now += 1;
    }
    let at = applied_at.expect("applied within the open deadline");
    assert_eq!(x.d.video_key_count(), 2, "+1 encoder");
    assert_eq!(x.d.binding_video_key(1), Some(1));
    assert_eq!(
        x.d.binding_video_key(0),
        Some(0),
        "the other output stays on Program"
    );
    let o1 = x.windows_of(1);
    let first_moved = o1
        .iter()
        .find(|w| w.first().is_some_and(|p| p.0 == i64::try_from(at).unwrap()))
        .unwrap();
    assert!(
        first_moved[0].1,
        "the moved output's first packet is a self-contained RAP"
    );
    let ptss: Vec<i64> = o1.iter().filter_map(|w| w.first().map(|p| p.0)).collect();
    assert!(
        ptss.windows(2).all(|w| w[1] == w[0] + 1),
        "PTS step constant: {ptss:?}"
    );
    let o0: Vec<i64> = x
        .windows_of(0)
        .iter()
        .filter_map(|w| w.first().map(|p| p.0))
        .collect();
    assert!(o0.windows(2).all(|w| w[1] == w[0] + 1) && o0.len() >= usize::try_from(at).unwrap());
    // Back to Program: +0 encoders.
    x.h.try_take(take("b", 1, PictureBus::Program)).unwrap();
    let mut back = None;
    for t in now..now + 40 {
        x.tick(t);
        if x.outcomes
            .try_iter()
            .any(|o| o.operation_id == "b" && o.outcome == TakeResult::Applied)
        {
            back = Some(t);
            break;
        }
    }
    let back = back.expect("back on Program");
    assert_eq!(x.d.binding_video_key(1), Some(0));
    assert_eq!(x.d.video_key_count(), 2, "+0 encoders back to Program");
    assert_eq!(x.h.snapshot().dynamic_encoders_live, 1, "lingering");
    for t in back + 1..=back + LINGER_TICKS + 1 {
        x.tick(t);
    }
    assert_eq!(
        x.h.snapshot().dynamic_encoders_live,
        0,
        "closed after the linger"
    );
    assert!(x.d.is_key_failed(1), "retired");
    let before = x.encoded.load(Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        x.encoded.load(Ordering::Relaxed),
        before,
        "reaped worker encodes nothing more"
    );
}

#[test]
fn a_wedged_open_never_delays_program_and_is_refused_at_its_deadline() {
    let (_hold, wait) = mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    let open: OpenFn = Box::new(move |_| {
        let _ = wait.lock().unwrap().recv(); // never returns while the test runs
        Err("unreachable".to_owned())
    });
    let mut x = harness(2, 6, Some(open));
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    for t in 1..=OPEN_DEADLINE_TICKS + 3 {
        let started = Instant::now();
        x.tick(t);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "consumer never waits"
        );
        // Every output's window t-? closes on time: one window per output per tick.
        assert_eq!(
            x.windows_of(0).len(),
            usize::try_from(t).unwrap(),
            "late/missing window at {t}"
        );
        assert_eq!(x.windows_of(1).len(), usize::try_from(t).unwrap());
    }
    assert_eq!(
        x.results(),
        vec![TakeResult::Refused(RefuseReason::EncoderNotReady)]
    );
    assert_eq!(
        x.d.binding_video_key(1),
        Some(0),
        "the output keeps its feed"
    );
    assert_eq!(x.h.snapshot().opens_in_flight, 0, "reservation released");
}

#[test]
fn two_takes_against_the_last_free_slot_admit_one() {
    let mut x = harness(3, 1, None);
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    x.tick(1);
    let mut y = take("b", 2, PictureBus::Source("cam2".to_owned()));
    y.principal = "q".to_owned();
    x.h.try_take(y).unwrap();
    let mut got = Vec::new();
    for t in 2..40 {
        x.tick(t);
        got.extend(x.outcomes.try_iter().map(|o| (o.operation_id, o.outcome)));
    }
    assert!(
        got.contains(&("a".to_owned(), TakeResult::Applied)),
        "{got:?}"
    );
    assert!(
        got.contains(&("b".to_owned(), TakeResult::Refused(RefuseReason::Capacity))),
        "{got:?}"
    );
    assert_eq!(x.d.video_key_count(), 2, "exactly one encoder opened");
}

#[test]
fn a_wedged_aux_degrades_then_falls_back_and_a_dead_aux_falls_back_at_once() {
    let mut x = harness(2, 6, None);
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    let mut t = 1;
    while !x
        .outcomes
        .try_iter()
        .any(|o| o.outcome == TakeResult::Applied)
    {
        x.tick(t);
        t += 1;
        assert!(t < 60);
    }
    x.aux_publishes = false;
    let wedged_at = t;
    let mut degraded_at = None;
    let mut fell_back_at = None;
    for _ in 0..FALLBACK_AFTER_TICKS + 20 {
        x.tick(t);
        if degraded_at.is_none() && x.h.snapshot().outputs[1].degraded {
            degraded_at = Some(t);
        }
        if x.outcomes
            .try_iter()
            .any(|o| o.outcome == TakeResult::Fallback(FallbackReason::AuxFailed))
        {
            fell_back_at = Some(t);
            break;
        }
        t += 1;
    }
    let held = |at: Option<u64>| at.map(|a| a - wedged_at + 1);
    assert_eq!(
        held(degraded_at),
        Some(DEGRADED_AFTER_TICKS),
        "degraded at 60 held ticks"
    );
    assert_eq!(
        held(fell_back_at),
        Some(FALLBACK_AFTER_TICKS),
        "fallback at 150"
    );
    for _ in 0..10 {
        t += 1;
        x.tick(t);
    }
    assert_eq!(x.d.binding_video_key(1), Some(0), "back on Program");
    assert_eq!(
        x.windows_of(0).len(),
        usize::try_from(t).unwrap(),
        "Program unaffected"
    );
    // Dead aux: a fresh take refused FeedNotFresh, an applied one falls back at once.
    x.aux_publishes = true;
    x.h.try_take(take("c", 1, src())).unwrap();
    while !x
        .outcomes
        .try_iter()
        .any(|o| o.outcome == TakeResult::Applied)
    {
        t += 1;
        x.tick(t);
    }
    x.failed.store(true, Ordering::Release);
    t += 1;
    x.tick(t);
    assert_eq!(
        x.results(),
        vec![TakeResult::Fallback(FallbackReason::AuxFailed)]
    );
    x.h.try_take(take("d", 1, src())).unwrap();
    for _ in 0..=TAKE_PRIME_DEADLINE_TICKS {
        t += 1;
        x.tick(t);
    }
    assert_eq!(
        x.results(),
        vec![TakeResult::Refused(RefuseReason::FeedNotFresh)]
    );
}

/// Test 2 (slow open): the encoder opens after 2 s, far past the 90-tick
/// deadline of this harness. Bounds: every Program window closes on its tick
/// (one window per output per tick), no tick of the consumer waits > 200 ms,
/// the take is refused at exactly its deadline, the late encoder is reaped
/// (never registered: still 1 key) and the output keeps its feed.
#[test]
fn a_slow_open_never_delays_program_and_its_late_encoder_is_reaped() {
    let encoded = Arc::new(AtomicUsize::new(0));
    let enc = Arc::clone(&encoded);
    let open: OpenFn = Box::new(move |_| {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let e: Box<dyn LiveEncoder> = Box::new(FakeEnc {
            forced: false,
            encoded: Arc::clone(&enc),
        });
        Ok(e)
    });
    let mut x = harness(2, 6, Some(open));
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    let mut refused_at = None;
    let mut t = 1;
    while t <= OPEN_DEADLINE_TICKS + 3 {
        let started = Instant::now();
        x.tick(t);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "consumer never waits"
        );
        assert_eq!(
            x.windows_of(0).len(),
            usize::try_from(t).unwrap(),
            "window at {t}"
        );
        assert_eq!(x.windows_of(1).len(), usize::try_from(t).unwrap());
        if let Some(o) = x.outcomes.try_iter().next() {
            assert_eq!(
                o.outcome,
                TakeResult::Refused(RefuseReason::EncoderNotReady)
            );
            refused_at = Some(o.tick);
        }
        t += 1;
    }
    // Admitted at tick 2 (tick 1 primes: the slot gets its first frame).
    let refused_at = refused_at.expect("refused at the deadline");
    assert!(
        (OPEN_DEADLINE_TICKS..=OPEN_DEADLINE_TICKS + 2).contains(&refused_at),
        "refused at the deadline, got {refused_at}"
    );
    // Let the open finish; its encoder arrives late and goes to the reaper.
    let open_started = Instant::now();
    while open_started.elapsed() < std::time::Duration::from_millis(2300) {
        x.tick(t);
        t += 1;
    }
    assert_eq!(
        x.d.video_key_count(),
        1,
        "the late encoder is never registered"
    );
    assert_eq!(
        x.d.binding_video_key(1),
        Some(0),
        "the output keeps its feed"
    );
    assert_eq!(x.h.snapshot().opens_in_flight, 0);
    assert_eq!(
        encoded.load(Ordering::Relaxed),
        0,
        "the late encoder never encodes"
    );
    assert_eq!(x.windows_of(0).len(), usize::try_from(t - 1).unwrap());
}

/// Test 5 (flapping source): the source is live for 60 ticks, dark for 60,
/// four times. A take issued in a dark phase (5 ticks after going dark, past
/// the 500 ms hold) is refused `FeedNotFresh` and never applied, even when the
/// encoder for that bus already lingers (+0 path); takes in live phases apply.
#[test]
fn a_flapping_source_never_gets_a_take_while_not_fresh() {
    let mut x = harness(2, 6, None);
    let mut results: Vec<(String, TakeResult)> = Vec::new();
    for t in 0..240_u64 {
        let phase = t / 60;
        let at = t % 60;
        x.live = phase % 2 == 0;
        if at == 5 {
            x.h.try_take(take(&format!("src{phase}"), 1, src()))
                .unwrap();
        }
        if x.live && at == 45 {
            x.h.try_take(take(&format!("pgm{phase}"), 1, PictureBus::Program))
                .unwrap();
        }
        x.tick(t);
        for o in x.outcomes.try_iter() {
            if !x.live && o.outcome == TakeResult::Applied {
                assert_ne!(
                    x.d.binding_video_key(1),
                    Some(1),
                    "applied while dark at {t}"
                );
            }
            results.push((o.operation_id, o.outcome));
        }
        if !x.live && at > 3 {
            assert_eq!(
                x.d.binding_video_key(1),
                Some(0),
                "dark phase stays on Program at {t}"
            );
        }
    }
    let of = |id: &str| -> Vec<TakeResult> {
        results
            .iter()
            .filter(|(op, _)| op == id)
            .map(|(_, r)| *r)
            .collect()
    };
    for live in ["src0", "src2", "pgm0", "pgm2"] {
        assert_eq!(of(live), vec![TakeResult::Applied], "{live}: {results:?}");
    }
    for dark in ["src1", "src3"] {
        assert_eq!(
            of(dark),
            vec![TakeResult::Refused(RefuseReason::FeedNotFresh)],
            "{dark}: {results:?}"
        );
    }
    assert_eq!(
        x.d.video_key_count(),
        2,
        "one encoder for the bus, re-used (+0)"
    );
}

/// A live encoder whose `finish()` takes 3 s (test 6).
struct SlowFinish(FakeEnc);

impl VideoEncodeWorker for SlowFinish {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.0.encode(item)
    }
    fn reorder_bound(&self) -> ReorderBound {
        ReorderBound::NoReorder
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        std::thread::sleep(std::time::Duration::from_secs(3));
        Ok(Vec::new())
    }
    fn force_keyframe(&mut self) {
        self.0.force_keyframe();
    }
}

impl LiveEncoder for SlowFinish {
    fn start_at(&mut self, t0: u64) {
        self.0.start_at(t0);
    }
    fn time_base(&self) -> Rational {
        self.0.time_base()
    }
    fn move_gate(&self) -> MoveGate {
        self.0.move_gate()
    }
}

/// Test 6: closing an encoder whose finish takes 3 s never blocks the
/// consumer. Bound: every router call around and after the close < 50 ms, and
/// the close lands on its linger tick.
#[test]
fn a_slow_close_never_blocks_the_consumer() {
    let encoded = Arc::new(AtomicUsize::new(0));
    let enc = Arc::clone(&encoded);
    let open: OpenFn = Box::new(move |_| {
        let e: Box<dyn LiveEncoder> = Box::new(SlowFinish(FakeEnc {
            forced: false,
            encoded: Arc::clone(&enc),
        }));
        Ok(e)
    });
    let mut x = harness(2, 6, Some(open));
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    let mut t = 1;
    while !x.results().contains(&TakeResult::Applied) {
        x.tick(t);
        t += 1;
        assert!(t < 60, "applied");
    }
    x.h.try_take(take("b", 1, PictureBus::Program)).unwrap();
    while !x.results().contains(&TakeResult::Applied) {
        x.tick(t);
        t += 1;
        assert!(t < 120, "back on Program");
    }
    // Run to the linger close and well past it; measure from before the close.
    x.max_router = std::time::Duration::ZERO;
    let close_by = t + LINGER_TICKS + 2;
    while x.h.snapshot().dynamic_encoders_live > 0 {
        x.tick(t);
        t += 1;
        assert!(t <= close_by, "closed at its linger tick");
    }
    let closed = Instant::now();
    while closed.elapsed() < std::time::Duration::from_millis(3200) {
        x.tick(t);
        t += 1;
    }
    assert!(
        x.max_router < std::time::Duration::from_millis(50),
        "router tick during a 3 s finish: {:?}",
        x.max_router
    );
    assert_eq!(
        x.windows_of(0).len(),
        usize::try_from(t - 1).unwrap(),
        "Program cadence"
    );
    assert_eq!(
        x.h.snapshot().reaper_overflow,
        0,
        "handed to the reaper, not dropped"
    );
}

/// A live encoder that counts its live worker threads (the encoder is owned
/// by the worker thread and dropped when that thread ends) and whose
/// `finish()` takes `finish_ms`.
struct Counted {
    inner: FakeEnc,
    live: Arc<AtomicUsize>,
    finish_ms: u64,
}

impl Counted {
    fn boxed(live: &Arc<AtomicUsize>, finish_ms: u64) -> Box<dyn VideoEncodeWorker> {
        live.fetch_add(1, Ordering::SeqCst);
        Box::new(Self {
            inner: FakeEnc {
                forced: false,
                encoded: Arc::new(AtomicUsize::new(0)),
            },
            live: Arc::clone(live),
            finish_ms,
        })
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

impl VideoEncodeWorker for Counted {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.encode(item)
    }
    fn reorder_bound(&self) -> ReorderBound {
        ReorderBound::NoReorder
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        std::thread::sleep(std::time::Duration::from_millis(self.finish_ms));
        Ok(Vec::new())
    }
    fn force_keyframe(&mut self) {}
}

/// Register a live key in the dispatcher and the router, as `drain_opens`
/// does, around an arbitrary worker.
fn inject(
    r: &mut OutputRouter,
    d: &mut RenditionDispatcher,
    bus: PictureBus,
    worker: Box<dyn VideoEncodeWorker>,
) -> usize {
    let key = d
        .add_video_key(DtsTimeBase { num: 1, den: 5 }, Box::new(|_| true))
        .unwrap();
    let lost = Arc::new(AtomicBool::new(false));
    let generation = r.next_generation;
    r.next_generation += 1;
    let w = r
        .spawn_live_worker(
            key,
            generation,
            Rational::new(1, 5),
            worker,
            Arc::clone(&lost),
        )
        .unwrap();
    r.dynamic.push(DynamicKey {
        bus,
        settings: 0,
        key,
        worker: Some(w),
        force_at: force_cell(),
        principal: "p".to_owned(),
        linger_deadline: None,
        lingering_since: 0,
        last_fed: None,
        last_aux_tick: None,
        seen_event: false,
        lost,
        generation,
        silent: 0,
    });
    key
}

/// A (lens 2 #1, lens 3 #1): 12 closes while the reaper is inside a 3 s
/// finish. The reaper queue (8) overflows; the overflowed handles are dropped
/// unjoined. Bound: every worker thread ends within 3 s + 1.5 s of the
/// closes (they finish concurrently because each input is closed before the
/// handoff), never serially behind the reaper (12 x 3 s), and no close call
/// takes > 50 ms.
#[test]
fn closes_beyond_the_reaper_queue_never_leave_a_worker_thread_running() {
    let live = Arc::new(AtomicUsize::new(0));
    let (mut r, _h, _aux) = router(1, 6, Box::new(|_| Err("never".to_owned())));
    let mut d = dispatcher(1);
    let mut worst = std::time::Duration::ZERO;
    for i in 0..12 {
        let key = inject(
            &mut r,
            &mut d,
            PictureBus::Source(format!("s{i}")),
            Counted::boxed(&live, 3000),
        );
        let started = Instant::now();
        r.close(key, &mut d);
        worst = worst.max(started.elapsed());
    }
    assert!(r.reaper_overflow >= 3, "overflow: {}", r.reaper_overflow);
    assert!(
        worst < std::time::Duration::from_millis(50),
        "close {worst:?}"
    );
    let closed = Instant::now();
    while live.load(Ordering::SeqCst) > 0
        && closed.elapsed() < std::time::Duration::from_millis(4500)
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        live.load(Ordering::SeqCst),
        0,
        "every worker thread ended within {:?}",
        closed.elapsed()
    );
}

/// A: a live worker whose event queue is never drained never blocks. Bound:
/// 200 ticks encoded within 2 s with nobody reading (queue 64), every drop
/// counted, the key marked lost; the next consumer drain fails that key
/// alone (retired, output falls back) in < 50 ms; after close the thread ends.
#[test]
fn an_undrained_live_worker_drops_its_events_and_never_blocks() {
    let live = Arc::new(AtomicUsize::new(0));
    let (mut r, h, _aux) = router(1, 6, Box::new(|_| Err("never".to_owned())));
    let mut d = dispatcher(1);
    let key = inject(&mut r, &mut d, src(), Counted::boxed(&live, 0));
    let started = Instant::now();
    let mut offered = 0_u64;
    while offered < 200 && started.elapsed() < std::time::Duration::from_secs(2) {
        let w = r.dynamic[0].worker.as_ref().unwrap();
        if w.offer(BakedTick {
            tick: offered,
            canvas: canvas(),
        })
        .is_ok()
        {
            offered += 1;
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    assert_eq!(offered, 200, "the worker kept taking input");
    let deadline = Instant::now();
    while r.dyn_dropped.load(Ordering::Relaxed) < 200 - 64
        && deadline.elapsed() < std::time::Duration::from_secs(1)
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(r.dyn_dropped.load(Ordering::Relaxed) >= 200 - 64);
    assert!(r.dynamic[0].lost.load(Ordering::Acquire));
    let tick = Instant::now();
    r.on_tick(1, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
        .unwrap();
    assert!(tick.elapsed() < std::time::Duration::from_millis(50));
    assert!(d.is_key_failed(key), "the key fails alone");
    assert_eq!(d.binding_video_key(0), Some(0), "Program untouched");
    assert!(h.snapshot().dropped_live_events >= 200 - 64);
    let closed = Instant::now();
    while live.load(Ordering::SeqCst) > 0 && closed.elapsed() < std::time::Duration::from_secs(1) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(live.load(Ordering::SeqCst), 0, "worker thread ended");
}

/// B (lens 2 #2): 200 sequential open/close cycles all succeed (the live cap
/// is the only bound, retired slots are reused), the stage vectors never grow
/// past base + live cap, and a closed worker's late event never reaches the
/// key that reused its slot.
#[test]
fn two_hundred_open_close_cycles_reuse_retired_slots() {
    let live = Arc::new(AtomicUsize::new(0));
    let (mut r, _h, _aux) = router(1, 6, Box::new(|_| Err("never".to_owned())));
    let mut d = dispatcher(1);
    for i in 0..200 {
        let key = inject(&mut r, &mut d, src(), Counted::boxed(&live, 0));
        assert!(key >= 1, "cycle {i}");
        r.close(key, &mut d);
        r.drain_live_events(&mut d, &mut NoDelivery).unwrap();
    }
    assert!(
        d.video_key_count() <= 2,
        "slots reused: {}",
        d.video_key_count()
    );
    // A late event tagged with a closed worker's generation is dropped.
    // Let every closed worker end and its tail be drained (and counted).
    let waited = Instant::now();
    while live.load(Ordering::SeqCst) > 0 && waited.elapsed() < std::time::Duration::from_secs(2) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(live.load(Ordering::SeqCst), 0, "closed workers ended");
    r.drain_live_events(&mut d, &mut NoDelivery).unwrap();
    let key = inject(&mut r, &mut d, src(), Counted::boxed(&live, 0));
    let before = r.dyn_dropped.load(Ordering::Relaxed);
    r.dyn_tx
        .try_send((
            0,
            DispatchEvent::Video {
                key_index: key,
                tick: 0,
                timebase: Rational::new(1, 5),
                packets: vec![pkt(0, true)],
                watermark: None,
            },
        ))
        .unwrap();
    r.drain_live_events(&mut d, &mut NoDelivery).unwrap();
    assert_eq!(r.dyn_dropped.load(Ordering::Relaxed), before + 1);
    assert!(!d.is_key_failed(key), "the new key is untouched");
    assert!(!r.dynamic[0].seen_event);
}

/// P (final-head abuse HIGH): an event is accepted only when ONE live key has
/// both its generation and its key index. A closed worker's event tagged
/// (generation = reused slot, key index = reused slot) matches another live
/// key's generation but names the slot reused by a new key: it is dropped and
/// counted, the new key's stage is untouched and the key does not fail.
#[test]
fn an_event_whose_generation_and_key_disagree_is_dropped() {
    let live = Arc::new(AtomicUsize::new(0));
    let (mut r, _h, _aux) = router(1, 6, Box::new(|_| Err("never".to_owned())));
    let mut d = dispatcher(1);
    let first = inject(&mut r, &mut d, src(), Counted::boxed(&live, 0));
    let other = inject(&mut r, &mut d, src(), Counted::boxed(&live, 0));
    assert_eq!((first, other), (1, 2));
    r.close(first, &mut d);
    // Let the closed worker end, then drain its tail (counted as dropped)
    // so the counter below measures only the injected event.
    let waited = Instant::now();
    while live.load(Ordering::SeqCst) > 1 && waited.elapsed() < std::time::Duration::from_secs(2) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(live.load(Ordering::SeqCst), 1, "the closed worker ended");
    r.drain_live_events(&mut d, &mut NoDelivery).unwrap();
    let reused_key = inject(&mut r, &mut d, src(), Counted::boxed(&live, 0));
    assert_eq!(reused_key, first, "the closed key's slot is reused");
    let reused = u64::try_from(reused_key).unwrap();
    assert!(
        r.dynamic
            .iter()
            .any(|k| k.generation == reused && k.key == other),
        "generation {reused} names the other live key"
    );
    let before = r.dyn_dropped.load(Ordering::Relaxed);
    // A timebase the key does not have: applied, it would fail the key.
    r.dyn_tx
        .try_send((
            reused,
            DispatchEvent::Video {
                key_index: reused_key,
                tick: 0,
                timebase: Rational::new(1, 7),
                packets: vec![pkt(0, true)],
                watermark: None,
            },
        ))
        .unwrap();
    r.drain_live_events(&mut d, &mut NoDelivery).unwrap();
    assert_eq!(r.dyn_dropped.load(Ordering::Relaxed), before + 1);
    assert!(!d.is_key_failed(reused_key), "the new key does not fail");
    assert!(
        !d.is_key_failed(other),
        "the generation's own key is untouched"
    );
    assert_eq!(
        d.counters().dropped_unbound,
        0,
        "nothing reached first stage"
    );
    assert!(r.dynamic.iter().all(|k| !k.seen_event));
}

/// B: an open whose result arrives on its deadline tick (before the take's
/// own deadline check) is reaped, never registered: no slot is consumed.
#[test]
fn a_deadline_missed_open_never_takes_a_slot() {
    let (release, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let encoded = Arc::new(AtomicUsize::new(0));
    let enc = Arc::clone(&encoded);
    let open: OpenFn = Box::new(move |_| {
        let _ = gate.lock().unwrap().recv();
        let e: Box<dyn LiveEncoder> = Box::new(FakeEnc {
            forced: false,
            encoded: Arc::clone(&enc),
        });
        Ok(e)
    });
    let mut x = harness(2, 6, Some(open));
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    let mut t = 1;
    while x.r.opens.is_empty() {
        x.tick(t);
        t += 1;
        assert!(t < 10, "admitted");
    }
    let deadline = x.r.opens.values().next().unwrap().deadline;
    while t < deadline {
        x.tick(t);
        t += 1;
    }
    release.send(()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    x.tick(t);
    assert_eq!(
        x.results(),
        vec![TakeResult::Refused(RefuseReason::EncoderNotReady)]
    );
    assert_eq!(x.d.video_key_count(), 1, "no slot consumed");
    assert_eq!(x.h.snapshot().dynamic_encoders_live, 0);
    assert_eq!(x.d.binding_video_key(1), Some(0));
}

/// D (lens 1 #1): the force is armed for tick T, the offer for T is dropped
/// and T+1 arrives (gap 1). The packet for T (a fill tick) is the keyframe,
/// not T+1, and a move with `at_tick = T` lands at T.
#[test]
fn a_forced_idr_lands_on_a_gap_filled_tick() {
    let force = force_cell();
    let mut w = GapFill {
        inner: Box::new(FakeEnc {
            forced: false,
            encoded: Arc::new(AtomicUsize::new(0)),
        }),
        force_at: Arc::clone(&force),
        next: None,
        last: None,
    };
    let t = 10;
    let mut packets = Vec::new();
    for tick in t - 3..t {
        packets.extend(
            w.encode(BakedTick {
                tick,
                canvas: canvas(),
            })
            .unwrap(),
        );
    }
    force.store(t, Ordering::Release);
    packets.extend(
        w.encode(BakedTick {
            tick: t + 1,
            canvas: canvas(),
        })
        .unwrap(),
    );
    let keys: Vec<(i64, bool)> = packets
        .iter()
        .map(|p| (p.pts().unwrap(), p.is_keyframe()))
        .collect();
    let t_i = i64::try_from(t).unwrap();
    assert!(keys.contains(&(t_i, true)), "T is the keyframe: {keys:?}");
    assert!(keys.contains(&(t_i + 1, false)), "only once: {keys:?}");
    // The dispatcher applies a move at T on that packet.
    let mut d = dispatcher(1);
    let key = d
        .add_video_key(
            DtsTimeBase { num: 1, den: 5 },
            Box::new(EncodedPacket::is_keyframe),
        )
        .unwrap();
    d.rebind_video(0, key, t, 1).unwrap();
    let mut rec = Rec::default();
    let now = Instant::now();
    let mark = |tick: i64| crate::rendition_workers::DtsWatermark {
        complete_before_ns: i128::from(tick + 1) * TICK_NS,
    };
    for p in packets {
        let tick = p.pts().unwrap();
        d.dispatch(
            DispatchEvent::Video {
                key_index: key,
                tick: u64::try_from(tick).unwrap(),
                timebase: Rational::new(1, 5),
                packets: vec![p],
                watermark: Some(mark(tick)),
            },
            now,
            &mut rec,
        )
        .unwrap();
    }
    for tick in 0..=t_i + 1 {
        d.dispatch(
            DispatchEvent::Video {
                key_index: 0,
                tick: u64::try_from(tick).unwrap(),
                timebase: Rational::new(1, 5),
                packets: vec![pkt(u64::try_from(tick).unwrap(), false)],
                watermark: Some(mark(tick)),
            },
            now,
            &mut rec,
        )
        .unwrap();
    }
    assert!(
        d.take_move_outcomes().contains(&MoveOutcome::Applied {
            output_index: 0,
            key,
            tick: t,
            parameter_sets_changed: false,
        }),
        "the rebind at T lands"
    );
}

/// C (lens 2 #3): over-long principal / operation ids are refused at the
/// mailbox; a take onto a key another principal's output subscribes does not
/// re-attribute that key (the holder keeps it, the newcomer is charged 0).
#[test]
fn untrusted_ids_are_bounded_and_a_held_key_is_never_re_attributed() {
    let mut x = harness(3, 6, None);
    let long = "x".repeat(MAX_TAKE_ID_BYTES + 1);
    let mut t1 = take("ok", 1, src());
    t1.principal.clone_from(&long);
    assert_eq!(x.h.try_take(t1), Err(TakeBusy));
    assert_eq!(x.h.try_take(take(&long, 1, src())), Err(TakeBusy));
    let mut fits = take(&"o".repeat(MAX_TAKE_ID_BYTES), 1, src());
    fits.principal = "p".repeat(MAX_TAKE_ID_BYTES);
    assert!(x.h.try_take(fits).is_ok(), "128 bytes fit");
    let mut t = 0;
    while !x.results().contains(&TakeResult::Applied) {
        x.tick(t);
        t += 1;
        assert!(t < 60, "applied");
    }
    let holder = "p".repeat(MAX_TAKE_ID_BYTES);
    assert_eq!(x.r.dynamic[0].principal, holder);
    // Past the per-key forced-IDR floor (the fake encoder has no natural IDR).
    for _ in 0..FORCE_FLOOR_TICKS {
        x.tick(t);
        t += 1;
    }
    let mut other = take("q-take", 2, src());
    other.principal = "q".to_owned();
    x.h.try_take(other).unwrap();
    let mut applied = false;
    for _ in 0..40 {
        x.tick(t);
        t += 1;
        if x.results().contains(&TakeResult::Applied) {
            applied = true;
            break;
        }
    }
    assert!(applied, "the newcomer rides the key");
    assert_eq!(x.d.video_key_count(), 2, "+0 encoders");
    assert_eq!(
        x.r.dynamic[0].principal, holder,
        "attribution stays with the holder"
    );
    assert!(x.r.has_capacity("q"), "the newcomer is not charged");
}

/// A live encoder that, once `die` is set, panics on its next encode or
/// (with `block`) blocks in it forever (until the test drops `release`).
struct Dying {
    inner: FakeEnc,
    die: Arc<AtomicBool>,
    block: Option<Mutex<Receiver<()>>>,
}

impl VideoEncodeWorker for Dying {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        if self.die.load(Ordering::SeqCst) {
            if let Some(gate) = &self.block {
                let _ = gate.lock().unwrap().recv();
            } else {
                panic!("injected live encoder panic");
            }
        }
        self.inner.encode(item)
    }
    fn reorder_bound(&self) -> ReorderBound {
        ReorderBound::NoReorder
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        Ok(Vec::new())
    }
    fn force_keyframe(&mut self) {
        self.inner.force_keyframe();
    }
}

impl LiveEncoder for Dying {
    fn start_at(&mut self, t0: u64) {
        self.inner.start_at(t0);
    }
    fn time_base(&self) -> Rational {
        Rational::new(1, 5)
    }
    fn move_gate(&self) -> MoveGate {
        self.inner.move_gate()
    }
}

/// U: drive a take onto a live encoder, make it die after the move (panic
/// or block), and assert the output falls back to Program with
/// Fallback(EncoderFailed) within `bound` ticks of the death, Program windows
/// of the other output contiguous throughout.
fn a_dying_live_encoder_falls_back(block: bool, bound: u64) {
    let (release, gate) = mpsc::channel::<()>();
    let mut gate = Some(gate);
    let die = Arc::new(AtomicBool::new(false));
    let kill = Arc::clone(&die);
    let open: OpenFn = Box::new(move |_| {
        let e: Box<dyn LiveEncoder> = Box::new(Dying {
            inner: FakeEnc {
                forced: false,
                encoded: Arc::new(AtomicUsize::new(0)),
            },
            die: Arc::clone(&die),
            block: if block {
                gate.take().map(Mutex::new)
            } else {
                None
            },
        });
        Ok(e)
    });
    let mut x = harness(2, 6, Some(open));
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("a", 1, src())).unwrap();
    let mut t = 5;
    let mut applied = false;
    let mut attempts = 1;
    // Same bound as the other end-to-end takes; a take refused on a loaded
    // host (its encoder not ready in time) is retried, at most 3 times.
    while t < 5 + 3 * OPEN_DEADLINE_TICKS && !applied {
        x.tick(t);
        let results = x.results();
        applied = results.contains(&TakeResult::Applied);
        if !applied && attempts < 3 && results.iter().any(|r| matches!(r, TakeResult::Refused(_))) {
            attempts += 1;
            x.h.try_take(take("a", 1, src())).unwrap();
        }
        t += 1;
    }
    assert!(applied, "the take applied before the encoder died");
    assert_eq!(x.d.binding_video_key(1), Some(1));
    kill.store(true, Ordering::SeqCst);
    if !block {
        // Tick once so the worker gets a frame, then wait (wall clock, not
        // ticks: the host may be loaded) for its thread to have ended, so
        // the detection below is the ended-worker path, not the wedge rule.
        x.tick(t);
        t += 1;
        let waited = Instant::now();
        while !x
            .r
            .dynamic
            .iter()
            .all(|d| d.worker.as_ref().is_none_or(RenditionWorker::is_finished))
            && waited.elapsed() < std::time::Duration::from_secs(5)
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let mut fell = None;
    let start = t;
    while t < start + bound && fell.is_none() {
        x.tick(t);
        if x.results()
            .contains(&TakeResult::Fallback(FallbackReason::EncoderFailed))
        {
            fell = Some(t);
        }
        t += 1;
    }
    let fell = fell.expect("Fallback(EncoderFailed) within the bound");
    // The fallback lands at the next Program RAP (gop 5).
    for t in fell + 1..fell + 8 {
        x.tick(t);
    }
    assert_eq!(x.d.binding_video_key(1), Some(0), "back on Program");
    assert_eq!(x.h.snapshot().dead_encoders, 1);
    assert_eq!(x.h.snapshot().dynamic_encoders_live, 0);
    let o0: Vec<i64> = x
        .windows_of(0)
        .iter()
        .filter_map(|w| w.first().map(|p| p.0))
        .collect();
    assert!(
        o0.windows(2).all(|w| w[1] == w[0] + 1) && o0.len() + 2 >= usize::try_from(fell).unwrap(),
        "Program windows contiguous: {o0:?}"
    );
    drop(release);
}

/// U (HIGH): a live encoder that panics after the move is detected (worker ended), its key closed, the output back on Program.
#[test]
fn a_panicking_live_encoder_falls_back_to_program() {
    // Detected on the first tick after the worker ended (never the 90-tick
    // wedge rule).
    a_dying_live_encoder_falls_back(false, 3);
}

/// U (HIGH): a live encoder that blocks forever in encode (wedged) is closed
/// after `WEDGED_ENCODER_TICKS` offered ticks without an event.
#[test]
fn a_wedged_live_encoder_falls_back_to_program() {
    a_dying_live_encoder_falls_back(true, u64::from(WEDGED_ENCODER_TICKS) + 20);
}
