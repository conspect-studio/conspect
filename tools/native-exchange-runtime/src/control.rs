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

/// A deny is sticky for the whole run. The watchdog does not wait for HTTP or decode.
pub struct Coordinator {
    stop: Arc<AtomicBool>,
    denied: Arc<AtomicBool>,
    active: Arc<[AtomicBool; 2]>,
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
            let (stop, denied, active, transport, booking, slot) = (
                stop.clone(),
                denied.clone(),
                active.clone(),
                transport.clone(),
                booking.clone(),
                slots[i].clone(),
            );
            workers.push(thread::spawn(move || {
                let mut due = Instant::now();
                let mut attempt_epoch = u64::MAX;
                let mut attempts = 0;
                while !stop.load(Ordering::Acquire) && !denied.load(Ordering::Acquire) {
                    if Instant::now() >= due && active[i].load(Ordering::Acquire) {
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
                                    active[i].store(false, Ordering::Release);
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
                            Err(_) => {
                                let mut slot = slot.lock().unwrap();
                                let (receiver, output) = &mut *slot;
                                let _ = receiver.deny(output);
                                active[i].store(false, Ordering::Release);
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
            workers,
            slots: shutdown_slots,
        }
    }
    pub fn deactivate(&self, i: usize) {
        self.active[i].store(false, Ordering::Release);
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
