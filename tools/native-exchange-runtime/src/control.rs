//! Independent private-harness control workers. No worker runs on an output clock.
use crate::{media::Output, ReceiverRuntime};
use conspect_media_crypto::{authority_http::VerifiedHttpsTransport, bootstrap::Bootstrap};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

pub type Slot = Arc<Mutex<(ReceiverRuntime, Output)>>;

/// Gate for one receiver's grant worker. A cycle (root fetch, grant request,
/// install) runs only while the gate is open, and is "busy" only while it may
/// send a request or install a grant. `close` stops new cycles and waits until
/// no cycle is busy. A cycle is not busy while it sleeps for epoch admission
/// (`GatedClock::sleep`), and its next clock sample after that sleep fails once
/// the gate is closed, so no request follows a `close`. A caller that is about
/// to cut a taker off (a withdrawal it sends itself) closes the gate FIRST: no
/// root or grant request of that worker reaches the authority afterwards, and
/// `close` waits for at most the requests already in flight (each bounded by
/// the transport timeout), never for an admission sleep.
pub struct CycleGate {
    state: Mutex<(bool, bool)>, // (open, busy)
    idle: std::sync::Condvar,
}
impl Default for CycleGate {
    fn default() -> Self {
        Self::new()
    }
}
/// A busy cycle; dropping it marks the gate idle.
pub struct Cycle<'a>(&'a CycleGate);
impl Drop for Cycle<'_> {
    fn drop(&mut self) {
        self.0.set_busy(false);
    }
}
impl CycleGate {
    pub fn new() -> Self {
        Self {
            state: Mutex::new((true, false)),
            idle: std::sync::Condvar::new(),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, (bool, bool)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn set_busy(&self, busy: bool) {
        self.lock().1 = busy;
        if !busy {
            self.idle.notify_all();
        }
    }
    /// Start one busy cycle, or `None` once closed.
    pub fn enter(&self) -> Option<Cycle<'_>> {
        let mut state = self.lock();
        if !state.0 {
            return None;
        }
        state.1 = true;
        Some(Cycle(self))
    }
    /// Mark the cycle idle for an admission sleep (no request is in flight).
    fn pause(&self) {
        self.set_busy(false);
    }
    /// Mark the cycle busy again before its next request; false once closed.
    fn resume(&self) -> bool {
        let mut state = self.lock();
        state.1 = state.0;
        state.0
    }
    /// Stop new cycles without waiting (a deny found by the worker itself).
    pub fn shut(&self) {
        self.lock().0 = false;
    }
    /// Stop new cycles and wait until no cycle is busy.
    pub fn close(&self) {
        let mut state = self.lock();
        state.0 = false;
        while state.1 {
            state = self.idle.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
    pub fn is_open(&self) -> bool {
        self.lock().0
    }
}
/// The grant worker's epoch-admission clock. Every admission wait in the grant
/// path (`bounded_grant`: the tail wait and the 404 retry's boundary wait) is a
/// `sleep` followed by a `wall_ms` sample before the next request, so the
/// cycle is idle while it sleeps and cannot send a request once the gate has
/// closed meanwhile.
pub struct GatedClock {
    gate: Arc<[CycleGate; 2]>,
    index: usize,
}
impl conspect_media_crypto::bootstrap::EpochAdmissionClock for GatedClock {
    fn wall_ms(&self) -> conspect_media_crypto::bootstrap::Result<u64> {
        if !self.gate[self.index].resume() {
            return Err("grant worker stopped");
        }
        Ok(crate::wall_ms())
    }
    fn sleep(&self, duration: Duration) {
        self.gate[self.index].pause();
        thread::sleep(duration);
    }
}

/// A deny is sticky for the whole run. The watchdog does not wait for HTTP or decode.
pub struct Coordinator {
    stop: Arc<AtomicBool>,
    denied: Arc<AtomicBool>,
    active: Arc<[AtomicBool; 2]>,
    grants: Arc<[CycleGate; 2]>,
    workers: Vec<thread::JoinHandle<()>>,
    slots: Vec<Slot>,
}
impl Coordinator {
    pub fn start(
        mut lease: Bootstrap,
        transport: Arc<VerifiedHttpsTransport>,
        slots: Vec<Slot>,
        deadline: Arc<Mutex<Option<Instant>>>,
        key: ed25519_dalek::VerifyingKey,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let denied = Arc::new(AtomicBool::new(false));
        let active = Arc::new([AtomicBool::new(true), AtomicBool::new(true)]);
        let grants = Arc::new([CycleGate::new(), CycleGate::new()]);
        let mut workers = Vec::new();
        let shutdown_slots = slots.clone();
        let booking = lease.clone();
        {
            let (stop, denied, transport, deadline) = (
                stop.clone(),
                denied.clone(),
                transport.clone(),
                deadline.clone(),
            );
            workers.push(thread::spawn(move || {
                let mut due = Instant::now();
                while !stop.load(Ordering::Acquire) && !denied.load(Ordering::Acquire) {
                    if Instant::now() >= due {
                        // Absolute start-to-start cadence. Three-second HTTP timeout plus
                        // one-second cadence keeps actual renewal requests within five seconds.
                        due = Instant::now() + Duration::from_secs(1);
                        if lease.renew(&*transport).is_err() {
                            if let Ok(l) = lease.publisher_lease() {
                                let (cause, status) = l.last_failure();
                                eprintln!(
                                    "lease renew failed at {}: {cause}; last lease HTTP status {status}",
                                    crate::wall_ms()
                                );
                            }
                            denied.store(true, Ordering::Release);
                            *deadline.lock().unwrap() = None;
                            break;
                        }
                        let mut shared = deadline.lock().unwrap();
                        if denied.load(Ordering::Acquire) {
                            *shared = None;
                            break;
                        }
                        *shared = lease.deadline();
                        if shared.is_none() {
                            // Renew succeeded but the local permit is closed: the
                            // answer's notBeforeMs/leaseExpiresAtMs bound sealing.
                            eprintln!(
                                "lease renew ok at {} but the local permit is closed (notBefore/expiry bound)",
                                crate::wall_ms()
                            );
                        }
                    }
                    thread::park_timeout(Duration::from_millis(10));
                }
            }));
        }
        for i in 0..2 {
            let (stop, denied, active, transport, booking) = (
                stop.clone(),
                denied.clone(),
                active.clone(),
                transport.clone(),
                booking.clone(),
            );
            workers.push(thread::spawn(move || {
                let mut due = booking.extension_due(i).unwrap_or_else(|_| Instant::now());
                let mut sequence = 0;
                while !stop.load(Ordering::Acquire) && !denied.load(Ordering::Acquire) {
                    if Instant::now() >= due && active[i].load(Ordering::Acquire) {
                        due += Duration::from_secs(20);
                        sequence += 1;
                        if booking.extend_booking(&*transport, i, sequence).is_err() {
                            denied.store(true, Ordering::Release);
                        }
                        if i == 0 && booking.funded_expiry() {
                            break;
                        }
                    }
                    thread::park_timeout(Duration::from_millis(10));
                }
            }));
        }
        for i in 0..2 {
            let (stop, denied, gates, transport, slot) = (
                stop.clone(),
                denied.clone(),
                grants.clone(),
                transport.clone(),
                slots[i].clone(),
            );
            let booking = booking.clone().with_admission_clock(Arc::new(GatedClock {
                gate: grants.clone(),
                index: i,
            }));
            workers.push(thread::spawn(move || {
                let mut due = Instant::now();
                let mut attempt_epoch = u64::MAX;
                let mut attempts = 0;
                while !stop.load(Ordering::Acquire) && !denied.load(Ordering::Acquire) {
                    if Instant::now() >= due && gates[i].is_open() {
                        let epoch = crate::wall_ms() / 5000;
                        if epoch != attempt_epoch {
                            attempt_epoch = epoch;
                            attempts = 0;
                        }
                        if attempts >= 2 {
                            due = Instant::now()
                                + Duration::from_millis(5000 - crate::wall_ms() % 5000);
                            continue;
                        }
                        // Busy only for the requests and install below; idle on
                        // every exit of this block and during admission sleeps
                        // (`GatedClock`), so `close` never waits for a wait.
                        let Some(_cycle) = gates[i].enter() else {
                            continue;
                        };
                        attempts += 1;
                        due = Instant::now() + Duration::from_secs(1);
                        // The issuer's current epoch must exist before requesting a grant.
                        let grant = booking
                            .root(&*transport)
                            .and_then(|_| booking.grant(&*transport, i));
                        match grant {
                            Ok(grant) => {
                                // A response from an epoch that ended in transit cannot
                                // replace a newer grant installed by the media worker.
                                let epoch = u64::from_be_bytes(grant[144..152].try_into().unwrap());
                                if epoch != crate::wall_ms() / 5000 {
                                    due = Instant::now();
                                    continue;
                                }
                                let mut slot = slot.lock().unwrap();
                                let (receiver, output) = &mut *slot;
                                if receiver
                                    .refresh(&key, &grant, crate::wall_ms(), 0, output)
                                    .is_err()
                                {
                                    let _ = receiver.deny(output);
                                    gates[i].shut();
                                }
                            }
                            Err("grant near boundary") => {
                                let wait = 5000 - crate::wall_ms() % 5000;
                                due = Instant::now() + Duration::from_millis(wait);
                            }
                            // A late or lost answer is not a denial: the lease is checked
                            // inside `grant`, so a still-valid lease keeps retrying within the
                            // per-epoch attempt budget. Only an authoritative refusal
                            // ("grant denied": a status after the single 404 retry) denies.
                            Err("grant transport") => {}
                            // The epoch boundary overtook the root answer in transit:
                            // timing, not a refusal. The next attempt (this epoch's
                            // second, or the next epoch's first) is admitted out of
                            // the tail. A refused, equivocating or rolled-back root
                            // is "canonical root rejected" and still denies below.
                            Err(conspect_media_crypto::bootstrap::ROOT_EXPIRED_IN_TRANSIT) => {}
                            // The gate closed during an admission wait (the caller
                            // quiesced this worker): no request was sent; stop.
                            Err(_) if !gates[i].is_open() => {}
                            Err(_) => {
                                let mut slot = slot.lock().unwrap();
                                let (receiver, output) = &mut *slot;
                                let _ = receiver.deny(output);
                                gates[i].shut();
                            }
                        }
                    }
                    thread::park_timeout(Duration::from_millis(10));
                }
            }));
        }
        {
            let (stop, denied) = (stop.clone(), denied.clone());
            workers.push(thread::spawn(move || {
                let mut flushed = false;
                while !stop.load(Ordering::Acquire) {
                    if deadline.lock().unwrap().is_none_or(|d| Instant::now() >= d)
                        && !denied.swap(true, Ordering::AcqRel)
                    {
                        eprintln!(
                            "local seal permit closed at {} (lease deadline passed or permit withheld)",
                            crate::wall_ms()
                        );
                    }
                    let fail = denied.load(Ordering::Acquire);
                    if fail {
                        *deadline.lock().unwrap() = None;
                    }
                    for slot in &slots {
                        let mut slot = slot.lock().unwrap();
                        let (receiver, output) = &mut *slot;
                        if fail && !flushed {
                            let _ = receiver.deny(output);
                        } else if !fail {
                            let _ = receiver.fence(crate::wall_ms(), output);
                        }
                    }
                    if fail && !flushed {
                        eprintln!(
                            "control fence denied; native receiver queues flushed at {}",
                            crate::wall_ms()
                        );
                    }
                    flushed |= fail;
                    thread::park_timeout(Duration::from_millis(10));
                }
            }));
        }
        Self {
            stop,
            denied,
            active,
            grants,
            workers,
            slots: shutdown_slots,
        }
    }
    pub fn deactivate(&self, i: usize) {
        self.active[i].store(false, Ordering::Release);
        self.grants[i].shut();
    }
    /// Stop receiver `i`'s grant worker and wait for a cycle in flight: after
    /// this returns that worker sends no root or grant request. Call it BEFORE
    /// sending a cutoff (withdrawal) that the worker must not outlive.
    pub fn quiesce(&self, i: usize) {
        self.deactivate(i);
        self.grants[i].close();
    }
    pub fn healthy(&self) -> bool {
        !self.denied.load(Ordering::Acquire)
    }
}
impl Drop for Coordinator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            worker.thread().unpark();
            let _ = worker.join();
        }
        // Shutdown must destroy all native queue observations, including when
        // decode returns an error or unwinds. No HTTP or lease token lives here.
        use crate::Flush;
        for slot in &self.slots {
            let mut slot = slot.lock().unwrap_or_else(|poison| poison.into_inner());
            slot.1.flush();
        }
    }
}

