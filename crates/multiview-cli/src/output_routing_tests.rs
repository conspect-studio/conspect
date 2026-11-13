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
        halved: Arc::new(AtomicBool::new(false)),
        sizing: None,
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
        halved: Arc::new(AtomicBool::new(false)),
        sizing: None,
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
            formats: Vec::new(),
            max_aux_canvases: DEFAULT_MAX_AUX_CANVASES,
            boot_retry: BootRetryPolicy::default(),
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
        bus: Some(bus),
        audio: None,
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
/// Audio packets are recorded apart, per output (routed-audio tests).
#[derive(Default)]
struct Rec {
    windows: Vec<(usize, Vec<(i64, bool)>)>,
    audio: Vec<(usize, EncodedPacket)>,
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
            .filter(|p| p.kind() == multiview_ffmpeg::StreamKind::Video)
            .map(|p| (p.pts().unwrap_or(-1), p.is_keyframe() && p.len() == 77))
            .collect();
        for a in p
            .iter()
            .filter(|p| p.kind() == multiview_ffmpeg::StreamKind::Audio)
        {
            self.audio.push((o, a.clone_shared_ref().unwrap()));
        }
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
    /// The pre-bake canvas handed to `publish_clean` per tick (kept alive so
    /// pointer identity is meaningful).
    clean: Vec<(u64, Arc<Nv12Image>)>,
    /// The clock's aux send gate and sent-tick counter.
    aux_active: Arc<AtomicBool>,
    aux_sent: Arc<AtomicU64>,
}

fn harness(outputs: usize, max: usize, open: Option<OpenFn>) -> Harness {
    harness_with(outputs, max, open, false)
}