#[cfg(test)]
mod gate_tests {
    use super::{CycleGate, GatedClock};
    use conspect_media_crypto::bootstrap::EpochAdmissionClock;
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};
    /// `close` waits for a busy cycle and no cycle starts after it.
    #[test]
    fn close_waits_for_a_busy_cycle_and_blocks_new_ones() {
        let gate = Arc::new(CycleGate::new());
        let (entered, wait_entered) = mpsc::channel();
        let (finish, wait_finish) = mpsc::channel::<()>();
        let worker = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                let cycle = gate.enter().expect("open gate");
                entered.send(()).unwrap();
                wait_finish.recv().unwrap();
                drop(cycle);
            })
        };
        wait_entered.recv().unwrap();
        let closer = {
            let gate = gate.clone();
            std::thread::spawn(move || gate.close())
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!closer.is_finished(), "close returned during a busy cycle");
        assert!(!gate.is_open());
        finish.send(()).unwrap();
        closer.join().unwrap();
        worker.join().unwrap();
        assert!(gate.enter().is_none(), "a cycle started after close");
    }
    /// The 404-retry boundary wait (up to an epoch) is an admission sleep:
    /// `close` returns while the cycle sleeps, and the cycle's next clock
    /// sample fails, so the retry request is never sent.
    #[test]
    fn close_does_not_wait_for_an_admission_sleep_and_no_request_follows() {
        let gates = Arc::new([CycleGate::new(), CycleGate::new()]);
        let clock = Arc::new(GatedClock {
            gate: gates.clone(),
            index: 1,
        });
        let (sleeping, wait_sleeping) = mpsc::channel();
        let worker = {
            let (gates, clock) = (gates.clone(), clock.clone());
            std::thread::spawn(move || {
                let _cycle = gates[1].enter().expect("open gate");
                clock.wall_ms().expect("first sample while open");
                sleeping.send(()).unwrap();
                clock.sleep(Duration::from_secs(3));
                clock.wall_ms()
            })
        };
        wait_sleeping.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        gates[1].close();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "close waited for the admission sleep"
        );
        assert_eq!(worker.join().unwrap(), Err("grant worker stopped"));
        // The other taker's gate is unaffected.
        assert!(gates[0].is_open());
    }
}