fn harness_with(outputs: usize, max: usize, open: Option<OpenFn>, has_preview: bool) -> Harness {
    let store = Arc::new(TileStore::<Nv12Image>::with_defaults("cam"));
    let store2 = Arc::new(TileStore::<Nv12Image>::with_defaults("cam2"));
    let lookup = Arc::clone(&store);
    let lookup2 = Arc::clone(&store2);
    let base_force = Arc::new(AtomicU64::new(NO_FORCE));
    let failed = Arc::new(AtomicBool::new(false));
    let encoded = Arc::new(AtomicUsize::new(0));
    let enc2 = Arc::clone(&encoded);
    let aux_active = Arc::new(AtomicBool::new(false));
    let aux_sent = Arc::new(AtomicU64::new(0));
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
            has_preview,
            formats: Vec::new(),
            max_aux_canvases: DEFAULT_MAX_AUX_CANVASES,
            boot_retry: BootRetryPolicy::default(),
        },
        AuxWiring {
            feeds: Arc::new(SwapCell::from_pointee(Vec::new())),
            active: Arc::clone(&aux_active),
            failed: Arc::clone(&failed),
            dropped: Arc::new(AtomicU64::new(0)),
            sent: Arc::clone(&aux_sent),
            stores: Arc::new(move |id: &str| match id {
                "cam" => Some(Arc::clone(&lookup)),
                "cam2" => Some(Arc::clone(&lookup2)),
                _ => None,
            }),
            preview_selection: None,
            halved: Arc::new(AtomicBool::new(false)),
            sizing: None,
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
        clean: Vec::new(),
        aux_active,
        aux_sent,
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
                // #342: feeds at an output format (none without formats).
                for (feed_bus, format) in self.r.feed_keys() {
                    let Some(format) = format.filter(|_| feed_bus == bus) else {
                        continue;
                    };
                    if let Some(slot) = self.r.slot_at(&bus, Some(format)) {
                        let frame = Nv12Image::solid(
                            format.width,
                            format.height,
                            16,
                            128,
                            128,
                            CanvasColor::default().output_tag(),
                        )
                        .unwrap();
                        slot.latest
                            .store(Some(Arc::new(crate::routing_aux::AuxFrame {
                                tick: t,
                                generation: 1,
                                frame: Arc::new(frame),
                            })));
                    }
                }
            }
        }
        // The bake consumer publishes the pre-bake canvas before the router.
        let clean = canvas();
        self.r.publish_clean(t, &clean);
        self.clean.push((t, clean));
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
    /// Simulate a consumer gap on Clean exactly as the pipeline computes it:
    /// `substitute_clean(skip_from..t)` is called for the missing range
    /// (never published/ticked at all — the hot queue shed them), BEFORE
    /// this tick's own `publish_clean`/`on_tick` for the surviving tick `t`,
    /// matching the pipeline's call order.
    fn tick_after_clean_gap(&mut self, skip_from: u64, t: u64) {
        self.r.substitute_clean(skip_from..t);
        self.tick(t);
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
            .map(|o| {
                // The control plane persists from the outcome alone (D1).
                assert_eq!(o.bus, src(), "applied carries the taken bus");
                o.tick
            });
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
        format: None,
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
        starved: 0,
        refused_this_tick: false,
        reported_degraded: false,
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

/// PR #352 review (MEDIUM, media): an audio take on a `routable: false`
/// output is refused like a video take — `OutputKindUnroutable` at plan,
/// `Unroutable` at take — and nothing reaches the engine mailbox.
#[test]
fn the_control_adapter_refuses_audio_takes_on_an_unroutable_output() {
    use multiview_control::output_routing::{
        OutputRouting, OutputTakeRequest, PlanRefusal, RoutedAudio, TakeSubmitError,
    };
    let (mut r, h, _aux) = router(2, 6, Box::new(|_| Err("never".to_owned())));
    let h = h.with_outputs(vec![
        RunOutput {
            id: Some("ok".into()),
            settings: 0,
            feed_format: None,
            routable: true,
            program_audio: true,
        },
        RunOutput {
            id: Some("vaapi".into()),
            settings: 0,
            feed_format: None,
            routable: false,
            program_audio: true,
        },
    ]);
    let slot = output_routing_slot();
    slot.store(Some(Arc::new(h)));
    let adapter = crate::output_routing_control::OutputRoutingAdapter::new(Arc::clone(&slot));
    let outcomes = adapter.subscribe_outcomes().unwrap();
    let src = RoutedAudio::Source("bars".into());
    assert_eq!(
        adapter.plan_audio("vaapi", None, Some(&src)),
        Err(PlanRefusal::OutputKindUnroutable)
    );
    assert!(adapter.plan_audio("ok", None, Some(&src)).is_ok());
    let audio_take = |output: &str| OutputTakeRequest {
        operation_id: format!("op-{output}"),
        output: output.into(),
        video: None,
        audio: Some(src.clone()),
        layout: None,
        principal: "p".into(),
    };
    assert_eq!(
        adapter.try_take(audio_take("vaapi")),
        Err(TakeSubmitError::Unroutable)
    );
    adapter.try_take(audio_take("ok")).unwrap();
    let mut d = dispatcher(2);
    for t in 0..4 {
        r.on_tick(t, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
            .unwrap();
    }
    let seen: Vec<_> = std::iter::from_fn(|| {
        outcomes
            .recv_timeout(std::time::Duration::from_millis(200))
            .ok()
    })
    .map(|o| o.operation_id)
    .collect();
    assert!(
        seen.iter().all(|op| op != "op-vaapi"),
        "the refused take never reached the engine: {seen:?}"
    );
}

/// The control adapter maps config ids → router indices from the run's own
/// output map (binding ids), never from config order: with an internal anchor
/// at index 0 and outputs renamed/reordered, a take addressed to "renamed"
/// lands on index 1 and its outcome comes back named "renamed".
#[test]
fn the_control_adapter_maps_renamed_ids_to_their_own_index() {
    use multiview_control::output_routing::{OutputRouting, OutputTakeRequest, RoutedFeed};
    let (mut r, h, _aux) = router(3, 6, Box::new(|_| Err("never".to_owned())));
    let h = h.with_outputs(vec![
        RunOutput {
            id: None,
            settings: 0,
            feed_format: None,
            routable: true,
            program_audio: true,
        },
        RunOutput {
            id: Some("renamed".into()),
            settings: 0,
            feed_format: None,
            routable: true,
            program_audio: true,
        },
        RunOutput {
            id: Some("zz-first".into()),
            settings: 0,
            feed_format: None,
            routable: false,
            program_audio: true,
        },
    ]);
    let slot = output_routing_slot();
    slot.store(Some(Arc::new(h)));
    let adapter = crate::output_routing_control::OutputRoutingAdapter::new(Arc::clone(&slot));
    let outcomes = adapter.subscribe_outcomes().unwrap();
    assert!(
        adapter.plan("zz-first", &RoutedFeed::Program).is_err(),
        "an unroutable kind is refused at plan"
    );
    assert!(adapter.plan("output-0", &RoutedFeed::Program).is_err());
    let cost = adapter
        .plan("renamed", &RoutedFeed::Source("x".into()))
        .unwrap();
    assert!(cost.needs_encoder && cost.fits(), "{cost:?}");
    assert!(
        !adapter
            .plan("renamed", &RoutedFeed::Program)
            .unwrap()
            .needs_encoder
    );
    adapter
        .try_take(OutputTakeRequest {
            operation_id: "op-1".into(),
            output: "renamed".into(),
            video: Some(RoutedFeed::Program),
            audio: None,
            layout: None,
            principal: "p".into(),
        })
        .unwrap();
    let mut d = dispatcher(3);
    r.on_tick(0, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
        .unwrap();
    let got = outcomes
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(got.output, "renamed");
    assert_eq!(got.operation_id, "op-1");
    assert_eq!(got.outcome, "applied");
    let live = adapter.snapshot().unwrap();
    assert!(live.outputs.contains_key("renamed") && live.outputs.contains_key("zz-first"));
    assert_eq!(
        live.outputs.len(),
        2,
        "the internal anchor is never exposed"
    );
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

/// A live encoder recording the canvas pointer of every tick it encodes.
struct PtrEnc {
    forced: bool,
    seen: Arc<Mutex<Vec<(u64, usize)>>>,
}

impl VideoEncodeWorker for PtrEnc {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.seen
            .lock()
            .unwrap()
            .push((item.tick, Arc::as_ptr(&item.canvas).addr()));
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

impl LiveEncoder for PtrEnc {
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

/// The (tick, canvas address) pairs a [`PtrEnc`] encoded.
type SeenPtrs = Arc<Mutex<Vec<(u64, usize)>>>;

fn ptr_harness(outputs: usize) -> (Harness, SeenPtrs) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rec = Arc::clone(&seen);
    let open: OpenFn = Box::new(move |_| {
        let enc: Box<dyn LiveEncoder> = Box::new(PtrEnc {
            forced: false,
            seen: Arc::clone(&rec),
        });
        Ok(enc)
    });
    (harness_with(outputs, 6, Some(open), true), seen)
}

fn apply(x: &mut Harness, from: u64, op: &str) -> (u64, u64) {
    for t in from..from + OPEN_DEADLINE_TICKS {
        x.tick(t);
        if let Some(o) = x.outcomes.try_iter().next() {
            assert_eq!(o.operation_id, op, "unexpected outcome {o:?}");
            assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
            return (o.tick, t + 1);
        }
    }
    panic!("{op} never applied");
}

/// ADR-0108 slice 2 (§2.5): a take to Clean opens one live encoder fed
/// exactly the pre-bake canvases handed to `publish_clean` (same `Arc`, the
/// frame of the same tick), applies at a RAP, and never wakes the aux thread.
#[test]
fn a_take_to_clean_feeds_the_pre_bake_canvas_and_never_wakes_aux() {
    let (mut x, seen) = ptr_harness(2);
    for t in 0..5 {
        x.tick(t);
    }
    assert!(!x.r.clean_active && x.r.slot(&PictureBus::Clean).is_none());
    assert!(
        seen.lock().unwrap().is_empty(),
        "no Clean take: publish_clean stores nothing and nothing opens"
    );
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    let (at, now) = apply(&mut x, 5, "c");
    assert_eq!(x.d.video_key_count(), 2, "+1 encoder for Clean");
    assert_eq!(x.d.binding_video_key(1), Some(1));
    assert_eq!(
        x.d.binding_video_key(0),
        Some(0),
        "output 0 stays on Program"
    );
    let o1 = x.windows_of(1);
    let first = o1
        .iter()
        .find(|w| w.first().is_some_and(|p| p.0 == i64::try_from(at).unwrap()))
        .unwrap();
    assert!(
        first[0].1,
        "the first packet on Clean is a self-contained RAP"
    );
    for t in now..now + 20 {
        x.tick(t);
    }
    // Let the worker drain what it was offered.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.len() >= 20,
        "the Clean encoder was fed: {}",
        seen.len()
    );
    for (tick, ptr) in &seen {
        let published = x
            .clean
            .iter()
            .find(|(t, _)| t == tick)
            .map(|(_, c)| Arc::as_ptr(c).addr());
        assert_eq!(
            published,
            Some(*ptr),
            "tick {tick}: the encoder got exactly the canvas published for that tick"
        );
    }
    assert!(
        !x.aux_active.load(Ordering::Acquire),
        "only Clean routed: the aux send gate stays closed"
    );
    assert_eq!(x.aux_sent.load(Ordering::Relaxed), 0, "no aux tick sent");
    assert_eq!(
        x.h.snapshot().outputs[1].bus,
        "clean",
        "readback names the Clean bus"
    );
}

/// The aux thread failing never moves an output off Clean (Clean is fed by
/// the bake consumer, not by the aux thread).
#[test]
fn an_aux_failure_never_moves_an_output_off_clean() {
    let (mut x, _seen) = ptr_harness(2);
    x.tick(0);
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    let (_, mut now) = apply(&mut x, 1, "c");
    x.failed.store(true, Ordering::Release);
    for _ in 0..30 {
        x.tick(now);
        now += 1;
    }
    assert_eq!(x.results(), Vec::new(), "no Fallback outcome");
    assert_eq!(x.d.binding_video_key(1), Some(1), "still on the Clean key");
    assert_eq!(x.h.snapshot().outputs[1].bus, "clean");
}

/// Clean without an M/E is unroutable (like Preview).
#[test]
fn clean_without_a_switcher_is_refused() {
    let mut x = harness(2, 6, None);
    x.tick(0);
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    x.tick(1);
    assert_eq!(
        x.results(),
        vec![TakeResult::Refused(RefuseReason::Unroutable)]
    );
}

/// Slice 1 limit fixed: the plan counts a lingering live encoder for the
/// same (bus, settings) as +0 (the take reuses it), and +1 once it closed.
#[test]
fn the_plan_counts_a_lingering_encoder_as_plus_zero() {
    use multiview_control::output_routing::{OutputRouting, RoutedFeed};
    let mut x = harness(2, 6, None);
    let h = x.h.clone().with_outputs(vec![
        RunOutput {
            id: Some("a".into()),
            settings: 0,
            feed_format: None,
            routable: true,
            program_audio: true,
        },
        RunOutput {
            id: Some("b".into()),
            settings: 0,
            feed_format: None,
            routable: true,
            program_audio: true,
        },
    ]);
    let slot = output_routing_slot();
    slot.store(Some(Arc::new(h)));
    let adapter = crate::output_routing_control::OutputRoutingAdapter::new(Arc::clone(&slot));
    let cam = RoutedFeed::Source("cam".into());
    x.tick(0);
    assert!(
        adapter.plan("b", &cam).unwrap().needs_encoder,
        "+1 at first"
    );
    x.h.try_take(take("to-src", 1, src())).unwrap();
    let (_, now) = apply(&mut x, 1, "to-src");
    x.h.try_take(take("back", 1, PictureBus::Program)).unwrap();
    let (_, now) = apply(&mut x, now, "back");
    assert_eq!(x.h.snapshot().dynamic_encoders_live, 1, "lingering");
    assert_eq!(
        x.h.snapshot().live_keys,
        vec![("src:cam".to_owned(), 0)],
        "the lingering key is published"
    );
    assert!(
        !adapter.plan("b", &cam).unwrap().needs_encoder,
        "a lingering encoder is reused: +0"
    );
    assert!(
        !adapter.plan("a", &cam).unwrap().needs_encoder,
        "same settings on the other output: +0 too"
    );
    for t in now..=now + LINGER_TICKS + 1 {
        x.tick(t);
    }
    assert_eq!(x.h.snapshot().dynamic_encoders_live, 0, "closed");
    assert_eq!(x.h.snapshot().live_keys, Vec::<(String, usize)>::new());
    assert!(
        adapter.plan("b", &cam).unwrap().needs_encoder,
        "after the linger closed: +1"
    );
}

// ---------------------------------------------------------------------------
// ADR-0108 slice 2: explicit per-output audio (router + real AAC encoders).
// ---------------------------------------------------------------------------

use multiview_audio::cadence::SampleClock;
use multiview_audio::crossfade::{grid_ceil, TakeCrossfade, AAC_FRAME_SAMPLES};
use multiview_audio::format::{AudioBlock, AudioFormat};
use std::sync::atomic::AtomicU32;

const NTSC: Rational = Rational::new(30_000, 1_001);

/// The program mix: a stationary 440 Hz tone at 0.3.
fn prog(n: u64) -> f32 {
    let t = f32::from(u16::try_from(n % 48_000).unwrap()) / 48_000.0;
    0.3 * (std::f32::consts::TAU * 440.0 * t).sin()
}

/// The `bars` source: a stationary 1 kHz tone at 0.5.
fn bars(n: u64) -> f32 {
    let t = f32::from(u16::try_from(n % 48_000).unwrap()) / 48_000.0;
    0.5 * (std::f32::consts::TAU * 1_000.0 * t).sin()
}

fn audio_only(op: &str, output: usize, choice: AudioChoice) -> OutputTake {
    OutputTake {
        operation_id: op.to_owned(),
        output_index: output,
        bus: None,
        audio: Some(choice),
        principal: "p".to_owned(),
    }
}

fn bars_src() -> AudioChoice {
    AudioChoice::Source("bars".to_owned())
}

/// A router with program audio over a real dispatcher: a real program AAC
/// encoder feeds the shared stage, real private buses feed theirs.
struct AudioHarness {
    r: OutputRouter,
    h: OutputRoutingHandle,
    d: RenditionDispatcher,
    rec: Rec,
    outcomes: Receiver<TakeOutcome>,
    program: multiview_output::AudioRenditionEncoder,
    tb: Rational,
    clock: SampleClock,
    cadence: Rational,
    gain: Arc<AtomicU32>,
    bars_live: bool,
    feed_max: std::time::Duration,
    /// The longest whole consumer step (router tick, routed-audio feed and
    /// program-audio dispatch; the simulated encode sleep excluded).
    step_max: std::time::Duration,
    store: Arc<TileStore<Nv12Image>>,
    base_force: Arc<AtomicU64>,
    tap: crate::routing_audio::AudioTapCell,
    others: Vec<TakeOutcome>,
}

fn audio_harness(outputs: usize, cadence: Rational, open: OpenFn) -> AudioHarness {
    let config = multiview_output::AudioEncodeConfig::aac(48_000, 2, 128_000);
    let program = multiview_output::AudioRenditionEncoder::new(&config).unwrap();
    let tb = program.time_base();
    let store = Arc::new(TileStore::<Nv12Image>::with_defaults("cam"));
    let lookup = Arc::clone(&store);
    let base_force = Arc::new(AtomicU64::new(NO_FORCE));
    let (mut r, h) = OutputRouter::new(
        RouterConfig {
            cadence,
            gop: 5,
            base_keys: vec![0; outputs],
            base_force: vec![Arc::clone(&base_force)],
            max_encoders: 6,
            max_per_principal: DEFAULT_MAX_PER_PRINCIPAL,
            has_preview: false,
            formats: Vec::new(),
            max_aux_canvases: DEFAULT_MAX_AUX_CANVASES,
            boot_retry: BootRetryPolicy::default(),
        },
        AuxWiring {
            feeds: Arc::new(SwapCell::from_pointee(Vec::new())),
            active: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            sent: Arc::new(AtomicU64::new(0)),
            stores: Arc::new(move |id: &str| (id == "cam").then(|| Arc::clone(&lookup))),
            preview_selection: None,
            halved: Arc::new(AtomicBool::new(false)),
            sizing: None,
        },
        open,
    )
    .unwrap();
    let gain = Arc::new(AtomicU32::new(1.0_f32.to_bits()));
    let g = Arc::clone(&gain);
    r.enable_audio(
        config,
        program.codec_params().codec_config_bytes(),
        Arc::new(move |id: &str| {
            if id == "bars" {
                f32::from_bits(g.load(Ordering::Acquire))
            } else {
                1.0
            }
        }),
    );
    let d = RenditionDispatcher::new(DispatchConfig {
        video_count: 1,
        audio_count: 1,
        video_time_base: vec![DtsTimeBase {
            num: cadence.den,
            den: cadence.num,
        }],
        audio_time_base: vec![DtsTimeBase {
            num: tb.num,
            den: tb.den,
        }],
        bindings: (0..outputs)
            .map(|i| BindingConfig {
                output_index: i,
                video_key: Some(0),
                audio_tracks: vec![0],
                inband_parameter_changes_ok: true,
            })
            .collect(),
        first_tick: 0,
        cadence,
        video_move_gates: vec![Some(Box::new(|p: &EncodedPacket| p.is_keyframe()))],
    })
    .unwrap();
    let outcomes = h.outcomes().unwrap();
    let tap = Arc::clone(&h.audio_tap);
    AudioHarness {
        r,
        h,
        d,
        rec: Rec::default(),
        outcomes,
        program,
        tb,
        clock: SampleClock::new(48_000, cadence),
        cadence,
        gain,
        bars_live: true,
        feed_max: std::time::Duration::ZERO,
        step_max: std::time::Duration::ZERO,
        store,
        base_force,
        tap,
        others: Vec::new(),
    }
}

impl AudioHarness {
    fn boundary_ns(&self, tick: u64) -> i128 {
        i128::from(tick) * i128::from(self.cadence.den) * 1_000_000_000
            / i128::from(self.cadence.num)
    }

    fn media(&self, tick: u64) -> MediaTime {
        MediaTime::from_nanos(i64::try_from(self.boundary_ns(tick)).unwrap())
    }

    fn block(&self, tick: u64, f: fn(u64) -> f32) -> AudioBlock {
        let (a, b) = (self.clock.total_at(tick), self.clock.total_at(tick + 1));
        AudioBlock::from_interleaved(
            AudioFormat::new(48_000, multiview_audio::ChannelLayout::Stereo),
            (a..b).flat_map(|n| [f(n), f(n)]).collect(),
        )
        .unwrap()
    }

    /// One Program tick in the pipeline's order.
    fn tick(&mut self, t: u64) {
        let step = Instant::now();
        self.r
            .drain_live_events(&mut self.d, &mut self.rec)
            .unwrap();
        if let Some(prev) = t.checked_sub(1) {
            let forced = take_force(&self.base_force, prev);
            self.d
                .dispatch(
                    DispatchEvent::Video {
                        key_index: 0,
                        tick: prev,
                        timebase: Rational::new(self.cadence.den, self.cadence.num),
                        packets: vec![pkt(prev, forced || prev % 5 == 0)],
                        watermark: Some(crate::rendition_workers::DtsWatermark {
                            complete_before_ns: self.boundary_ns(prev + 1),
                        }),
                    },
                    Instant::now(),
                    &mut self.rec,
                )
                .unwrap();
        }
        let frame = Nv12Image::solid(16, 16, 16, 128, 128, CanvasColor::default().output_tag());
        self.store.publish(frame.unwrap(), self.media(t));
        if let Some(slot) = self.r.slot(&src()) {
            slot.latest
                .store(Some(Arc::new(crate::routing_aux::AuxFrame {
                    tick: t,
                    generation: 1,
                    frame: canvas(),
                })));
        }
        self.r
            .on_tick(t, self.media(t), &mut self.d, &mut self.rec)
            .unwrap();
        let program = self.block(t, prog);
        let tone = self.block(t, bars);
        let inputs: Vec<(&str, &AudioBlock)> = if self.bars_live {
            vec![("bars", &tone)]
        } else {
            Vec::new()
        };
        let started = Instant::now();
        self.r.feed_audio(t, &program, &inputs, &mut self.d);
        self.feed_max = self.feed_max.max(started.elapsed());
        self.r.tap_program_audio(t, &program);
        let packets = self
            .program
            .encode_interleaved(program.interleaved(), program.frame_count())
            .unwrap();
        let wm = packets.last().and_then(|p| {
            crate::rendition_workers::next_frame_lower_bound_ns(p.pts()?, 1024, 48_000)
        });
        self.d
            .dispatch(
                DispatchEvent::Audio {
                    tick: t,
                    timebases: vec![self.tb],
                    packets,
                    watermarks: vec![wm],
                },
                Instant::now(),
                &mut self.rec,
            )
            .unwrap();
        self.step_max = self.step_max.max(step.elapsed());
        // The private encode threads run within the tick period.
        std::thread::sleep(std::time::Duration::from_millis(3));
    }

    /// Tick from `from` until the outcome of `op` arrives (other outcomes
    /// are kept in `others`); returns it and the next tick.
    fn until(&mut self, from: u64, op: &str, max: u64) -> (TakeOutcome, u64) {
        for t in from..from + max {
            self.tick(t);
            for o in self.outcomes.try_iter().collect::<Vec<_>>() {
                if o.operation_id == op {
                    return (o, t + 1);
                }
                self.others.push(o);
            }
        }
        panic!("no outcome for {op} within {max} ticks");
    }

    fn run(&mut self, from: u64, n: u64) -> u64 {
        for t in from..from + n {
            self.tick(t);
        }
        from + n
    }

    fn audio_pts(&self, o: usize) -> Vec<i64> {
        self.rec
            .audio
            .iter()
            .filter(|(i, _)| *i == o)
            .map(|(_, p)| p.pts().unwrap())
            .collect()
    }

    fn audio_packets(&self, o: usize) -> Vec<&EncodedPacket> {
        self.rec
            .audio
            .iter()
            .filter(|(i, _)| *i == o)
            .map(|(_, p)| p)
            .collect()
    }

    /// Output `o`'s private-bus PCM (left channel) by absolute sample, from
    /// the tap; asserts the fed blocks are contiguous.
    fn private_left(&self, o: usize) -> std::collections::BTreeMap<u64, f32> {
        let tap = self.tap.lock().unwrap();
        let mut out = std::collections::BTreeMap::new();
        let mut next: Option<u64> = None;
        for (output, start, pcm) in &tap.private {
            if *output != o {
                continue;
            }
            if let Some(n) = next {
                // A new bus may start later (after a close), never overlap.
                assert!(*start >= n, "private PCM overlaps at {start} (< {n})");
            }
            for (i, v) in pcm.chunks(2).enumerate() {
                out.insert(start + u64::try_from(i).unwrap(), v[0]);
            }
            next = Some(start + u64::try_from(pcm.len() / 2).unwrap());
        }
        out
    }
}

/// Every audio packet one AAC frame after the previous one (1024 samples =
/// 1920 units of the 90 kHz MPEG-TS clock), none missing or duplicated.
fn assert_pts_step(pts: &[i64], what: &str) {
    assert!(pts.len() > 4, "{what}: too few audio packets");
    for pair in pts.windows(2) {
        assert_eq!(pair[1] - pair[0], 1024, "{what}: audio PTS step");
        assert_eq!(
            (pair[1] - pair[0]) * 90_000 / 48_000,
            1920,
            "{what}: 90 kHz step"
        );
    }
}

/// A take to `bars` on output 1, then back to Program; returns the drain
/// ticks of both takes and the next tick.
fn tone_and_back(x: &mut AudioHarness) -> (u64, u64, u64) {
    let t = x.run(0, 12);
    x.h.try_take(audio_only("to-bars", 1, bars_src())).unwrap();
    let (o, t2) = x.until(t, "to-bars", 60);
    assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
    assert_eq!(o.audio, Some(bars_src()));
    let t3 = x.run(t2, 15);
    x.h.try_take(audio_only("to-program", 1, AudioChoice::Program))
        .unwrap();
    let (o, t4) = x.until(t3, "to-program", 60);
    assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
    // Long enough for the return to the shared stream and its close.
    let end = x.run(t4, 20);
    (t, t3, end)
}

/// Integration of #329 boot takes with slice-2 boot audio: a persisted row
/// with a video feed AND an explicit audio feed boots as ONE combined boot
/// take (so the audio-only boot take never supersedes the video one). When
/// that take's video is refused for good, the boot take is given up but its
/// audio is still applied, alone, as `boot:audio:<output index>` — keyed on
/// the OUTPUT INDEX, never by re-parsing the give-up take's `operation_id`
/// (PR #352 round 2): the id below deliberately does NOT name the output
/// index (an HLS config id is a path and can be arbitrarily long and
/// unrelated to the index), yet the re-issued audio id is still
/// `boot:audio:1`.
#[test]
fn a_given_up_combined_boot_take_still_applies_its_audio() {
    let mut x = audio_harness(2, NTSC, Box::new(|_| Err("no video".to_owned())));
    let t = x.run(0, 6);
    // Preview on a run without a switcher: refused `unroutable` for good. The
    // operation id is a long, unrelated string (as a truncated HLS path
    // would be, under the take-id length cap) to prove `boot_audio_alone`
    // never parses it.
    let long_config_id_take = format!("{BOOT_TAKE_PREFIX}{}", "z".repeat(100));
    x.h.try_take(OutputTake {
        operation_id: long_config_id_take.clone(),
        output_index: 1,
        bus: Some(PictureBus::Preview),
        audio: Some(bars_src()),
        principal: BOOT_PRINCIPAL.to_owned(),
    })
    .unwrap();
    let (o, t) = x.until(t, &format!("{BOOT_AUDIO_TAKE_PREFIX}1"), 60);
    assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
    // The router-minted audio id is keyed on the output index (1), not on
    // any substring of the give-up take's (unrelated) operation id.
    assert_eq!(o.operation_id, "boot:audio:1");
    assert!(multiview_control::output_routing::is_boot_take(
        &o.operation_id
    ));
    assert_eq!(o.audio, Some(bars_src()));
    let boot: Vec<_> = x
        .others
        .iter()
        .filter(|o| o.operation_id == long_config_id_take)
        .collect();
    assert_eq!(boot.len(), 1, "{:?}", x.others);
    assert_eq!(
        boot[0].outcome,
        TakeResult::Refused(RefuseReason::Unroutable)
    );
    x.run(t, 4);
    let s = x.h.snapshot();
    assert_eq!(s.boot_takes_given_up, 1);
    assert_eq!(s.boot_takes_pending, 0);
    assert_eq!(s.outputs[1].bus, "program", "the video stays on Program");
    assert_eq!(s.outputs[1].audio_choice, "src:bars");
}

/// Spec G2: the PCM fed to the private bus across a tone -> program fade is
/// continuous, the fade lasts exactly one tick of samples from G, no sample
/// exceeds either side's peak, and the encoded stream keeps one frame per
/// 1024 samples.
#[test]
fn an_audio_only_crossfade_is_sample_continuous_and_one_tick_long() {
    let mut x = audio_harness(2, NTSC, Box::new(|_| Err("no video".to_owned())));
    let (_, back_tick, _) = tone_and_back(&mut x);
    let fades: Vec<(u64, u64)> = x
        .tap
        .lock()
        .unwrap()
        .fades
        .iter()
        .filter(|f| f.0 == 1)
        .map(|f| (f.1, f.2))
        .collect();
    assert_eq!(fades.len(), 2, "one fade per take: {fades:?}");
    let (g, len) = fades[1];
    assert_eq!(
        g,
        grid_ceil(x.clock.total_at(back_tick), AAC_FRAME_SAMPLES),
        "G = first grid boundary at or after total_at(T)"
    );
    assert_eq!(
        len,
        x.clock.total_at(back_tick + 1) - x.clock.total_at(back_tick),
        "fade length = total_at(T+1) - total_at(T)"
    );
    let pcm = x.private_left(1);
    let step = |a: u64, b: u64| -> f32 {
        (a..b)
            .map(|n| (pcm[&(n + 1)] - pcm[&n]).abs())
            .fold(0.0, f32::max)
    };
    // Steady signals on either side of the fade.
    let s_old = step(g - 3000, g - 1);
    let s_new = step(g + len, g + len + 3000);
    assert!(s_old > 0.06 && s_new > 0.01, "steady steps {s_old} {s_new}");
    for n in g - 3000..g {
        assert!((pcm[&n] - bars(n)).abs() < 1e-6, "tone before G at {n}");
    }
    for n in g + len..g + len + 3000 {
        assert!(
            (pcm[&n] - prog(n)).abs() < 1e-6,
            "program after the fade at {n}"
        );
    }
    // Across the fade: an equal-power pair o·cosθ + n·sinθ moves by at most
    // hypot(o', n') plus the envelope's own slope (π/2 per fade length).
    let fade_step = step(g - 1, g + len + 1);
    let len_f = f32::from(u16::try_from(len).unwrap());
    let bound = s_old.hypot(s_new) + (0.5 + 0.3) * std::f32::consts::FRAC_PI_2 / len_f;
    assert!(
        fade_step <= bound,
        "largest step across the fade {fade_step} > bound {bound} (steady {s_old}, {s_new})"
    );
    let xf = TakeCrossfade { start: g, len };
    for n in g..g + len {
        let peak = bars(n).abs().max(prog(n).abs());
        assert!(pcm[&n].abs() <= peak + 1e-6, "sample {n} above both peaks");
        let (go, gn) = xf.gains(n);
        let want = (bars(n) * go + prog(n) * gn).clamp(-peak, peak);
        assert!((pcm[&n] - want).abs() < 1e-5, "envelope at {n}");
    }
    // The encoded streams: one frame per 1024 samples, the same frames.
    assert_pts_step(&x.audio_pts(1), "output 1");
    assert_pts_step(&x.audio_pts(0), "output 0");
    let (p0, p1) = (x.audio_pts(0), x.audio_pts(1));
    let n = p0.len().min(p1.len());
    assert_eq!(p0[..n], p1[..n], "the same frames on both outputs");
}

fn decode(packets: &[&EncodedPacket]) -> Vec<(i64, Vec<f32>)> {
    let reference = multiview_output::AudioRenditionEncoder::new(
        &multiview_output::AudioEncodeConfig::aac(48_000, 2, 128_000),
    )
    .unwrap();
    let mut decoder = multiview_ffmpeg::StreamAudioDecoder::from_codec_parameters(
        reference.codec_params(),
        reference.time_base(),
    )
    .unwrap();
    packets
        .iter()
        .map(|p| {
            let pcm = decoder.decode_interleaved(p).unwrap();
            (p.pts().unwrap(), pcm.chunks(2).map(|c| c[0]).collect())
        })
        .collect()
}

/// Spec C + G3: back on the program mix, the private bus returns the output
/// to the shared program stream at G' (>= 4 frames after the fade) and
/// closes. Decoded output 1 across G' equals decoded output 0 (shared) within
/// twice the steady-state difference of the two encoders before G', and
/// within one 16-bit LSB once both decoders receive the same packets.
#[test]
fn a_private_bus_back_on_program_returns_to_the_shared_stream() {
    let mut x = audio_harness(2, NTSC, Box::new(|_| Err("no video".to_owned())));
    let (_, back_tick, _) = tone_and_back(&mut x);
    let snap = x.h.snapshot();
    assert_eq!(snap.audio_buses_live, 0, "{snap:?}");
    assert_eq!(snap.audio.returned, 1, "{snap:?}");
    assert_eq!(snap.audio.failed, 0, "{snap:?}");
    assert_eq!(snap.outputs[1].audio, "program");
    assert_eq!(snap.outputs[1].audio_choice, "program");
    assert_eq!(x.d.binding_audio_key(1), None, "back on the shared stage");
    let tap = x.tap.lock().unwrap();
    let fade = *tap.fades.iter().rev().find(|f| f.0 == 1).unwrap();
    let fade_end = fade.1 + fade.2;
    let (_, g_ret) = *tap.returns.iter().find(|r| r.0 == 1).expect("a return");
    drop(tap);
    assert!(
        g_ret >= fade_end + 4 * 1024,
        "G' {g_ret} >= fade end {fade_end} + 4 frames"
    );
    assert_eq!(g_ret % 1024, 0, "G' on the grid");
    // Within ~85 ms + one fade of the take's tick.
    assert!(
        g_ret - x.clock.total_at(back_tick) <= 4 * 1024 + 2 * 1602 + 2 * 1024,
        "returned late: {g_ret}"
    );
    let d0 = decode(&x.audio_packets(0));
    let d1 = decode(&x.audio_packets(1));
    let g_ret = i64::try_from(g_ret).unwrap();
    let fade_end = i64::try_from(fade_end).unwrap();
    let diff = |from: i64, to: i64| -> (f32, usize) {
        let mut max = 0.0_f32;
        let mut n = 0;
        for ((p0, s0), (p1, s1)) in d0.iter().zip(&d1) {
            assert_eq!(p0, p1);
            if *p0 >= from && *p0 < to {
                for (a, b) in s0.iter().zip(s1) {
                    max = max.max((a - b).abs());
                    n += 1;
                }
            }
        }
        (max, n)
    };
    // Two encoders coding the same PCM, before G': their steady difference.
    let (base, nb) = diff(fade_end + 2048, g_ret);
    assert!(nb >= 1024, "baseline span {nb}");
    let (across, na) = diff(g_ret, g_ret + 2048);
    assert!(na >= 2048, "span after G' {na}");
    assert!(
        across <= 2.0 * base + 1e-4,
        "decoded difference across G' {across} > 2 x steady {base}"
    );
    let (after, nl) = diff(g_ret + 2048, i64::MAX);
    assert!(nl > 0);
    // The same packets from here on: the two decoders' remaining difference
    // is their own float state (measured ~1e-5), far below one 16-bit LSB.
    assert!(after < 3e-5, "the same packets after G': {after}");
    assert_pts_step(&x.audio_pts(1), "output 1 across G'");
}

/// Spec G4 (chaos): a flood of 5,000 audio-only takes alternating Source /
/// Program / None on outputs 1 and 2 as fast as the mailbox accepts. Output
/// 0 (Program) keeps every audio frame and every video window; the mailbox
/// refuses (`TakeBusy`) instead of growing; private buses never exceed the
/// outputs; every output's audio stays one frame per 1024 samples.
#[test]
fn an_audio_take_flood_never_disturbs_program_and_stays_bounded() {
    let mut x = audio_harness(3, NTSC, Box::new(|_| Err("no video".to_owned())));
    let mut t = x.run(0, 8);
    let choices = [bars_src(), AudioChoice::Program, AudioChoice::Silence];
    let (mut sent, mut busy) = (0_usize, 0_usize);
    while sent < 5_000 {
        loop {
            let take = audio_only(
                &format!("f{sent}"),
                1 + sent % 2,
                choices[(sent / 2) % 3].clone(),
            );
            if x.h.try_take(take).is_err() {
                busy += 1;
                break;
            }
            sent += 1;
            if sent == 5_000 {
                break;
            }
        }
        x.tick(t);
        t += 1;
        let live = x.h.snapshot().audio_buses_live;
        assert!(live <= 2, "private buses {live} > routed outputs");
    }
    let end = x.run(t, 40);
    assert!(busy > 0, "the bounded mailbox refused takes");
    let video: Vec<i64> = x.windows_of_video(0);
    let want: Vec<i64> = (0..i64::try_from(video.len()).unwrap()).collect();
    assert_eq!(video, want, "output 0: one video window per tick");
    assert!(
        u64::try_from(video.len()).unwrap() + 8 >= end,
        "windows {} for {end} ticks",
        video.len()
    );
    let p0 = x.audio_pts(0);
    assert_pts_step(&p0, "Program (output 0)");
    let samples = x.clock.total_at(end - 6);
    assert!(
        i64::try_from(p0.len()).unwrap() * 1024 >= i64::try_from(samples).unwrap(),
        "output 0 audio frames {} for {samples} samples",
        p0.len()
    );
    for o in 1..3 {
        let p = x.audio_pts(o);
        assert_pts_step(&p, &format!("output {o}"));
        let n = p.len().min(p0.len());
        assert_eq!(p[..n], p0[..n], "output {o}: the same frames as Program");
    }
    assert!(
        x.feed_max < std::time::Duration::from_millis(50),
        "feed {:?}",
        x.feed_max
    );
}

impl AudioHarness {
    fn windows_of_video(&self, o: usize) -> Vec<i64> {
        self.rec
            .windows
            .iter()
            .filter(|w| w.0 == o)
            .flat_map(|w| w.1.iter().map(|p| p.0))
            .collect()
    }
}

/// Spec G5 (chaos): the routed source dies mid-take (before G) and, on a
/// second take, mid-crossfade. Its audio is silence from then on (no stale
/// repeat): the bus carries exactly the old audio times the falling
/// envelope, then nothing. The takes apply (a source without audio is
/// silence, §5.1); every stream stays continuous.
#[test]
fn a_routed_source_dying_mid_take_and_mid_fade_carries_silence() {
    let mut x = audio_harness(2, NTSC, Box::new(|_| Err("no video".to_owned())));
    let t = x.run(0, 12);
    // Mid-take: the strip goes away the tick the take is drained, before G.
    x.h.try_take(audio_only("die-a", 1, bars_src())).unwrap();
    x.bars_live = false;
    let (o, t) = x.until(t, "die-a", 60);
    assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
    let t = x.run(t, 6);
    let fade_a = *x.tap.lock().unwrap().fades.last().unwrap();
    let pcm = x.private_left(1);
    let xf = TakeCrossfade {
        start: fade_a.1,
        len: fade_a.2,
    };
    for n in fade_a.1..fade_a.1 + fade_a.2 + 2000 {
        let want = prog(n) * xf.gains(n).0;
        assert!(
            (pcm[&n] - want).abs() < 1e-6,
            "mid-take {n}: {} vs {want}",
            pcm[&n]
        );
    }
    // Back to Program, then a second take with the source alive that dies
    // halfway through its fade.
    x.h.try_take(audio_only("back", 1, AudioChoice::Program))
        .unwrap();
    let (_, t) = x.until(t, "back", 60);
    let t = x.run(t, 4);
    x.bars_live = true;
    x.h.try_take(audio_only("die-b", 1, bars_src())).unwrap();
    let mut t = t;
    let mut died_at = None;
    let mut applied = None;
    for _ in 0..60 {
        x.tick(t);
        t += 1;
        let fade = x.tap.lock().unwrap().fades.last().copied();
        if let Some((_, g, len)) = fade.filter(|f| f.1 > fade_a.1) {
            if died_at.is_none() && x.clock.total_at(t) > g {
                // The tick that fed the fade's start: the source dies now.
                x.bars_live = false;
                died_at = Some((x.clock.total_at(t), g, len));
            }
        }
        for o in x.outcomes.try_iter() {
            if o.operation_id == "die-b" {
                applied = Some(o.outcome);
            }
        }
        if applied.is_some() && died_at.is_some() {
            break;
        }
    }
    assert_eq!(applied, Some(TakeResult::Applied));
    let (dead_from, g, len) = died_at.expect("the fade started");
    assert!(dead_from > g && dead_from < g + len, "died mid-fade");
    let t = x.run(t, 4);
    let pcm = x.private_left(1);
    let xf = TakeCrossfade { start: g, len };
    for n in dead_from..g + len + 1000 {
        if !pcm.contains_key(&n) {
            break; // the bus returned and closed
        }
        let want = prog(n) * xf.gains(n).0;
        assert!(
            (pcm[&n] - want).abs() < 1e-6,
            "mid-fade {n}: {} vs {want} (stale tone?)",
            pcm[&n]
        );
    }
    let _ = x.run(t, 2);
    assert_pts_step(&x.audio_pts(0), "Program");
    assert_pts_step(&x.audio_pts(1), "output 1");
    assert_eq!(x.h.snapshot().audio.failed, 0);
}

fn stalled_or_crashing_worker(panic_worker: bool) {
    let mut x = audio_harness(2, NTSC, Box::new(|_| Err("no video".to_owned())));
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let wait = Arc::new(Mutex::new(wait));
    let wedged = Arc::new(AtomicBool::new(false));
    let wedge = Arc::clone(&wedged);
    x.r.set_audio_worker_hook(Arc::new(move |_job| {
        if !wedge.load(Ordering::Acquire) {
            return;
        }
        assert!(!panic_worker, "injected private-bus worker panic");
        // A wedged worker: never drains its job queue until released.
        let _ = wait.lock().unwrap().recv();
    }));
    // The take applies first: the output is on its private bus (tone).
    let t = x.run(0, 12);
    x.h.try_take(audio_only("tone", 1, bars_src())).unwrap();
    let (o, t) = x.until(t, "tone", 60);
    assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
    let t = x.run(t, 2);
    assert_eq!(x.h.snapshot().audio_buses_live, 1);
    assert!(x.d.binding_audio_key(1).is_some(), "on the private bus");
    // Now the worker wedges (or panics) at its next job.
    wedged.store(true, Ordering::Release);
    let mut failed_at = None;
    let depth = u64::try_from(crate::routing_audio::PRIVATE_AUDIO_QUEUE).unwrap();
    for k in 0..depth + 4 {
        x.tick(t + k);
        if x.h.snapshot().audio_buses_live == 0 {
            failed_at = Some(k);
            break;
        }
    }
    let k = failed_at.expect("the bus failed");
    assert!(k <= depth + 1, "failed within the queue depth: {k}");
    if !panic_worker {
        // Mechanism (measured): the wedged worker emits nothing, so its
        // dispatcher stage never proves a window; the output's pending FIFO
        // (4 windows) fills and the dispatcher fails that stage alone
        // (`AudioFallback`) at about 4 ticks — before the 8-slot PCM queue
        // would fill (`TrySendError::Full`, the backstop). Either way the
        // bus fails alone and the consumer never waits (timed below).
        assert!(k >= 2, "the bus is not failed before the wedge bites: {k}");
    }
    let t = x.run(t + k + 1, 4);
    let snap = x.h.snapshot();
    assert_eq!(snap.audio.failed, 1, "{snap:?}");
    assert_eq!(x.d.binding_audio_key(1), None, "back on the shared stage");
    assert!(
        x.feed_max < std::time::Duration::from_millis(50),
        "the consumer never waits: {:?}",
        x.feed_max
    );
    assert!(
        x.step_max < std::time::Duration::from_millis(100),
        "the whole consumer step never waits on the wedged worker: {:?}",
        x.step_max
    );
    let _ = x.run(t, 4);
    let o0: Vec<i64> = x
        .rec
        .windows
        .iter()
        .filter(|w| w.0 == 0)
        .filter_map(|w| w.1.first().map(|p| p.0))
        .collect();
    assert!(
        o0.windows(2).all(|w| w[1] == w[0] + 1),
        "Program video keeps one window per tick through the wedge: {o0:?}"
    );
    assert_pts_step(&x.audio_pts(0), "Program");
    assert_pts_step(&x.audio_pts(1), "output 1");
    drop(release);
}

/// Spec G6 (chaos): a wedged private-bus worker fails its bus alone within
/// the queue depth; the output stays on / returns to the shared program
/// stream; the consumer never waits; Program is untouched.
#[test]
fn a_stalled_private_bus_worker_fails_alone_and_never_blocks() {
    stalled_or_crashing_worker(false);
}

/// Spec G6: the same for a panicking private-bus worker.
#[test]
fn a_panicking_private_bus_worker_fails_alone() {
    stalled_or_crashing_worker(true);
}

/// Spec G7: a combined take (video to a source + audio Program) moves the
/// video and keeps the program audio (no private bus); a combined take whose
/// video is refused (feed never fresh) restores the choice and leaves the
/// audio alone.
#[test]
fn a_combined_take_moves_video_with_its_explicit_audio_or_restores_it() {
    let encoded = Arc::new(AtomicUsize::new(0));
    let open: OpenFn = Box::new(move |_| {
        let enc: Box<dyn LiveEncoder> = Box::new(FakeEnc {
            forced: false,
            encoded: Arc::clone(&encoded),
        });
        Ok(enc)
    });
    let mut x = audio_harness(2, CADENCE, open);
    let t = x.run(0, 5);
    x.h.try_take(OutputTake {
        operation_id: "both".to_owned(),
        output_index: 1,
        bus: Some(src()),
        audio: Some(AudioChoice::Program),
        principal: "p".to_owned(),
    })
    .unwrap();
    let (o, t) = x.until(t, "both", OPEN_DEADLINE_TICKS);
    assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
    assert_eq!(o.bus, src());
    assert_eq!(o.audio, Some(AudioChoice::Program));
    let t = x.run(t, 3);
    let snap = x.h.snapshot();
    assert_eq!(snap.outputs[1].bus, "src:cam");
    assert_eq!(snap.outputs[1].audio, "program");
    assert_eq!(snap.outputs[1].audio_choice, "program");
    assert_eq!(snap.audio.opened, 0, "no private bus: {snap:?}");
    // A video take that is never fresh, with audio Source(bars).
    x.h.try_take(OutputTake {
        operation_id: "stale".to_owned(),
        output_index: 1,
        bus: Some(PictureBus::Source("nowhere".to_owned())),
        audio: Some(bars_src()),
        principal: "p".to_owned(),
    })
    .unwrap();
    let (o, _) = x.until(t, "stale", TAKE_PRIME_DEADLINE_TICKS + 2);
    assert_eq!(o.outcome, TakeResult::Refused(RefuseReason::FeedNotFresh));
    assert_eq!(o.audio, Some(AudioChoice::Program), "choice restored");
    let snap = x.h.snapshot();
    assert_eq!(snap.outputs[1].audio_choice, "program");
    assert_eq!(snap.outputs[1].audio, "program");
    assert_eq!(snap.audio_buses_live, 0);
    assert_eq!(snap.audio.opened, 0, "{snap:?}");
}

/// Spec G7 (second half, dispatcher-refused): a combined take whose video
/// move is refused after the audio was installed fades back and restores
/// the previous choice.
#[test]
fn a_superseded_combined_take_restores_the_audio_choice() {
    let encoded = Arc::new(AtomicUsize::new(0));
    let open: OpenFn = Box::new(move |_| {
        let enc: Box<dyn LiveEncoder> = Box::new(FakeEnc {
            forced: false,
            encoded: Arc::clone(&encoded),
        });
        Ok(enc)
    });
    let mut x = audio_harness(2, CADENCE, open);
    let mut t = x.run(0, 5);
    x.h.try_take(OutputTake {
        operation_id: "combined".to_owned(),
        output_index: 1,
        bus: Some(src()),
        audio: Some(bars_src()),
        principal: "p".to_owned(),
    })
    .unwrap();
    // Wait until the video move is issued (Moving), then supersede it with
    // a video-only take back to Program before it lands.
    let mut moving = false;
    for _ in 0..OPEN_DEADLINE_TICKS {
        x.tick(t);
        t += 1;
        if matches!(
            x.r.outputs[1].pending.as_ref().map(|p| p.phase),
            Some(Phase::Moving(_))
        ) {
            moving = true;
            break;
        }
    }
    assert!(moving, "the video move was issued");
    assert_eq!(x.h.snapshot().outputs[1].audio_choice, "src:bars");
    x.h.try_take(OutputTake {
        operation_id: "cancel".to_owned(),
        output_index: 1,
        bus: Some(PictureBus::Program),
        audio: None,
        principal: "p".to_owned(),
    })
    .unwrap();
    let (o, t) = x.until(t, "cancel", 20);
    assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
    let superseded = x
        .others
        .iter()
        .find(|o| o.operation_id == "combined")
        .expect("the combined take ended");
    assert_eq!(superseded.outcome, TakeResult::Superseded);
    let _ = x.run(t, 12);
    let snap = x.h.snapshot();
    assert_eq!(snap.outputs[1].audio_choice, "follow", "choice restored");
    assert_eq!(snap.outputs[1].audio, "program");
    assert_eq!(snap.outputs[1].bus, "program");
}

/// Spec G8: an explicit Source carries its `audio-routing` gain and mute,
/// read live (a change between ticks applies to the next tick's block); a
/// Track choice carries its input's block without the route gain.
#[test]
fn an_explicit_source_follows_the_live_route_gain_and_a_track_does_not() {
    let mut x = audio_harness(2, NTSC, Box::new(|_| Err("no video".to_owned())));
    x.gain.store(0.5_f32.to_bits(), Ordering::Release);
    let t = x.run(0, 12);
    x.h.try_take(audio_only("gain", 1, bars_src())).unwrap();
    let (o, t) = x.until(t, "gain", 60);
    assert_eq!(o.outcome, TakeResult::Applied);
    let t = x.run(t, 3);
    let check = |x: &AudioHarness, tick: u64, f: &dyn Fn(u64) -> f32, what: &str| {
        let pcm = x.private_left(1);
        for n in x.clock.total_at(tick)..x.clock.total_at(tick + 1) {
            assert!((pcm[&n] - f(n)).abs() < 1e-6, "{what} at {n}");
        }
    };
    check(&x, t - 1, &|n| bars(n) * 0.5, "gain 0.5");
    x.gain.store(0.25_f32.to_bits(), Ordering::Release);
    x.tick(t);
    check(&x, t, &|n| bars(n) * 0.25, "gain 0.25 on the next tick");
    x.gain.store(0.0_f32.to_bits(), Ordering::Release);
    x.tick(t + 1);
    check(&x, t + 1, &|_| 0.0, "muted");
    x.h.try_take(audio_only(
        "track",
        1,
        AudioChoice::Track {
            name: "eng".to_owned(),
            input: "bars".to_owned(),
        },
    ))
    .unwrap();
    let (o, t) = x.until(t + 2, "track", 60);
    assert_eq!(o.outcome, TakeResult::Applied);
    let t = x.run(t, 3);
    check(&x, t - 1, &bars, "a track is not gained");
    let snap = x.h.snapshot();
    assert_eq!(snap.outputs[1].audio, "track:eng");
    assert_eq!(snap.outputs[1].audio_choice, "track:eng");
}

/// An audio take on a run without program audio, or with neither video nor
/// audio, is refused `Unroutable`; an audio take that changes nothing
/// applies at once.
#[test]
fn audio_takes_without_a_program_audio_slot_are_unroutable() {
    let (mut rt, handle, _) = router(2, 6, Box::new(|_| Err("never".to_owned())));
    let outcomes = handle.outcomes().unwrap();
    let mut disp = dispatcher(2);
    handle
        .try_take(audio_only("a", 1, AudioChoice::Program))
        .unwrap();
    handle
        .try_take(OutputTake {
            operation_id: "none".to_owned(),
            output_index: 1,
            bus: None,
            audio: None,
            principal: "p".to_owned(),
        })
        .unwrap();
    rt.on_tick(0, MediaTime::from_nanos(0), &mut disp, &mut NoDelivery)
        .unwrap();
    let got: Vec<_> = outcomes.try_iter().map(|o| o.outcome).collect();
    assert_eq!(
        got,
        vec![
            TakeResult::Refused(RefuseReason::Unroutable),
            TakeResult::Refused(RefuseReason::Unroutable)
        ]
    );
    let mut x = audio_harness(2, NTSC, Box::new(|_| Err("no video".to_owned())));
    let t = x.run(0, 3);
    x.h.try_take(audio_only("same", 1, AudioChoice::Program))
        .unwrap();
    let (o, next) = x.until(t, "same", 2);
    assert_eq!((o.outcome, o.tick), (TakeResult::Applied, next - 1));
    assert_eq!(x.h.snapshot().audio.opened, 0);
}

/// Spec B (flood bound): a change taken while a fade runs never overlaps it
/// and never steps. It starts from the running fade's target at the first
/// grid boundary after that fade ends; the PCM stays continuous.
#[test]
fn a_take_during_a_running_fade_is_queued_after_it() {
    let mut x = audio_harness(2, NTSC, Box::new(|_| Err("no video".to_owned())));
    let mut t = x.run(0, 12);
    x.h.try_take(audio_only("first", 1, bars_src())).unwrap();
    // Tick until the first fade has started being fed.
    let mut first = None;
    for _ in 0..60 {
        x.tick(t);
        t += 1;
        if let Some(f) = x.tap.lock().unwrap().fades.iter().find(|f| f.0 == 1) {
            first = Some((f.1, f.2));
        }
        if first.is_some() {
            break;
        }
    }
    let (g1, len1) = first.expect("the first fade started");
    assert!(
        x.clock.total_at(t) < g1 + len1,
        "the first fade is still running when the next take drains"
    );
    x.h.try_take(audio_only("second", 1, AudioChoice::Silence))
        .unwrap();
    let (o, t) = x.until(t, "second", 60);
    assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
    let first_outcome = x.others.iter().find(|o| o.operation_id == "first");
    assert_eq!(
        first_outcome.map(|o| o.outcome),
        Some(TakeResult::Superseded),
        "latest wins: the first take reports superseded"
    );
    let _ = x.run(t, 4);
    let fades: Vec<(u64, u64)> = x
        .tap
        .lock()
        .unwrap()
        .fades
        .iter()
        .filter(|f| f.0 == 1)
        .map(|f| (f.1, f.2))
        .collect();
    assert_eq!(fades.len(), 2, "{fades:?}");
    let (g2, _) = fades[1];
    assert_eq!(fades[0], (g1, len1), "the running fade ran to its end");
    // The tick block in which the running fade ends is already mixed when
    // it ends: the next fade starts at the first grid boundary at or after
    // that block's end (never inside the running fade, never a step).
    let mut k = 0;
    while x.clock.total_at(k + 1) < g1 + len1 {
        k += 1;
    }
    assert_eq!(
        g2,
        grid_ceil(x.clock.total_at(k + 1), AAC_FRAME_SAMPLES),
        "the next fade starts at the first grid boundary after the running one ends"
    );
    assert!(g2 >= g1 + len1, "no overlap");
    let pcm = x.private_left(1);
    // Between the fades the bus carries the first fade's target exactly.
    for n in g1 + len1..g2 {
        assert!(
            (pcm[&n] - bars(n)).abs() < 1e-6,
            "tone between fades at {n}"
        );
    }
    // No step anywhere above the fade-bounded maximum.
    let worst = (g1 - 1..g2 + 2000)
        .map(|n| (pcm[&(n + 1)] - pcm[&n]).abs())
        .fold(0.0_f32, f32::max);
    let steady = 0.5 * std::f32::consts::TAU * 1_000.0 / 48_000.0;
    assert!(
        worst <= steady * 1.5,
        "largest step {worst} (steady tone {steady})"
    );
}

// ---------------------------------------------------------------------------
// #330 (PR 319 review): router faults.
// ---------------------------------------------------------------------------

/// #330 item 1: a take superseded while its open is wedged releases the
/// open's encoder session at the open's deadline (it leaked forever).
#[test]
fn a_superseded_wedged_open_releases_its_session_at_the_deadline() {
    let (_hold, wait) = mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    let open: OpenFn = Box::new(move |_| {
        let _ = wait.lock().unwrap().recv(); // wedged while the test runs
        Err("unreachable".to_owned())
    });
    let mut x = harness(2, 6, Some(open));
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    x.tick(1);
    x.tick(2);
    assert_eq!(x.h.snapshot().opens_in_flight, 1, "the open is in flight");
    // Superseded by a take back to Program (no waiter remains).
    x.h.try_take(take("b", 1, PictureBus::Program)).unwrap();
    for t in 3..=OPEN_DEADLINE_TICKS + 3 {
        x.tick(t);
    }
    let got: Vec<_> = x
        .outcomes
        .try_iter()
        .map(|o| (o.operation_id, o.outcome))
        .collect();
    assert!(
        got.contains(&("a".to_owned(), TakeResult::Superseded)),
        "{got:?}"
    );
    assert_eq!(
        x.h.snapshot().opens_in_flight,
        0,
        "the superseded open's session is released at its deadline"
    );
}

/// #330 item 2: two outputs' takes to the same bus with the same settings,
/// while the first open is still in flight, open ONE encoder (+0 for the
/// second), never two.
#[test]
fn two_in_flight_takes_to_one_bus_open_one_encoder() {
    let opens = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&opens);
    let encoded = Arc::new(AtomicUsize::new(0));
    let open: OpenFn = Box::new(move |_| {
        count.fetch_add(1, Ordering::Relaxed);
        // Slow enough that the second take is admitted meanwhile.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let enc: Box<dyn LiveEncoder> = Box::new(FakeEnc {
            forced: false,
            encoded: Arc::clone(&encoded),
        });
        Ok(enc)
    });
    let mut x = harness(3, 6, Some(open));
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    x.h.try_take(take("b", 2, src())).unwrap();
    let mut got = Vec::new();
    for t in 1..60 {
        x.tick(t);
        got.extend(x.outcomes.try_iter().map(|o| (o.operation_id, o.outcome)));
        if got.len() == 2 {
            break;
        }
    }
    assert!(
        got.contains(&("a".to_owned(), TakeResult::Applied)),
        "{got:?}"
    );
    assert!(
        got.contains(&("b".to_owned(), TakeResult::Applied)),
        "{got:?}"
    );
    assert_eq!(
        opens.load(Ordering::Relaxed),
        1,
        "one encoder for one (bus, settings)"
    );
    assert_eq!(x.h.snapshot().dynamic_encoders_live, 1);
}

/// A fake live encoder whose keyframes can be withheld (its windows then
/// never pass the move gate, so a move onto it stays pending).
struct GatedEnc {
    forced: bool,
    no_keys: Arc<AtomicBool>,
}

impl VideoEncodeWorker for GatedEnc {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        let key = std::mem::take(&mut self.forced) && !self.no_keys.load(Ordering::Acquire);
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

impl LiveEncoder for GatedEnc {
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

/// Output 1 on a live `cam` key (principal `p`), output 2 on a live `cam2`
/// key (principal `q`), then `cam2`'s keyframes withheld and output 1's take
/// "c" to `cam2` left pending in `Moving(kB)`. Returns (harness, kA, kB,
/// next tick, keyframe switch).
fn stuck_move_onto_a_shared_key() -> (Harness, usize, usize, u64, Arc<AtomicBool>) {
    let no_keys = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&no_keys);
    let open: OpenFn = Box::new(move |_| {
        let enc: Box<dyn LiveEncoder> = Box::new(GatedEnc {
            forced: false,
            no_keys: Arc::clone(&flag),
        });
        Ok(enc)
    });
    let mut x = harness(3, 6, Some(open));
    x.tick(0);
    let until_applied = |x: &mut Harness, from: u64, op: &str| -> u64 {
        for t in from..from + OPEN_DEADLINE_TICKS {
            x.tick(t);
            let got: Vec<_> = x.outcomes.try_iter().collect();
            if got
                .iter()
                .any(|o| o.operation_id == op && o.outcome == TakeResult::Applied)
            {
                return t + 1;
            }
            assert!(got.is_empty(), "unexpected outcomes {got:?}");
        }
        panic!("{op} never applied");
    };
    x.h.try_take(take("a", 1, src())).unwrap();
    let t = until_applied(&mut x, 1, "a");
    let mut b = take("b", 2, PictureBus::Source("cam2".to_owned()));
    b.principal = "q".to_owned();
    x.h.try_take(b).unwrap();
    let t = until_applied(&mut x, t, "b");
    let ka = x.d.binding_video_key(1).unwrap();
    let kb = x.d.binding_video_key(2).unwrap();
    assert!(ka != 0 && kb != 0 && ka != kb, "two live keys: {ka} {kb}");
    no_keys.store(true, Ordering::Release);
    x.h.try_take(take("c", 1, PictureBus::Source("cam2".to_owned())))
        .unwrap();
    x.tick(t);
    assert!(x.results().is_empty(), "c is pending");
    assert!(
        x.h.snapshot().outputs[1].pending.is_some(),
        "c moves onto the warm key at once"
    );
    assert_eq!(x.d.pending_video_move(1), Some(kb), "the move is pending");
    (x, ka, kb, t + 1, no_keys)
}

/// #330 item 3: an aux failure that rebinds an output to Program while its
/// take's dispatcher move is pending settles that take (it stranded in
/// `Moving` forever, holding its destination key subscribed), and the
/// destination key then lingers and closes like any 0-subscriber key.
#[test]
fn an_aux_fallback_settles_a_take_whose_move_is_pending() {
    let (mut x, _ka, kb, mut t, _no_keys) = stuck_move_onto_a_shared_key();
    x.failed.store(true, Ordering::Release);
    let fell_at = t;
    let mut got = Vec::new();
    for _ in 0..20 {
        x.tick(t);
        t += 1;
        got.extend(
            x.outcomes
                .try_iter()
                .filter(|o| o.output_index == 1)
                .map(|o| (o.operation_id, o.outcome, o.tick)),
        );
    }
    // Settled at the fallback itself, before it is reported (not later by
    // the dispatcher's Superseded echo).
    assert_eq!(
        got,
        vec![
            ("c".to_owned(), TakeResult::Superseded, fell_at),
            (
                String::new(),
                TakeResult::Fallback(FallbackReason::AuxFailed),
                fell_at
            ),
        ],
        "the pending take gets a terminal outcome at the fallback"
    );
    assert!(
        x.h.snapshot().outputs.iter().all(|o| o.pending.is_none()),
        "no take stays pending"
    );
    assert_eq!(x.d.binding_video_key(1), Some(0), "output 1 on Program");
    assert_eq!(x.d.binding_video_key(2), Some(0), "output 2 on Program");
    // kB has no subscriber since the fallback: it lingers, then closes.
    while t < fell_at + LINGER_TICKS - 2 {
        x.tick(t);
        t += 1;
    }
    assert!(!x.d.is_key_failed(kb), "kB lingers");
    for _ in 0..5 {
        x.tick(t);
        t += 1;
    }
    assert!(x.d.is_key_failed(kb), "kB closed after the linger");
}

/// #330 item 3, the dispatcher-Fallback arm: the output's live key fails
/// while its take's move onto another key is pending. The take is settled
/// at the fallback (Superseded) and its move cancelled: it never lands
/// later behind the reported Fallback.
#[test]
fn an_encoder_fallback_settles_a_take_whose_move_is_pending() {
    let (mut x, ka, _kb, mut t, no_keys) = stuck_move_onto_a_shared_key();
    x.r.close(ka, &mut x.d);
    x.tick(t);
    t += 1;
    let got: Vec<_> = x
        .outcomes
        .try_iter()
        .map(|o| (o.operation_id, o.outcome))
        .collect();
    assert!(
        got.contains(&("c".to_owned(), TakeResult::Superseded)),
        "{got:?}"
    );
    assert!(
        got.contains(&(
            String::new(),
            TakeResult::Fallback(FallbackReason::EncoderFailed)
        )),
        "{got:?}"
    );
    assert!(x.h.snapshot().outputs[1].pending.is_none());
    assert_eq!(x.d.pending_video_move(1), None, "the move is cancelled");
    // kB's keyframes return: nothing lands on output 1.
    no_keys.store(false, Ordering::Release);
    for _ in 0..15 {
        x.tick(t);
        t += 1;
    }
    assert_eq!(
        x.d.binding_video_key(1),
        Some(0),
        "output 1 stays on Program"
    );
    let o = &x.h.snapshot().outputs[1];
    assert_eq!((o.bus.as_str(), o.key.as_str()), ("program", "program/0"));
}

/// #330 item 4: a take superseded while the output's current key is
/// already retired (its Fallback not yet drained) cannot cancel the old
/// move by re-asserting that key. The old move is cancelled directly
/// (counted) and never lands later with no take, and the output's `key` and
/// `bus` always agree.
#[test]
fn a_failed_cancel_never_lets_an_old_move_land() {
    let (mut x, ka, kb, mut t, no_keys) = stuck_move_onto_a_shared_key();
    // kA retires between drains (the router has not seen it yet); a take
    // that stays priming supersedes "c".
    x.d.retire_video_key(ka).unwrap();
    x.h.try_take(take("d", 1, PictureBus::Source("nope".to_owned())))
        .unwrap();
    x.tick(t);
    t += 1;
    let got: Vec<_> = x
        .outcomes
        .try_iter()
        .map(|o| (o.operation_id, o.outcome))
        .collect();
    assert!(
        got.contains(&("c".to_owned(), TakeResult::Superseded)),
        "{got:?}"
    );
    assert_eq!(x.h.snapshot().move_cancel_failures, 1, "counted");
    assert_eq!(x.d.pending_video_move(1), None, "the old move is gone");
    // kB's keyframes return while "d" still primes: the old move to kB
    // must not land.
    no_keys.store(false, Ordering::Release);
    for _ in 0..15 {
        x.tick(t);
        t += 1;
        let o = &x.h.snapshot().outputs[1];
        assert_eq!(
            o.key,
            format!("{}/{}", o.bus, x.d.binding_video_key(1).unwrap()),
            "key and bus agree"
        );
    }
    assert_ne!(
        x.d.binding_video_key(1),
        Some(kb),
        "the old move never lands"
    );
    assert_eq!(x.d.binding_video_key(1), Some(0));
}

/// I1: a live encoder that blocks forever in `encode` (never drains its
/// input queue, `WORKER_QUEUE_TICKS` = 4): once its bounded queue fills, the
/// consumer's `offer` is rejected (`Overloaded`), never waited on. A dropped
/// offer counts as a held tick for that key's bus, so Clean — published
/// fresh by the bake consumer every tick, which would otherwise always look
/// "fresh" — degrades and falls back exactly like a lagging aux bus.
struct BlockedEnc {
    forced: bool,
    block: Arc<AtomicBool>,
    gate: Mutex<Receiver<()>>,
    /// The production encoder's contract (`encode_frame_at`): after
    /// `start_at(t0)` it accepts only its next tick, and any other tick is an
    /// error that ends the worker. Enforced here so gap-fill is tested against it.
    next: Option<u64>,
}

impl VideoEncodeWorker for BlockedEnc {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        // Block while `block` is set, polling it so a later `store(false,
        // ..)` lets this call return (used by the transient-stall test);
        // never releases on its own otherwise, so the input queue never
        // drains while `block` stays set.
        let gate = self.gate.lock().unwrap();
        while self.block.load(Ordering::SeqCst) {
            let _ = gate.recv_timeout(std::time::Duration::from_millis(5));
        }
        drop(gate);
        if let Some(next) = self.next {
            if item.tick != next {
                return Err(PipelineError::Output {
                    kind: "encode",
                    reason: format!(
                        "video tick discontinuity: expected {next}, got {}",
                        item.tick
                    ),
                });
            }
        }
        self.next = Some(item.tick + 1);
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

impl LiveEncoder for BlockedEnc {
    fn start_at(&mut self, t0: u64) {
        self.forced = true;
        self.next = Some(t0);
    }
    fn time_base(&self) -> Rational {
        Rational::new(1, 5)
    }
    fn move_gate(&self) -> MoveGate {
        Box::new(|p| p.is_keyframe() && p.len() == 77)
    }
}

/// Build a harness whose live-opened encoder blocks forever in `encode` once
/// `block` is set (never draining its input queue, so `worker.offer` starts
/// returning `Overloaded` once the bounded queue fills). The returned sender
/// keeps the worker's gate open; drop it at the end of the test to let the
/// blocked worker thread return (never leaking a spinning/blocked thread).
fn blocked_harness(outputs: usize) -> (Harness, Arc<AtomicBool>, SyncSender<()>) {
    let block = Arc::new(AtomicBool::new(false));
    let b = Arc::clone(&block);
    let (release, gate) = mpsc::sync_channel::<()>(0);
    let mut gate = Some(gate);
    let open: OpenFn = Box::new(move |_| {
        let e: Box<dyn LiveEncoder> = Box::new(BlockedEnc {
            forced: false,
            block: Arc::clone(&b),
            gate: Mutex::new(gate.take().expect("opened once")),
            next: None,
        });
        Ok(e)
    });
    (harness_with(outputs, 6, Some(open), true), block, release)
}

/// (a): a Clean output whose live encoder worker is blocked never drains its
/// input queue -> `degraded` in the snapshot after ~60 held ticks, then
/// `Fallback(...)` back to Program by ~150. Output 0 (Program) stays
/// contiguous and on time throughout; every `on_tick` call stays well under
/// the real-time envelope (never waits on the blocked encoder).
#[test]
fn a_blocked_clean_encoder_degrades_then_falls_back_and_program_is_unaffected() {
    let (mut x, block, release) = blocked_harness(2);
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    let (_, now) = apply(&mut x, 5, "c");
    assert_eq!(x.d.binding_video_key(1), Some(1), "on the Clean key");
    // Let a few healthy ticks pass before wedging the encoder, so "held"
    // below counts cleanly from the wedge point.
    let mut t = now;
    for _ in 0..3 {
        x.tick(t);
        t += 1;
    }
    block.store(true, Ordering::SeqCst);
    let wedged_at = t;
    let mut degraded_at = None;
    let mut fell_back_at = None;
    // The pre-existing wedged-worker rule (`WEDGED_ENCODER_TICKS` = 90, a
    // worker producing no events at all) may close the key before the new
    // 150-tick held-bus rule even gets a chance to: either path is a
    // legitimate Fallback, so this loop accepts the first of either and
    // only pins the NEW behaviour this task adds (`degraded` raised from a
    // dropped offer, which did not happen before this change).
    for _ in 0..FALLBACK_AFTER_TICKS + 20 {
        let started = Instant::now();
        x.tick(t);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "on_tick never waits on the blocked encoder: took {:?}",
            started.elapsed()
        );
        if degraded_at.is_none() && x.h.snapshot().outputs[1].degraded {
            degraded_at = Some(t);
        }
        if matches!(
            x.results()
                .iter()
                .find(|r| matches!(r, TakeResult::Fallback(_))),
            Some(TakeResult::Fallback(_))
        ) {
            fell_back_at = Some(t);
            break;
        }
        t += 1;
    }
    // Margin: the worker's own bounded queue (`WORKER_QUEUE_TICKS`) must
    // first fill with real offers before any one is actually dropped (an
    // in-flight recv on the worker thread can also absorb one more), so the
    // "held" count only starts climbing a few ticks after the wedge; the
    // design doc states the bounds as "~60"/"~150" for exactly this reason.
    let sched_margin: u64 = 20;
    let degraded_at = degraded_at.expect("Clean degrades once its queue is saturated");
    let fell_back_at = fell_back_at.expect("Clean falls back to Program");
    assert!(
        degraded_at - wedged_at < DEGRADED_AFTER_TICKS + sched_margin,
        "degraded close to the 60-held-tick bound: {} ticks after the wedge",
        degraded_at - wedged_at + 1
    );
    assert!(
        fell_back_at - wedged_at < FALLBACK_AFTER_TICKS + sched_margin,
        "fallback close to the 150-held-tick bound: {} ticks after the wedge",
        fell_back_at - wedged_at + 1
    );
    assert_eq!(x.d.binding_video_key(1), Some(0), "back on Program");
    let o0: Vec<i64> = x
        .windows_of(0)
        .iter()
        .filter_map(|w| w.first().map(|p| p.0))
        .collect();
    assert!(
        o0.windows(2).all(|w| w[1] == w[0] + 1),
        "Program windows contiguous and on time throughout: {o0:?}"
    );
    assert_eq!(
        o0.len(),
        usize::try_from(t).unwrap(),
        "Program delivered exactly one window per tick"
    );
    drop(release);
}

/// (b): same scenario, on a source bus (not Clean): a live encoder whose
/// queue fills with dropped offers degrades and falls back just like Clean.
#[test]
fn a_blocked_source_encoder_degrades_then_falls_back() {
    let (mut x, block, release) = blocked_harness(2);
    x.tick(0);
    x.h.try_take(take("a", 1, src())).unwrap();
    let (_, now) = apply(&mut x, 1, "a");
    assert_eq!(x.d.binding_video_key(1), Some(1));
    let mut t = now;
    for _ in 0..3 {
        x.tick(t);
        t += 1;
    }
    block.store(true, Ordering::SeqCst);
    let wedged_at = t;
    let mut degraded_at = None;
    let mut fell_back_at = None;
    // As in the Clean test: the pre-existing wedged-worker rule (90 ticks of
    // total silence) may close the key before the new 150-tick held-bus
    // rule gets a chance to; either is a legitimate Fallback here.
    for _ in 0..FALLBACK_AFTER_TICKS + 20 {
        x.tick(t);
        if degraded_at.is_none() && x.h.snapshot().outputs[1].degraded {
            degraded_at = Some(t);
        }
        if x.results()
            .iter()
            .any(|r| matches!(r, TakeResult::Fallback(_)))
        {
            fell_back_at = Some(t);
            break;
        }
        t += 1;
    }
    // Margin, same reasoning as the Clean test above.
    let sched_margin: u64 = 20;
    let degraded_at = degraded_at.expect("degrades once its queue is saturated");
    let fell_back_at = fell_back_at.expect("falls back to Program");
    assert!(
        degraded_at - wedged_at < DEGRADED_AFTER_TICKS + sched_margin,
        "degraded close to the 60-held-tick bound"
    );
    assert!(
        fell_back_at - wedged_at < FALLBACK_AFTER_TICKS + sched_margin,
        "fallback close to the 150-held-tick bound"
    );
    assert_eq!(x.d.binding_video_key(1), Some(0), "back on Program");
    drop(release);
}

/// (c): a transient stall of fewer than `MAX_GAP_FILL_TICKS` dropped offers
/// must not degrade or fall back: the encoder catches up (unblocks) well
/// inside the gap-fill horizon.
#[test]
fn a_transient_offer_stall_within_the_gap_fill_horizon_never_degrades() {
    let (mut x, block, release) = blocked_harness(2);
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    let (_, now) = apply(&mut x, 5, "c");
    let mut t = now;
    for _ in 0..3 {
        x.tick(t);
        t += 1;
    }
    block.store(true, Ordering::SeqCst);
    // A short stall (6 ticks, as when this test was written against the old
    // 8-tick gap-fill bound): well under the degraded bound.
    let stall_ticks: u64 = 6;
    for _ in 0..stall_ticks {
        x.tick(t);
        t += 1;
    }
    block.store(false, Ordering::SeqCst);
    for _ in 0..10 {
        x.tick(t);
        t += 1;
    }
    assert!(
        !x.h.snapshot().outputs[1].degraded,
        "a transient stall under the gap-fill horizon never degrades"
    );
    assert!(
        x.results()
            .iter()
            .all(|r| !matches!(r, TakeResult::Fallback(_))),
        "no fallback from a transient stall"
    );
    assert_eq!(x.d.binding_video_key(1), Some(1), "still on the Clean key");
    drop(release);
}

/// #329: a refused boot take is re-issued no sooner than the retry period;
/// an operator take on the same output ends the boot intent for good (the
/// boot take is superseded and never re-issued).
#[test]
fn an_operator_take_ends_a_boot_take_retry_window() {
    let (mut r, h, _aux) = router(2, 6, Box::new(|_| Err("never".to_owned())));
    let outcomes = h.outcomes().unwrap();
    let mut d = dispatcher(2);
    let boot = OutputTake {
        operation_id: format!("{BOOT_TAKE_PREFIX}o1"),
        output_index: 1,
        bus: Some(PictureBus::Source("cam".to_owned())),
        audio: None,
        principal: BOOT_PRINCIPAL.to_owned(),
    };
    h.try_take(boot).unwrap();
    // CADENCE is 5/1: 2 s = 10 ticks; the prime deadline is 50 ticks.
    let mut t = 0;
    let mut refused = Vec::new();
    while refused.len() < 2 {
        r.on_tick(t, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
            .unwrap();
        refused.extend(outcomes.try_iter().map(|o| (o.tick, o.outcome)));
        t += 1;
        assert!(t < 400, "never retried: {refused:?}");
    }
    assert!(refused
        .iter()
        .all(|(_, o)| *o == TakeResult::Refused(RefuseReason::FeedNotFresh)));
    assert!(refused[1].0 - refused[0].0 >= TAKE_PRIME_DEADLINE_TICKS);
    assert_eq!(r.snapshot.load().boot_takes_pending, 1);
    // The operator takes Program on that output while the boot take retries.
    h.try_take(take("op", 1, PictureBus::Program)).unwrap();
    for _ in 0..200 {
        r.on_tick(t, MediaTime::from_nanos(0), &mut d, &mut NoDelivery)
            .unwrap();
        t += 1;
    }
    let after: Vec<_> = outcomes
        .try_iter()
        .map(|o| (o.operation_id, o.outcome))
        .collect();
    // At most one boot outcome (the pending attempt, superseded) and then
    // only the operator's take.
    let boot: Vec<_> = after
        .iter()
        .filter(|(op, _)| op.starts_with(BOOT_TAKE_PREFIX))
        .collect();
    assert!(
        boot.len() <= 1 && boot.iter().all(|(_, o)| *o == TakeResult::Superseded),
        "the boot take is never re-issued after an operator take: {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|(op, o)| op == "op" && *o == TakeResult::Applied),
        "{after:?}"
    );
    assert_eq!(r.snapshot.load().boot_takes_pending, 0);
}

/// R2-1 (media lens): a bake-consumer stall sheds hot-queue ticks from the
/// Clean bus exactly like Program's own base-worker substitution loop.
/// Scenario: a genuine consumer gap of `k = 8` missed ticks (never
/// published/ticked at all, simulating what the pipeline's
/// `missing_ticks_before` computes when the hot queue sheds them),
/// substituted via `substitute_clean` before the surviving tick's own
/// `publish_clean`/`on_tick`. Observed directly at the Clean live encoder
/// (`PtrEnc`, which records every tick `encode()` actually received, i.e.
/// strictly AFTER `GapFill` has already tried to paper over any gap):
/// without `substitute_clean`, the Clean worker would be offered only the
/// surviving tick 8+ ticks after its last offer — a gap beyond `GapFill`'s
/// own `MAX_GAP_FILL_TICKS` bound — and `GapFill::encode` would return an
/// error (`tick gap ...`), ending the worker thread and failing the key
/// (`Fallback`). The fix makes the Clean worker see every tick strictly
/// consecutively (the substituted ticks, then the surviving tick) and the key
/// survives. The test pins that substitution itself delivers the gap (the
/// first four substituted ticks reach the encoder before anything new is
/// published); on a loaded host the tail of a long substitution can be
/// dropped by the bounded queue and is closed by `GapFill` at the next
/// accepted offer (one extra tick at most), as `substitute_clean` documents.
#[test]
fn a_consumer_gap_on_clean_is_substituted_like_program_and_never_fails_the_key() {
    let (mut x, seen) = ptr_harness(2);
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    let (_, now) = apply(&mut x, 5, "c");
    let mut t = now;
    for _ in 0..3 {
        x.tick(t);
        t += 1;
    }
    let last_fed_tick = t;
    // Bounded waits that publish NOTHING: only the worker thread advances.
    let wait_for_encoded = |tick: u64| {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while Instant::now() < deadline && !seen.lock().unwrap().iter().any(|(s, _)| *s >= tick) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };
    // The worker has drained every pre-gap tick, so its bounded queue
    // (`WORKER_QUEUE_TICKS` = 4) is empty when the gap is substituted.
    wait_for_encoded(last_fed_tick - 1);
    // The genuine consumer gap: `FRAME_SUBSTITUTION_LIMIT` in `pipeline.rs`
    // is 8, matching `MAX_GAP_FILL_TICKS` — exactly the boundary where an
    // unfilled gap handed straight to the Clean worker (no
    // `substitute_clean`) would fail it outright, since `GapFill::encode`
    // checks `item.tick - next > MAX_GAP_FILL_TICKS` BEFORE filling anything
    // in, and 8 is already over that strict `>` bound once GapFill's own
    // `next` is one past `last_fed_tick`.
    let gap: u64 = 8;
    let surviving_tick = last_fed_tick + gap + 1;
    x.tick_after_clean_gap(last_fed_tick + 1, surviving_tick);
    // Substitution, not the GapFill backstop: with the queue empty, the
    // first four substituted ticks are accepted whatever the scheduling, and
    // they reach the encoder before any later tick is published.
    let first_four = last_fed_tick + 4;
    wait_for_encoded(first_four);
    {
        let got: Vec<u64> = seen.lock().unwrap().iter().map(|(s, _)| *s).collect();
        assert!(
            (last_fed_tick + 1..=first_four).all(|s| got.contains(&s)),
            "the substituted ticks reached the encoder before any new frame: {got:?}"
        );
    }
    // On a loaded runner the worker may not keep up with the rest of the
    // substitution: `substitute_clean` stops at the first refused offer, and
    // the surviving tick's own offer can be dropped (never blocked). The
    // key's `GapFill` then closes the rest at the next accepted offer, by
    // design (its bound covers a full substitution gap plus a full queue).
    // At most ONE further tick is published for that backstop.
    wait_for_encoded(surviving_tick);
    if !seen
        .lock()
        .unwrap()
        .iter()
        .any(|(s, _)| *s == surviving_tick)
    {
        x.tick(surviving_tick + 1);
        wait_for_encoded(surviving_tick + 1);
    }
    assert!(
        x.results()
            .iter()
            .all(|r| !matches!(r, TakeResult::Fallback(_))),
        "the Clean key must not fail from a substituted consumer gap: {:?}",
        x.results()
    );
    assert!(
        !x.d.is_key_failed(1),
        "the Clean key's encoder must not have ended from an unfilled gap"
    );
    let ticks: Vec<u64> = seen.lock().unwrap().iter().map(|(t, _)| *t).collect();
    assert!(
        ticks.contains(&surviving_tick),
        "the surviving tick reached the Clean encoder: {ticks:?}"
    );
    assert!(
        ticks.windows(2).all(|w| w[1] == w[0] + 1),
        "the Clean encoder's observed tick sequence is strictly consecutive \
         with no gap, proving the substituted ticks closed it: {ticks:?}"
    );
    assert!(
        ticks
            .iter()
            .min()
            .is_some_and(|&first| first <= last_fed_tick),
        "the sequence covers back through the last healthy tick: {ticks:?}"
    );
}

/// PR #326 round 3 (invariants + abuse lenses): starvation is counted per
/// KEY. A second, healthy key on the same picture bus (another output with
/// other encoder settings) accepts its offers every tick; that must not hide
/// the starved key the output rides: the output is marked degraded at about
/// `DEGRADED_AFTER_TICKS` refused offers, before any fallback.
#[test]
fn a_healthy_sibling_key_never_hides_a_starved_key_on_the_same_bus() {
    let (mut x, block, release) = blocked_harness(2);
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    let (_, now) = apply(&mut x, 5, "c");
    assert_eq!(x.d.binding_video_key(1), Some(1), "on the Clean key");
    // A healthy sibling on Clean (as a second output with other settings).
    let live = Arc::new(AtomicUsize::new(0));
    let sibling = inject(
        &mut x.r,
        &mut x.d,
        PictureBus::Clean,
        Counted::boxed(&live, 0),
    );
    let mut t = now;
    for _ in 0..3 {
        x.tick(t);
        t += 1;
    }
    block.store(true, Ordering::SeqCst);
    let wedged_at = t;
    let mut degraded_at = None;
    for _ in 0..FALLBACK_AFTER_TICKS + 20 {
        x.tick(t);
        let fell_back = x
            .results()
            .iter()
            .any(|r| matches!(r, TakeResult::Fallback(_)));
        if x.h.snapshot().outputs[1].degraded {
            degraded_at = Some(t);
            break;
        }
        assert!(
            !fell_back,
            "the starved key fell back before it was ever reported degraded"
        );
        t += 1;
    }
    let degraded_at = degraded_at.expect("the starved key's output is marked degraded");
    assert!(
        degraded_at - wedged_at < DEGRADED_AFTER_TICKS + 20,
        "degraded near the 60-refused-offer bound: {} ticks after the wedge",
        degraded_at - wedged_at
    );
    assert!(
        x.r.dynamic.iter().any(|d| d.key == sibling),
        "the healthy sibling key is untouched"
    );
    drop(release);
}

/// PR #326 round 3: a consumer gap while the Clean worker's queue is already
/// full. Substitution offers are refused; the refused ticks must stay a gap
/// the key's own `GapFill` closes once the encoder drains, never a jump that
/// fails the key.
#[test]
fn a_consumer_gap_on_a_full_clean_queue_never_fails_the_key() {
    let (mut x, block, release) = blocked_harness(2);
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    let (_, now) = apply(&mut x, 5, "c");
    let mut t = now;
    for _ in 0..3 {
        x.tick(t);
        t += 1;
    }
    // Fill the worker's queue (and its in-flight slot).
    block.store(true, Ordering::SeqCst);
    for _ in 0..6 {
        x.tick(t);
        t += 1;
    }
    // A full consumer gap of 8 ticks while the queue is full.
    let surviving = t + 8;
    x.tick_after_clean_gap(t, surviving);
    t = surviving + 1;
    block.store(false, Ordering::SeqCst);
    for _ in 0..20 {
        x.tick(t);
        t += 1;
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    let results = x.results();
    assert!(
        results
            .iter()
            .all(|r| !matches!(r, TakeResult::Fallback(_))),
        "the Clean key survives the gap: {results:?}"
    );
    assert!(!x.d.is_key_failed(1), "the Clean key's worker did not end");
    assert_eq!(x.d.binding_video_key(1), Some(1), "still on the Clean key");
    drop(release);
}

/// PR #326 round 4: a key that refuses every substitution offer but accepts
/// every surviving tick is still starved: the accepted survivor after a
/// refused substitution never resets the count, so repeated consumer gaps
/// reach the degraded bound.
#[test]
fn refused_substitutions_count_toward_starvation_across_accepted_survivors() {
    let (mut x, seen) = ptr_harness(2);
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("c", 1, PictureBus::Clean)).unwrap();
    let (_, now) = apply(&mut x, 5, "c");
    x.tick(now);
    let key = x.d.binding_video_key(1).unwrap();
    // Model "refuse the substitution, accept the survivor" directly on the
    // key's bookkeeping: each round, a substitution offer is refused.
    for round in 0..=DEGRADED_AFTER_TICKS {
        if let Some(d) = x.r.dynamic.iter_mut().find(|d| d.key == key) {
            d.starved = d.starved.saturating_add(1);
            d.refused_this_tick = true;
        }
        x.tick(now + 1 + round);
    }
    let starved = x.r.dynamic.iter().find(|d| d.key == key).map(|d| d.starved);
    assert!(
        starved.is_some_and(|s| s > DEGRADED_AFTER_TICKS),
        "accepted survivors did not reset the refusals: {starved:?}"
    );
    assert!(x.h.snapshot().outputs[1].degraded, "the output is degraded");
    drop(seen);
}

// ---- #342: routed feeds at an output format ----

fn vertical_format() -> crate::routing_aux::AuxFormat {
    crate::routing_aux::AuxFormat::new(
        1080,
        1920,
        multiview_config::OutputFit::new(
            multiview_config::OutputFitMode::Cover,
            multiview_config::Anchor::Center,
        ),
    )
}

fn take_until_applied(x: &mut Harness, ops: &[&str], from: u64) -> u64 {
    let mut applied = 0;
    let mut now = from;
    while now < from + OPEN_DEADLINE_TICKS && applied < ops.len() {
        x.tick(now);
        for o in x.outcomes.try_iter() {
            if o.outcome == TakeResult::Applied && ops.contains(&o.operation_id.as_str()) {
                applied += 1;
            }
        }
        now += 1;
    }
    assert_eq!(applied, ops.len(), "every take applied");
    now
}

#[test]
fn two_outputs_with_the_same_format_share_one_dynamic_key_and_one_feed() {
    let mut x = harness(2, 6, None);
    x.r.cfg.formats = vec![Some(vertical_format()); 2];
    for t in 0..5 {
        x.tick(t);
    }
    // (Takes landing in the same tick, before the first open completes, may
    // open twice, exactly as for canvas-format outputs; a later take shares.)
    x.h.try_take(take("a", 0, src())).unwrap();
    let mut now = take_until_applied(&mut x, &["a"], 5);
    // Past the key's forced-IDR floor, so the joining take gets its IDR.
    for _ in 0..FORCE_FLOOR_TICKS {
        x.tick(now);
        now += 1;
    }
    x.h.try_take(take("b", 1, src())).unwrap();
    take_until_applied(&mut x, &["b"], now);
    assert_eq!(
        x.r.feed_keys(),
        vec![(src(), Some(vertical_format()))],
        "one feed per (bus, format)"
    );
    let keys = x.r.dynamic_keys();
    assert_eq!(keys.len(), 1, "one dynamic key: {keys:?}");
    assert_eq!(keys[0].1, Some(vertical_format()));
    assert_eq!(x.d.binding_video_key(0), Some(keys[0].3));
    assert_eq!(x.d.binding_video_key(1), Some(keys[0].3));
    assert!(
        x.r.slot(&src()).is_none(),
        "no canvas-format feed is composed for formatted outputs"
    );
}

#[test]
fn two_outputs_with_different_formats_get_two_feeds_and_two_keys() {
    let mut x = harness(2, 6, None);
    x.r.cfg.formats = vec![Some(vertical_format()), None];
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("a", 0, src())).unwrap();
    x.h.try_take(take("b", 1, src())).unwrap();
    take_until_applied(&mut x, &["a", "b"], 5);
    assert_eq!(
        x.r.feed_keys(),
        vec![(src(), None), (src(), Some(vertical_format()))],
        "one feed per format of the same source"
    );
    let keys = x.r.dynamic_keys();
    assert_eq!(
        keys.len(),
        2,
        "one key per (picture, format, settings): {keys:?}"
    );
    assert_ne!(x.d.binding_video_key(0), x.d.binding_video_key(1));
    let formatted = keys.iter().find(|k| k.1.is_some()).unwrap();
    assert_eq!(x.d.binding_video_key(0), Some(formatted.3));
}

#[test]
fn a_formatted_output_falls_back_to_its_own_base_key() {
    let mut x = harness(2, 6, None);
    x.r.cfg.formats = vec![Some(vertical_format()), None];
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("a", 0, src())).unwrap();
    let now = take_until_applied(&mut x, &["a"], 5);
    // The routed feed stops: the lag rule moves the output to its base key.
    x.aux_publishes = false;
    let mut fell = None;
    for t in now..now + FALLBACK_AFTER_TICKS + 20 {
        x.tick(t);
        if x.outcomes
            .try_iter()
            .any(|o| matches!(o.outcome, TakeResult::Fallback(_)))
        {
            fell = Some(t);
            break;
        }
    }
    let fell = fell.expect("a held formatted feed falls back");
    for t in fell + 1..fell + 12 {
        x.tick(t);
    }
    assert_eq!(x.d.binding_video_key(0), Some(x.r.cfg.base_keys[0]));
}

/// #342 review A2: the published live key carries the output format, and the
/// plan counts a take at a different format onto the same source as +1.
#[test]
fn the_plan_counts_a_different_format_as_plus_one() {
    use multiview_control::output_routing::{OutputRouting, RoutedFeed};
    let mut x = harness(2, 6, None);
    x.r.cfg.formats = vec![Some(vertical_format()), None];
    let h = x.h.clone().with_outputs(vec![
        RunOutput {
            id: Some("tall".into()),
            settings: 0,
            feed_format: Some(vertical_format()),
            routable: true,
            program_audio: true,
        },
        RunOutput {
            id: Some("wide".into()),
            settings: 0,
            feed_format: None,
            routable: true,
            program_audio: true,
        },
    ]);
    let slot = output_routing_slot();
    slot.store(Some(Arc::new(h)));
    let adapter = crate::output_routing_control::OutputRoutingAdapter::new(Arc::clone(&slot));
    let cam = RoutedFeed::Source("cam".into());
    for t in 0..5 {
        x.tick(t);
    }
    x.h.try_take(take("a", 0, src())).unwrap();
    take_until_applied(&mut x, &["a"], 5);
    let name = crate::routing_aux::feed_name(&src(), Some(vertical_format()));
    assert_eq!(x.h.snapshot().live_keys, vec![(name, 0)]);
    assert!(
        adapter.plan("wide", &cam).unwrap().needs_encoder,
        "same settings index, other format: +1"
    );
    assert!(
        !adapter.plan("tall", &cam).unwrap().needs_encoder,
        "same (bus, format, settings): +0"
    );
}

/// #342 review A2: the format-keyed feed set is bounded by the dynamic-key
/// caps (live keys + opens <= `max_encoders`) plus pending takes, whatever the
/// number of distinct formats requested.
#[test]
fn format_keyed_feeds_are_bounded_by_the_encoder_cap() {
    const OUTPUTS: usize = 6;
    const MAX: usize = 2;
    let mut x = harness(OUTPUTS, MAX, None);
    x.r.cfg.max_per_principal = OUTPUTS;
    x.r.cfg.formats = (0..OUTPUTS)
        .map(|i| {
            Some(crate::routing_aux::AuxFormat::new(
                64 + 16 * u32::try_from(i).unwrap(),
                64,
                multiview_config::OutputFit::default(),
            ))
        })
        .collect();
    for t in 0..5 {
        x.tick(t);
    }
    for i in 0..OUTPUTS {
        x.h.try_take(take(&i.to_string(), i, src())).unwrap();
    }
    let mut max_feeds = 0;
    for t in 5..5 + OPEN_DEADLINE_TICKS {
        x.tick(t);
        max_feeds = max_feeds.max(x.r.feed_keys().len());
        assert!(x.r.dynamic_keys().len() <= MAX, "encoder cap holds");
    }
    assert!(
        max_feeds <= MAX + OUTPUTS,
        "feeds <= max_encoders + pending takes: {max_feeds}"
    );
    // Once the takes settle (refused beyond the cap), only live keys hold feeds.
    assert!(x.r.feed_keys().len() <= MAX, "{:?}", x.r.feed_keys());
    assert_eq!(x.r.dynamic_keys().len(), MAX);
}

fn layout_bus(id: &str) -> PictureBus {
    use multiview_core::layout::{Canvas, Cell, Layout};
    PictureBus::Layout(crate::routing_aux::LayoutRef::new(Arc::new(
        crate::aux_layout::RoutedLayout {
            id: id.to_owned(),
            layout: Layout {
                name: id.to_owned(),
                canvas: Canvas {
                    width: 16,
                    height: 16,
                    fps_num: 5,
                    fps_den: 1,
                },
                cells: vec![Cell {
                    w: 1.0,
                    h: 1.0,
                    source: Some("cam".to_owned()),
                    ..Cell::default()
                }],
            },
            slates: vec![multiview_config::FailoverSlate::Black],
            audio: multiview_config::LayoutAudio::default(),
            revision: crate::output_routing::next_layout_revision(),
        },
    )))
}

impl Harness {
    /// One tick with the aux thread "composing" every active layout bus.
    fn tick_layouts(&mut self, t: u64, buses: &[PictureBus]) {
        for bus in buses {
            if let Some(slot) = self.r.slot(bus) {
                slot.latest
                    .store(Some(Arc::new(crate::routing_aux::AuxFrame {
                        tick: t,
                        generation: 1,
                        frame: canvas(),
                    })));
            }
        }
        self.tick(t);
    }
}

/// ADR-0108 slice 3 admission and failure (design §7, §4.4): with
/// `max_aux_canvases = 1`, a second distinct layout is refused `Capacity`
/// while the first composes; a second output on the SAME layout shares its
/// canvas (and encoder: +0); the aux thread failing moves every output on a
/// layout back to Program at once, and Program never misses a window.
#[test]
fn layout_canvases_are_admitted_shared_and_fall_back_when_aux_fails() {
    let mut x = harness(3, 6, None);
    x.r.cfg.max_aux_canvases = 1;
    let (l1, l2) = (layout_bus("L1"), layout_bus("L2"));
    let buses = [l1.clone(), l2.clone()];
    let mut t = 0;
    x.tick_layouts(t, &buses);
    x.h.try_take(take("one", 1, l1.clone())).unwrap();
    let mut got = Vec::new();
    while !got
        .iter()
        .any(|(op, r)| op == "one" && *r == TakeResult::Applied)
    {
        t += 1;
        x.tick_layouts(t, &buses);
        got.extend(x.outcomes.try_iter().map(|o| (o.operation_id, o.outcome)));
        assert!(t < 60, "{got:?}");
    }
    assert_eq!(x.h.snapshot().aux_canvases, vec!["aux:L1".to_owned()]);
    assert!(
        x.aux_active.load(Ordering::Acquire),
        "the aux thread is woken"
    );
    // A second layout: refused at the cap, output 2 keeps Program.
    x.h.try_take(take("two", 2, l2.clone())).unwrap();
    t += 1;
    x.tick_layouts(t, &buses);
    assert_eq!(
        x.results(),
        vec![TakeResult::Refused(RefuseReason::Capacity)]
    );
    assert_eq!(x.d.binding_video_key(2), Some(0));
    // The same layout on output 2 shares the canvas and the encoder (past
    // the per-key forced-IDR floor: the fake encoder has no natural IDR).
    for _ in 0..FORCE_FLOOR_TICKS {
        t += 1;
        x.tick_layouts(t, &buses);
    }
    let keys = x.d.video_key_count();
    x.h.try_take(take("share", 2, l1.clone())).unwrap();
    let mut shared = false;
    let mut seen = Vec::new();
    for _ in 0..60 {
        t += 1;
        x.tick_layouts(t, &buses);
        seen.extend(x.outcomes.try_iter().map(|o| (o.operation_id, o.outcome)));
        if seen
            .iter()
            .any(|(op, r)| op == "share" && *r == TakeResult::Applied)
        {
            shared = true;
            break;
        }
    }
    assert!(
        shared,
        "the second output joins the composing layout: {seen:?}"
    );
    assert_eq!(x.d.video_key_count(), keys, "+0 encoders");
    assert_eq!(x.h.snapshot().aux_canvases, vec!["aux:L1".to_owned()]);
    // Output 1 moving from L1 to L2 is still refused: output 2 holds L1.
    x.h.try_take(take("swap", 1, l2.clone())).unwrap();
    t += 1;
    x.tick_layouts(t, &buses);
    assert_eq!(
        x.results(),
        vec![TakeResult::Refused(RefuseReason::Capacity)]
    );
    // The aux thread dies: both layout outputs fall back to Program at once.
    x.failed.store(true, Ordering::Release);
    t += 1;
    x.tick_layouts(t, &buses);
    let fallen = x.results();
    assert_eq!(
        fallen,
        vec![TakeResult::Fallback(FallbackReason::AuxFailed); 2],
        "{fallen:?}"
    );
    for _ in 0..5 {
        t += 1;
        x.tick_layouts(t, &buses);
    }
    assert_eq!(x.d.binding_video_key(1), Some(0));
    assert_eq!(x.d.binding_video_key(2), Some(0));
    assert_eq!(
        x.windows_of(0).len(),
        usize::try_from(t).unwrap(),
        "Program never missed a window"
    );
}

/// A layout whose canvas stops publishing (every cell's tile store wedged is
/// NOT this: dead cells show slates; this is the aux canvas itself holding)
/// degrades at 60 held ticks and falls back at 150, like any aux bus.
#[test]
fn a_held_layout_canvas_degrades_then_falls_back() {
    let mut x = harness(2, 6, None);
    let l1 = layout_bus("L1");
    let mut t = 0;
    x.tick_layouts(t, std::slice::from_ref(&l1));
    x.h.try_take(take("one", 1, l1.clone())).unwrap();
    while !x
        .outcomes
        .try_iter()
        .any(|o| o.outcome == TakeResult::Applied)
    {
        t += 1;
        x.tick_layouts(t, std::slice::from_ref(&l1));
        assert!(t < 60);
    }
    let held_from = t + 1;
    let mut fell = None;
    for _ in 0..FALLBACK_AFTER_TICKS + 5 {
        t += 1;
        x.tick(t); // nothing composes the layout any more
        if x.outcomes
            .try_iter()
            .any(|o| o.outcome == TakeResult::Fallback(FallbackReason::AuxFailed))
        {
            fell = Some(t);
            break;
        }
    }
    assert_eq!(fell.map(|f| f - held_from + 1), Some(FALLBACK_AFTER_TICKS));
    for _ in 0..10 {
        t += 1;
        x.tick(t);
    }
    assert_eq!(
        x.d.binding_video_key(1),
        Some(0),
        "back on Program at a RAP"
    );
}

/// #368 review (lens 1 #3): a re-take of a composing layout with an edited
/// body swaps the body the aux thread composes and requests an IDR on that
/// layout's encoder, so the edited picture starts on a random-access point.
#[test]
fn a_re_take_of_an_edited_layout_swaps_the_body_and_requests_an_idr() {
    let mut x = harness(2, 6, None);
    let first = layout_bus("L1");
    let mut t = 0;
    x.tick_layouts(t, std::slice::from_ref(&first));
    x.h.try_take(take("one", 1, first.clone())).unwrap();
    while !x
        .outcomes
        .try_iter()
        .any(|o| o.outcome == TakeResult::Applied)
    {
        t += 1;
        x.tick_layouts(t, std::slice::from_ref(&first));
        assert!(t < 60);
    }
    // Past the per-key forced-IDR floor.
    for _ in 0..FORCE_FLOOR_TICKS {
        t += 1;
        x.tick_layouts(t, std::slice::from_ref(&first));
    }
    let edited = layout_bus("L1");
    let (PictureBus::Layout(old), PictureBus::Layout(new)) = (&first, &edited) else {
        panic!("layout buses");
    };
    assert!(!Arc::ptr_eq(&old.body, &new.body), "a new body");
    x.h.try_take(take("edit", 1, edited.clone())).unwrap();
    t += 1;
    x.tick_layouts(t, std::slice::from_ref(&edited));
    let composing = x.r.aux.feeds.load();
    let body = composing
        .iter()
        .find_map(|f| match &f.bus {
            PictureBus::Layout(l) => Some(Arc::clone(&l.body)),
            _ => None,
        })
        .expect("the layout still composes");
    assert!(
        Arc::ptr_eq(&body, &new.body),
        "the aux thread composes the edit"
    );
    assert_eq!(
        x.r.last_forced.get(&x.r.dynamic[0].key).copied(),
        Some(t + 1),
        "an IDR is requested on the layout's encoder"
    );
}

/// A layout taken onto output 1, ticked until applied. Returns the tick.
fn on_layout(x: &mut Harness, bus: &PictureBus) -> u64 {
    let mut t = 0;
    x.tick_layouts(t, std::slice::from_ref(bus));
    x.h.try_take(take("one", 1, bus.clone())).unwrap();
    while !x
        .outcomes
        .try_iter()
        .any(|o| o.outcome == TakeResult::Applied)
    {
        t += 1;
        x.tick_layouts(t, std::slice::from_ref(bus));
        assert!(t < 60);
    }
    t
}

/// PR 2 (#306): an edit of a stored layout reaches the routed canvas live,
/// with no take: the body swaps, the output stays on the layout, an IDR is
/// requested, the decoders' aux sizing follows the new cells, and no take
/// outcome is emitted.
#[test]
fn a_live_layout_edit_swaps_the_body_without_a_take() {
    let mut x = harness(2, 6, None);
    let sizing = crate::display_size::aux_sizing_slot();
    x.r.aux.sizing = Some(Arc::clone(&sizing));
    let first = layout_bus("L1");
    let mut t = on_layout(&mut x, &first);
    assert_eq!(sizing.load().len(), 1, "the routed cell sizes its decode");
    for _ in 0..FORCE_FLOOR_TICKS {
        t += 1;
        x.tick_layouts(t, std::slice::from_ref(&first));
    }
    let edited = layout_bus("L1");
    let PictureBus::Layout(new) = &edited else {
        panic!("a layout bus");
    };
    x.h.try_layout_change(crate::output_routing::LayoutChange::Edited(new.clone()))
        .unwrap();
    t += 1;
    x.tick_layouts(t, std::slice::from_ref(&edited));
    let body =
        x.r.aux
            .feeds
            .load()
            .iter()
            .find_map(|f| match &f.bus {
                PictureBus::Layout(l) => Some(Arc::clone(&l.body)),
                _ => None,
            })
            .expect("the layout still composes");
    assert!(Arc::ptr_eq(&body, &new.body), "the edit is live");
    assert_eq!(
        *sizing.load(),
        new.body.layout.cells,
        "the decoders size for the edited cells"
    );
    assert_eq!(
        x.r.last_forced.get(&x.r.dynamic[0].key).copied(),
        Some(t + 1),
        "an IDR is requested on the layout's encoder"
    );
    assert!(
        x.outcomes.try_iter().next().is_none(),
        "a live edit is not a take"
    );
    assert_eq!(x.h.snapshot().outputs[1].bus, "aux:L1");
    // An edit of a layout nothing routes is a no-op.
    let PictureBus::Layout(other) = layout_bus("Other") else {
        panic!("a layout bus");
    };
    x.h.try_layout_change(crate::output_routing::LayoutChange::Edited(other))
        .unwrap();
    t += 1;
    x.tick_layouts(t, std::slice::from_ref(&edited));
    assert_eq!(x.h.snapshot().aux_canvases, vec!["aux:L1".to_owned()]);
}

/// PR 2 (#306): a deleted (or no longer routable) layout moves every output
/// on it back to Program at a RAP, refuses a take still priming onto it, and
/// frees its canvas and its aux sizing.
#[test]
fn a_deleted_layout_falls_its_outputs_back_to_program() {
    let mut x = harness(3, 6, None);
    let sizing = crate::display_size::aux_sizing_slot();
    x.r.aux.sizing = Some(Arc::clone(&sizing));
    let l1 = layout_bus("L1");
    let mut t = on_layout(&mut x, &l1);
    // A second output starts a take onto L1 but has not landed yet: the
    // canvas is not published this tick, so it stays priming.
    x.h.try_take(take("two", 2, l1.clone())).unwrap();
    t += 1;
    x.tick(t);
    x.h.try_layout_change(crate::output_routing::LayoutChange::Removed {
        id: "L1".to_owned(),
        revision: crate::output_routing::next_layout_revision(),
    })
    .unwrap();
    t += 1;
    x.tick(t);
    let mut results = x.results();
    results.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(
        results,
        vec![
            TakeResult::Fallback(FallbackReason::AuxFailed),
            TakeResult::Refused(RefuseReason::Unroutable),
        ],
    );
    for _ in 0..10 {
        t += 1;
        x.tick(t);
    }
    assert_eq!(x.d.binding_video_key(1), Some(0), "back on Program");
    assert_eq!(x.h.snapshot().outputs[1].bus, "program");
    // Its encoder lingers like any unused key, then closes: the canvas and
    // its aux sizing go with it.
    for _ in 0..LINGER_TICKS + 5 {
        t += 1;
        x.tick(t);
    }
    assert_eq!(x.h.snapshot().aux_canvases, Vec::<String>::new());
    assert_eq!(sizing.load().len(), 0, "no routed cells left to size for");
    assert_eq!(
        x.windows_of(0).len(),
        usize::try_from(t).unwrap(),
        "Program never missed a window"
    );
}

/// PR 2 (#306, §7): while the aux cadence is halved, a routed picture one
/// tick old is still fresh and a skipped tick is not a hold, so a layout
/// composed every second tick neither degrades nor falls back.
#[test]
fn a_halved_aux_cadence_neither_degrades_nor_falls_back() {
    let mut x = harness(2, 6, None);
    let l1 = layout_bus("L1");
    let mut t = on_layout(&mut x, &l1);
    x.r.aux.halved.store(true, Ordering::Relaxed);
    for _ in 0..FALLBACK_AFTER_TICKS * 2 {
        t += 1;
        if t % 2 == 0 {
            x.tick_layouts(t, std::slice::from_ref(&l1));
        } else {
            x.tick(t);
        }
        assert!(!x.h.snapshot().outputs[1].degraded, "tick {t}");
    }
    assert!(x.outcomes.try_iter().next().is_none(), "no fallback");
    assert_eq!(x.h.snapshot().outputs[1].bus, "aux:L1");
    assert!(x.h.snapshot().aux_halved);
}

/// #369 review (lens 1 #1/#2): a take queued with a body older than a later
/// edit or removal of its layout is refused, never adopted: it can neither
/// revert the edit nor air a deleted layout.
#[test]
fn a_take_carrying_a_stale_layout_body_is_refused() {
    let mut x = harness(3, 6, None);
    let old = layout_bus("L1");
    let mut t = on_layout(&mut x, &old);
    let edited = layout_bus("L1");
    let PictureBus::Layout(new) = &edited else {
        panic!("a layout bus");
    };
    x.h.try_layout_change(crate::output_routing::LayoutChange::Edited(new.clone()))
        .unwrap();
    // Output 2's take was resolved before the edit (it carries `old`).
    x.h.try_take(take("stale", 2, old.clone())).unwrap();
    t += 1;
    x.tick_layouts(t, std::slice::from_ref(&edited));
    assert_eq!(
        x.results(),
        vec![TakeResult::Refused(RefuseReason::Unroutable)]
    );
    let composing =
        x.r.aux
            .feeds
            .load()
            .iter()
            .find_map(|f| match &f.bus {
                PictureBus::Layout(l) => Some(Arc::clone(&l.body)),
                _ => None,
            })
            .unwrap();
    assert!(Arc::ptr_eq(&composing, &new.body), "the edit stands");
    // A removal, then a take queued with the pre-removal body: refused.
    x.h.try_layout_change(crate::output_routing::LayoutChange::Removed {
        id: "L1".to_owned(),
        revision: crate::output_routing::next_layout_revision(),
    })
    .unwrap();
    x.h.try_take(take("after-delete", 2, edited.clone()))
        .unwrap();
    t += 1;
    x.tick(t);
    let results = x.results();
    assert!(
        results.contains(&TakeResult::Refused(RefuseReason::Unroutable)),
        "{results:?}"
    );
    assert_eq!(
        x.d.binding_video_key(2),
        Some(0),
        "never on the deleted layout"
    );
    // A layout re-created later (a newer resolution) is not stale: it is
    // admitted (whatever its encoder then does).
    let PictureBus::Layout(recreated) = layout_bus("L1") else {
        panic!("a layout bus");
    };
    assert!(!x.r.layout_take_is_stale(&recreated));
    assert!(x.r.layout_take_is_stale(new));
}

/// #369 review (lens 1 #4, lens 3 #1): a halved cadence never hides a
/// stalled canvas: with no new picture, the layout still falls back.
#[test]
fn a_stalled_canvas_falls_back_even_while_halved() {
    let mut x = harness(2, 6, None);
    let l1 = layout_bus("L1");
    let mut t = on_layout(&mut x, &l1);
    x.r.aux.halved.store(true, Ordering::Relaxed);
    let mut fell = false;
    for _ in 0..FALLBACK_AFTER_TICKS + 10 {
        t += 1;
        x.tick(t); // nothing composes
        if x.results()
            .contains(&TakeResult::Fallback(FallbackReason::AuxFailed))
        {
            fell = true;
            break;
        }
    }
    assert!(fell, "a stalled canvas falls back while halved");
}

/// #369 review (lens 3 #3): a burst of changes is coalesced per layout,
/// newest revision first, and never loses the last one.
#[test]
fn layout_changes_coalesce_and_keep_the_newest() {
    let mut x = harness(2, 6, None);
    let first = layout_bus("L1");
    let mut t = on_layout(&mut x, &first);
    let mut last = None;
    for _ in 0..100 {
        let PictureBus::Layout(next) = layout_bus("L1") else {
            panic!("a layout bus");
        };
        x.h.try_layout_change(crate::output_routing::LayoutChange::Edited(next.clone()))
            .unwrap();
        last = Some(next);
    }
    for i in 0..100 {
        x.h.try_layout_change(crate::output_routing::LayoutChange::Removed {
            id: format!("other-{i}"),
            revision: crate::output_routing::next_layout_revision(),
        })
        .unwrap();
    }
    t += 1;
    x.tick_layouts(t, std::slice::from_ref(&first));
    let composing =
        x.r.aux
            .feeds
            .load()
            .iter()
            .find_map(|f| match &f.bus {
                PictureBus::Layout(l) => Some(Arc::clone(&l.body)),
                _ => None,
            })
            .unwrap();
    assert!(Arc::ptr_eq(&composing, &last.unwrap().body));
    assert!(x.r.layout_states.len() <= crate::output_routing::LAYOUT_STATES);
    // States were forgotten: a take older than any forgotten state, onto a
    // layout with no remembered state, fails closed; a newer one does not.
    assert!(x.r.layout_states_forgotten > 0);
    let mut old = crate::aux_layout::RoutedLayout::clone(&first_body(&first));
    old.id = "other-0".to_owned();
    old.revision = 1;
    assert!(x
        .r
        .layout_take_is_stale(&crate::routing_aux::LayoutRef::new(Arc::new(old))));
    let PictureBus::Layout(fresh) = layout_bus("other-0") else {
        panic!("a layout bus");
    };
    assert!(!x.r.layout_take_is_stale(&fresh));
}

fn first_body(bus: &PictureBus) -> Arc<crate::aux_layout::RoutedLayout> {
    let PictureBus::Layout(layout) = bus else {
        panic!("a layout bus");
    };
    Arc::clone(&layout.body)
}

/// A layout bus with `audio` on the test canvas (one cell of `cam`).
fn layout_bus_with_audio(id: &str, audio: multiview_config::LayoutAudio) -> PictureBus {
    let PictureBus::Layout(base) = layout_bus(id) else {
        panic!("a layout bus");
    };
    let mut body = crate::aux_layout::RoutedLayout::clone(&base.body);
    body.audio = audio;
    body.revision = crate::output_routing::next_layout_revision();
    PictureBus::Layout(crate::routing_aux::LayoutRef::new(Arc::new(body)))
}

impl AudioHarness {
    /// Publish every active layout slot for tick `t` (the aux thread's job).
    fn compose_layouts(&self, t: u64) {
        for feed in self.r.aux.feeds.load().iter() {
            if matches!(feed.bus, PictureBus::Layout(_)) {
                feed.slot
                    .latest
                    .store(Some(Arc::new(crate::routing_aux::AuxFrame {
                        tick: t,
                        generation: 1,
                        frame: canvas(),
                    })));
            }
        }
    }
}

/// #369 review (lens 1): an output whose audio follows a routed layout
/// carries the layout's `audio` attribute, and a live edit of that attribute
/// is followed on air, also when the edit lands while the take's move is in
/// flight (the audio follows the body that landed).
#[test]
fn layout_audio_follows_live_edits_also_mid_move() {
    use multiview_config::LayoutAudio;
    let encoded = Arc::new(AtomicUsize::new(0));
    let open: OpenFn = Box::new(move |_| {
        let enc: Box<dyn LiveEncoder> = Box::new(FakeEnc {
            forced: false,
            encoded: Arc::clone(&encoded),
        });
        Ok(enc)
    });
    let mut x = audio_harness(2, CADENCE, open);
    let silent = layout_bus_with_audio("L", LayoutAudio::None);
    let mut t = x.run(0, 3);
    x.h.try_take(OutputTake {
        audio: Some(AudioChoice::Follow),
        ..take("to-layout", 1, silent)
    })
    .unwrap();
    // Edit as soon as the take is moving (the move's audio followed `none`).
    let mut edited = false;
    for _ in 0..60 {
        x.compose_layouts(t);
        x.tick(t);
        t += 1;
        let moving =
            x.r.outputs
                .get(1)
                .and_then(|o| o.pending.as_ref())
                .is_some_and(|p| matches!(p.phase, Phase::Moving(_)));
        if moving && !edited {
            let PictureBus::Layout(next) =
                layout_bus_with_audio("L", LayoutAudio::Source("bars".into()))
            else {
                panic!("a layout bus");
            };
            x.h.try_layout_change(crate::output_routing::LayoutChange::Edited(next))
                .unwrap();
            edited = true;
        }
        if x.outcomes
            .try_iter()
            .any(|o| o.operation_id == "to-layout" && o.outcome == TakeResult::Applied)
        {
            break;
        }
    }
    assert!(edited, "the take reached its move");
    for _ in 0..40 {
        x.compose_layouts(t);
        x.tick(t);
        t += 1;
    }
    let view = &x.h.snapshot().outputs[1];
    assert_eq!(view.bus, "aux:L");
    assert_eq!(view.audio, "src:bars", "the edited attribute is on air");
    // A later live edit to `none` is followed on air too.
    let PictureBus::Layout(silent_again) = layout_bus_with_audio("L", LayoutAudio::None) else {
        panic!("a layout bus");
    };
    x.h.try_layout_change(crate::output_routing::LayoutChange::Edited(silent_again))
        .unwrap();
    for _ in 0..40 {
        x.compose_layouts(t);
        x.tick(t);
        t += 1;
    }
    assert_eq!(x.h.snapshot().outputs[1].audio, "none");
}
