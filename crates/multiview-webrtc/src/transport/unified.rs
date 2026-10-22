//! The single-socket [`UnifiedEndpoint`] (ADR-0048 §4) — **feature `native`**.
//!
//! ADR-0048 mandates **one** process-wide dual-stack UDP socket adopted by **all**
//! WebRTC roles: WHIP ingest publishers, WHEP preview viewers, WHEP output
//! viewers, and the outbound `whip_push` client. Before this, the cli bound the
//! `webrtc.udp_port` once per role (preview + WHIP + WHEP-serve + each `whip_push`),
//! so the 2nd/3rd `bind` hit `EADDRINUSE` and silently degraded those roles to
//! "unavailable" — with preview + WHIP + WHEP-serve in one config, ingest and
//! output-serve were dead (box-validation defect B).
//!
//! [`UnifiedEndpoint`] fixes that: it binds the **one** socket, owns the **one**
//! [`TurnRelayDriver`] (so every role shares the relay allocations), and runs a
//! **single** driver task that demultiplexes every inbound datagram to the right
//! role's session by str0m's ufrag/peer demux (`Rtc::accepts()`) — exactly the
//! sans-IO single-socket pattern. Outbound datagrams are routed relay-aware (a
//! str0m `Transmit` whose source is an allocated relay is framed as a TURN Send
//! indication — defect C). No `SO_REUSEPORT`, one socket, one driver, many
//! sessions.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! The driver never `.await`s a peer (UDP send is non-blocking); per-role media
//! crosses only bounded drop-oldest rings (ingest `RtpRing`, egress `SampleFeed` /
//! `EgressFeed`). A wedged or saturated endpoint loses *preview/ingest/viewer
//! media*, never an output tick. Registration crosses bounded command channels.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::EndpointConfig;
use crate::egress::EgressFeed;
use crate::error::{Result, WebRtcError};
use crate::transport::whep_serve::{ServeLane, WhepServeHandle};
use crate::transport::whip_endpoint::{IngestLane, WhipEndpoint, WhipHandle};
use crate::transport::whip_push::{PushLane, WhipPushSpec};
use crate::transport::{WebRtcEndpoint, WhepServeEndpoint};
use crate::turn::TurnRelayDriver;

/// The maximum UDP datagram the unified recv loop reads.
const RECV_BUFFER: usize = 2048;

/// How often the driver wakes to advance timers, drain feeds, GC, and pump TURN
/// when otherwise idle. The tightest role cadence (WHEP egress) wins so program
/// AUs are packetized promptly.
const DRIVER_TICK: Duration = Duration::from_millis(10);

/// Per-wake inbound drain budget: the **maximum datagrams** one `readable` wake
/// reads from the socket before it MUST break to pump str0m timers / TURN
/// keepalives / commands / GC and yield back to `select!` (invariant #10). At the
/// ADR-0048 §12 worst case (~50 kpps aggregate), 256 datagrams is ~5 ms of
/// arrivals — bounded work — while the `select!` re-fires immediately if the
/// socket still has data, so no datagram is *lost* by the cap (only deferred to
/// the next wake; the OS receive buffer holds the rest, dropping oldest on
/// overflow exactly as UDP already does). Without this cap a sustained/hostile
/// flood (the socket never returns `WouldBlock`) would keep the task in the inner
/// loop forever and starve the whole driver — the panel's MAJOR #1.
const MAX_DATAGRAMS_PER_WAKE: usize = 256;

/// Per-wake inbound drain **time budget**: even below [`MAX_DATAGRAMS_PER_WAKE`],
/// once this much wall-clock has elapsed inside one drain the loop breaks to
/// pump/yield. A belt-and-braces bound for the case where per-datagram routing is
/// unexpectedly costly, so a wake can never monopolise the task for longer than
/// this regardless of datagram size/rate. Well under [`DRIVER_TICK`] so pumping
/// and the tick arm still run promptly.
const MAX_DRAIN_TIME: Duration = Duration::from_millis(2);

/// How a single-wake inbound drain ended (invariant #10, MAJOR #1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DrainOutcome {
    /// Datagrams read this wake (routed + dropped-malformed), capped by the budget.
    read_count: usize,
    /// `true` when the per-wake budget (count or time) stopped the drain before
    /// the socket reported `WouldBlock` — the socket may still hold data, so the
    /// caller must re-arm the `select!` (after pumping/yielding).
    budget_exhausted: bool,
}

/// Diagnostic counters for the driver loop's invariant-#10 handoff, recorded
/// unconditionally (two relaxed adds, only when each event fires) so a soak test
/// can assert the **ordering** guarantee the round-2 BLOCKER turned on: every
/// exhausted receive budget is followed by a deterministic servicing pump
/// **before** the next datagram is read. Production calls
/// [`UnifiedEndpoint::run`], which threads a throwaway probe; only tests inspect
/// the counts.
#[derive(Debug, Default)]
#[doc(hidden)]
struct RunProbe {
    /// Times an inbound drain stopped on its per-wake budget (still-readable
    /// socket) rather than `WouldBlock`.
    recv_budgets_exhausted: std::sync::atomic::AtomicU64,
    /// Times the driver ran a full servicing pump (commands, timers, outbound,
    /// GC) — once per idle tick, and once per exhausted receive budget as the
    /// bounded handoff.
    driver_services: std::sync::atomic::AtomicU64,
}

/// The single-socket WebRTC endpoint: ONE bound dual-stack UDP socket and ONE
/// TURN relay driver, shared by every WebRTC role (ADR-0048 §4).
///
/// Build with [`UnifiedEndpoint::bind`], register the roles your config needs
/// (preview egress, WHIP ingest, WHEP-serve outputs, `whip_push` outputs) via the
/// returned [`UnifiedBuilder`], then run the one driver task with
/// [`UnifiedEndpoint::run`].
pub struct UnifiedEndpoint {
    endpoint: WebRtcEndpoint,
    /// The single TURN relay driver shared by every role (one allocation set).
    config: EndpointConfig,
    /// The concrete host candidates str0m gathered (IPv6-first), shared by every
    /// role. Inbound datagrams' local destination is resolved onto one of these so
    /// str0m's STUN matching accepts them (box-validation defect #3).
    host_candidates: Vec<SocketAddr>,
    /// The WHIP ingest lane (registered when the config has `webrtc` sources).
    ingest: Option<IngestLane>,
    /// The WHEP output-serve lane (registered when the config has `webrtc` outputs).
    serve: Option<ServeLane>,
    /// The native WHEP preview egress (registered when preview is wired).
    preview: Option<Arc<crate::whep_egress::WhepEgress>>,
    /// The `whip_push` client lanes (one per configured `whip_push` output).
    push: Vec<PushLane>,
}

impl std::fmt::Debug for UnifiedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedEndpoint")
            .field("endpoint", &self.endpoint)
            .field("has_ingest", &self.ingest.is_some())
            .field("has_serve", &self.serve.is_some())
            .field("has_preview", &self.preview.is_some())
            .field("push_lanes", &self.push.len())
            .finish_non_exhaustive()
    }
}

/// A builder that wires the WebRTC roles onto the one shared socket before the
/// driver task is spawned. Each `with_*` call returns the role's negotiation
/// handle (or the shared preview egress) so the cli can connect it to the control
/// plane / pipeline, exactly as the per-role endpoints did — but all on one socket.
pub struct UnifiedBuilder {
    endpoint: UnifiedEndpoint,
    host_candidates: Vec<SocketAddr>,
}

impl std::fmt::Debug for UnifiedBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedBuilder")
            .field("host_candidates", &self.host_candidates)
            .finish_non_exhaustive()
    }
}

impl UnifiedEndpoint {
    /// Bind the single dual-stack socket and start a [`UnifiedBuilder`].
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] / [`WebRtcError::Config`] if the bind fails.
    pub fn bind(config: EndpointConfig) -> Result<UnifiedBuilder> {
        let endpoint = WebRtcEndpoint::bind(config.clone())?;
        let host_candidates = endpoint.host_candidates()?;
        Ok(UnifiedBuilder {
            endpoint: Self {
                endpoint,
                config,
                host_candidates: host_candidates.clone(),
                ingest: None,
                serve: None,
                preview: None,
                push: Vec::new(),
            },
            host_candidates,
        })
    }

    /// The gathered host candidate addresses (IPv6-first), shared by every role.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn host_candidates(&self) -> Result<Vec<SocketAddr>> {
        self.endpoint.host_candidates()
    }
}

impl UnifiedBuilder {
    /// The gathered host candidate addresses (IPv6-first), shared by every role.
    #[must_use]
    pub fn host_candidates(&self) -> &[SocketAddr] {
        &self.host_candidates
    }

    /// The local address the single shared socket is bound to (the relay
    /// candidate's `raddr` base; the bound host candidate the preview egress uses).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.endpoint.local_addr()
    }

    /// Register the WHIP **ingest** lane on the shared socket, returning the
    /// [`WhipHandle`] the control plane negotiates publishers against.
    #[must_use]
    pub fn with_ingest(mut self) -> (Self, WhipHandle) {
        let (handle, commands, shared) =
            WhipHandle::build(&self.endpoint.config, self.host_candidates.clone());
        self.endpoint.ingest = Some(IngestLane {
            commands,
            shared,
            sessions: Vec::new(),
        });
        (self, handle)
    }

    /// Register the WHEP **output-serve** lane on the shared socket, returning the
    /// [`WhepServeHandle`] the control plane negotiates viewers against.
    #[must_use]
    pub fn with_serve(mut self) -> (Self, WhepServeHandle) {
        let (handle, commands, shared) =
            WhepServeHandle::build(&self.endpoint.config, self.host_candidates.clone());
        self.endpoint.serve = Some(ServeLane {
            commands,
            shared,
            viewers: Vec::new(),
        });
        (self, handle)
    }

    /// Register the native **WHEP preview egress** on the shared socket. The
    /// returned [`WhepEgress`](crate::whep_egress::WhepEgress) is the transport the
    /// cli's `CliWhepProvider` negotiates preview viewers against; the driver pumps
    /// its egress + fans inbound datagrams to it.
    #[must_use]
    pub fn with_preview(mut self, preview: Arc<crate::whep_egress::WhepEgress>) -> Self {
        self.endpoint.preview = Some(preview);
        self
    }

    /// Register one `whip_push` **output** on the shared socket: the program egress
    /// `feed` it publishes, whether it carries audio, and the HTTP `signaller` that
    /// `POST`s the offer to the remote WHIP origin.
    #[must_use]
    pub fn with_push(
        mut self,
        feed: EgressFeed,
        audio: bool,
        signaller: Box<dyn crate::transport::whip_push::WhipSignaller>,
    ) -> Self {
        self.endpoint.push.push(PushLane::new(
            WhipPushSpec {
                feed,
                audio,
                host_candidates: self.host_candidates.clone(),
            },
            signaller,
        ));
        self
    }

    /// Finish wiring and return the [`UnifiedEndpoint`] ready to [`run`](UnifiedEndpoint::run).
    #[must_use]
    pub fn build(self) -> UnifiedEndpoint {
        self.endpoint
    }
}

impl UnifiedEndpoint {
    /// Run the single driver loop until `stop` is raised: ONE socket, ONE TURN
    /// driver, every role's sessions demuxed by str0m on each inbound datagram and
    /// pumped on each tick (ADR-0048 §4/§7). The live socket loop is hardware-gated
    /// for full media flow; the structure (demux, relay framing, isolation) is
    /// proven offline.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    pub async fn run(self, stop: Arc<AtomicBool>) -> Result<()> {
        self.run_inner(stop, &RunProbe::default()).await
    }

    /// The driver loop, threading a [`RunProbe`] so a soak test can assert the
    /// invariant-#10 ordering guarantee (servicing keeps pace with receive
    /// budgets). [`run`](Self::run) calls this with a throwaway probe.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    #[allow(
        clippy::too_many_lines,
        reason = "one select! loop owning the single socket per ADR-0048 §4 — \
                  splitting the arms would re-introduce per-role drivers"
    )]
    async fn run_inner(self, stop: Arc<AtomicBool>, probe: &RunProbe) -> Result<()> {
        let bind_addr = self.endpoint.config().bind_addr();
        let local_addr = self.endpoint.local_addr()?;
        // The concrete candidates an inbound datagram's local destination may be
        // resolved onto — the gathered host candidates with the unspecified `[::]`
        // bind addr removed (str0m never gathers it and discards STUN to it).
        let concrete_candidates: Vec<SocketAddr> = self
            .host_candidates
            .iter()
            .copied()
            .filter(|a| !a.ip().is_unspecified())
            .collect();
        let mut turn = TurnRelayDriver::from_config(&self.config, Instant::now());

        let std_socket = self.endpoint.into_socket();
        std_socket
            .set_nonblocking(true)
            .map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        let socket =
            tokio::net::UdpSocket::from_std(std_socket).map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        // Ask the kernel to report each datagram's concrete destination IP via
        // `IPV6_PKTINFO` / `IP_PKTINFO` so the unspecified-bound socket can still
        // tell str0m the local candidate the packet arrived on (defect #3). A
        // failure here is fatal: without PKTINFO every inbound STUN would be
        // discarded as "unknown interface" and ICE could never complete.
        crate::transport::local_addr::enable_pktinfo(&socket2::SockRef::from(&socket)).map_err(
            |source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            },
        )?;

        let mut ingest = self.ingest;
        let mut serve = self.serve;
        let preview = self.preview;
        let mut push = self.push;

        let mut buf = vec![0u8; RECV_BUFFER];
        let mut tick = tokio::time::interval(DRIVER_TICK);

        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }

            // `biased`: poll the tick arm before the receive arm so a permanently
            // ready socket can never crowd the idle tick out of the schedule. The
            // receive arm additionally performs an explicit bounded handoff after
            // an exhausted budget (below), so servicing does not depend on this
            // bias alone — but biasing keeps the steady-state tick cadence tight
            // even mid-flood (invariant #10).
            tokio::select! {
                biased;
                _ = tick.tick() => {
                    let now = Instant::now();
                    Self::service_timers_and_gc(
                        &socket, &mut turn,
                        &mut ingest, &mut serve, preview.as_ref(), &mut push,
                        local_addr, now, probe,
                    ).await;
                }
                ready = socket.readable() => {
                    let now = Instant::now();
                    let outcome = if ready.is_ok() {
                        // Drain at most one wake's BUDGET of datagrams (count +
                        // time), then break to hand off — never an unbounded drain
                        // a sustained/hostile flood could ride forever and starve
                        // the rest of the loop (invariant #10, MAJOR #1). Each read
                        // recovers the CONCRETE local destination via PKTINFO and
                        // resolves it onto a gathered candidate so str0m's STUN
                        // matching accepts the check (defect #3).
                        Self::drain_inbound_budgeted(
                            &socket, &mut buf, local_addr, &concrete_candidates,
                            |src, dst, payload| {
                                Self::route_inbound(
                                    &mut turn,
                                    ingest.as_mut(),
                                    serve.as_mut(),
                                    preview.as_ref(),
                                    &mut push,
                                    src,
                                    dst,
                                    payload,
                                    now,
                                );
                            },
                        )
                    } else {
                        DrainOutcome { read_count: 0, budget_exhausted: false }
                    };

                    if outcome.budget_exhausted {
                        // The budget cut the drain short while the socket still
                        // holds data, so the `readable()` arm would re-fire at
                        // once. Perform an explicit BOUNDED HANDOFF before that can
                        // happen: yield to the runtime, then run ONE full servicing
                        // pump (commands + timers + outbound + GC). This makes the
                        // rest of the driver run GUARANTEED between consecutive
                        // receive budgets — it does not rely on `select!` happening
                        // to pick the tick arm (invariant #10, round-2 BLOCKER).
                        probe
                            .recv_budgets_exhausted
                            .fetch_add(1, Ordering::Relaxed);
                        tokio::task::yield_now().await;
                        Self::service_timers_and_gc(
                            &socket, &mut turn,
                            &mut ingest, &mut serve, preview.as_ref(), &mut push,
                            local_addr, now, probe,
                        ).await;
                    } else {
                        // The socket drained within budget: just pump outbound
                        // (the full tick servicing runs on the idle tick arm). No
                        // re-arm is needed — tokio cleared readiness.
                        Self::pump_all(
                            &socket, &mut turn,
                            ingest.as_mut(), serve.as_mut(), preview.as_ref(), &mut push,
                            local_addr, now,
                        ).await;
                    }
                }
            }
        }
    }

    /// One full servicing pass: drain each lane's register/release commands,
    /// advance every role's ICE/DTLS timers + feeds, pump outbound onto the one
    /// socket, then GC dead sessions. Run once per idle tick **and** once per
    /// exhausted receive budget (the invariant-#10 bounded handoff), so a
    /// permanent inbound flood can never defer command/timer/GC servicing past
    /// one receive budget.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single servicing pass drives every role on one socket"
    )]
    async fn service_timers_and_gc(
        socket: &tokio::net::UdpSocket,
        turn: &mut TurnRelayDriver,
        ingest: &mut Option<IngestLane>,
        serve: &mut Option<ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        local_addr: SocketAddr,
        now: Instant,
        probe: &RunProbe,
    ) {
        probe.driver_services.fetch_add(1, Ordering::Relaxed);
        // Drain pending register/release commands for each lane.
        Self::drain_commands(ingest.as_mut(), serve.as_mut());
        // Advance each role's timers / feeds.
        if let Some(lane) = ingest.as_mut() {
            WhipEndpoint::tick(&mut lane.sessions, now);
        }
        if let Some(lane) = serve.as_mut() {
            WhepServeEndpoint::tick(&mut lane.viewers, &lane.shared, now);
        }
        Self::pump_all(
            socket,
            turn,
            ingest.as_mut(),
            serve.as_mut(),
            preview,
            push,
            local_addr,
            now,
        )
        .await;
        // GC dead sessions per lane.
        if let Some(lane) = ingest.as_mut() {
            WhipEndpoint::reap(&mut lane.sessions, &lane.shared);
        }
        if let Some(lane) = serve.as_mut() {
            WhepServeEndpoint::reap(&mut lane.viewers, &lane.shared);
        }
    }

    /// Drain (non-blocking) every pending register/release command for the lanes,
    /// so a registration is admitted promptly without an awaited per-lane arm.
    fn drain_commands(ingest: Option<&mut IngestLane>, serve: Option<&mut ServeLane>) {
        if let Some(lane) = ingest {
            while let Ok(cmd) = lane.commands.try_recv() {
                WhipEndpoint::apply_command(&mut lane.sessions, Some(cmd));
            }
        }
        if let Some(lane) = serve {
            while let Ok(cmd) = lane.commands.try_recv() {
                WhepServeEndpoint::apply_command(&mut lane.viewers, Some(cmd));
            }
        }
    }

    /// Drain the readable socket for **at most one wake's budget** of datagrams,
    /// invoking `route` for each, then return how the drain ended (invariant #10,
    /// MAJOR #1).
    ///
    /// The drain stops at the **first** of: the socket draining (`WouldBlock`),
    /// the [`MAX_DATAGRAMS_PER_WAKE`] count cap, or the [`MAX_DRAIN_TIME`] time
    /// budget. A per-datagram receive error other than `WouldBlock` (a malformed
    /// datagram, or `MSG_CTRUNC` from [`recv_from_with_local`]) **drops that one
    /// datagram and continues** — it still counts against the budget, so a hostile
    /// peer spraying malformed datagrams cannot get unbounded reads, and one bad
    /// datagram never halts the drain of good ones.
    ///
    /// [`DrainOutcome::budget_exhausted`] tells the caller the socket may still
    /// have data. On exhaustion the driver performs an explicit bounded handoff —
    /// [`yield_now`](tokio::task::yield_now) then exactly one
    /// [`service_timers_and_gc`](Self::service_timers_and_gc) pass — **before**
    /// the `readable` arm can re-fire, so commands/timers/outbound/GC are
    /// serviced once per exhausted budget regardless of `select!` arm ordering,
    /// and the loop's top-of-iteration stop check is reached within a bounded
    /// number of iterations. The flood gets bounded work per wake and cannot
    /// defer the rest of the driver past one receive budget.
    ///
    /// [`recv_from_with_local`]: crate::transport::local_addr::recv_from_with_local
    fn drain_inbound_budgeted(
        socket: &tokio::net::UdpSocket,
        buf: &mut [u8],
        local_addr: SocketAddr,
        concrete_candidates: &[SocketAddr],
        mut route: impl FnMut(SocketAddr, SocketAddr, &[u8]),
    ) -> DrainOutcome {
        let started = Instant::now();
        let mut read_count = 0usize;
        loop {
            if read_count >= MAX_DATAGRAMS_PER_WAKE || started.elapsed() >= MAX_DRAIN_TIME {
                // Budget hit while the socket may still hold data: break to
                // pump/yield; the caller re-arms the `select!` (defect #1 fix).
                return DrainOutcome {
                    read_count,
                    budget_exhausted: true,
                };
            }
            let read = socket.try_io(tokio::io::Interest::READABLE, || {
                crate::transport::local_addr::recv_from_with_local(
                    &socket2::SockRef::from(socket),
                    buf,
                    local_addr,
                )
            });
            match read {
                Ok((len, src, arrival)) => {
                    read_count += 1;
                    let dst =
                        crate::transport::resolve_local_destination(arrival, concrete_candidates);
                    if let Some(payload) = buf.get(..len) {
                        route(src, dst, payload);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Socket drained: tokio cleared readiness, so the next
                    // `select!` `readable()` genuinely waits. No re-arm needed.
                    return DrainOutcome {
                        read_count,
                        budget_exhausted: false,
                    };
                }
                Err(_) => {
                    // A malformed/truncated datagram (e.g. MSG_CTRUNC): drop just
                    // this one, count it against the budget, keep draining the
                    // rest. A hostile peer cannot halt the drain or escape the cap.
                    read_count += 1;
                }
            }
        }
    }

    /// Route one inbound datagram to the role whose session accepts it. The relay
    /// classification (TURN control / relayed Data / media) happens once; relayed
    /// media is fanned to every role exactly like direct media (str0m's per-session
    /// ufrag/peer demux silently ignores a datagram not addressed to a session).
    #[allow(
        clippy::too_many_arguments,
        reason = "the single demux fans one datagram to every role lane"
    )]
    fn route_inbound(
        turn: &mut TurnRelayDriver,
        ingest: Option<&mut IngestLane>,
        serve: Option<&mut ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        src: SocketAddr,
        local_dst: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) {
        // Classify once: a relayed Data indication decapsulates to media arriving
        // on the RELAY candidate addr (so str0m's relay candidate pair validates);
        // a TURN control reply feeds the relay driver; anything else is media from
        // `src` arriving on `local_dst` — the CONCRETE local candidate the caller
        // resolved from the datagram's PKTINFO destination, never the unspecified
        // `[::]` bind (defect #3 / defect C).
        let (route_src, route_dst, bytes): (SocketAddr, SocketAddr, Vec<u8>) =
            match crate::transport::relay_io::classify_inbound(turn, src, payload, now) {
                crate::transport::relay_io::Inbound::Relayed {
                    peer,
                    relay,
                    payload,
                } => (peer, relay, payload),
                crate::transport::relay_io::Inbound::TurnControl => return,
                crate::transport::relay_io::Inbound::Media => (src, local_dst, payload.to_vec()),
            };

        // Fan the (decapsulated) datagram to every registered role; str0m's demux
        // means only the owning session consumes it.
        if let Some(lane) = ingest {
            WhipEndpoint::route_datagram(&mut lane.sessions, route_src, route_dst, &bytes, now);
        }
        if let Some(lane) = serve {
            WhepServeEndpoint::route_datagram(&mut lane.viewers, route_src, route_dst, &bytes, now);
        }
        if let Some(preview) = preview {
            let _ = preview.handle_datagram_broadcast(route_src, route_dst, &bytes, now);
        }
        for lane in push.iter_mut() {
            lane.handle_inbound(route_src, route_dst, &bytes, now);
        }
    }

    /// Pump every role's outbound + the shared TURN driver onto the one socket.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single loop drives every role on one socket"
    )]
    async fn pump_all(
        socket: &tokio::net::UdpSocket,
        turn: &mut TurnRelayDriver,
        ingest: Option<&mut IngestLane>,
        serve: Option<&mut ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        local_addr: SocketAddr,
        now: Instant,
    ) {
        // Drive the shared TURN driver first so a fresh relay is published before
        // the roles gather candidates for the next negotiation.
        while let Some((destination, payload)) = turn.poll_transmit(now) {
            let _ = socket.send_to(&payload, destination).await;
        }
        let new_relays = turn.take_new_relays();
        if !new_relays.is_empty() {
            Self::publish_relays(
                &new_relays,
                ingest.as_deref(),
                serve.as_deref(),
                preview,
                local_addr,
            );
        }

        if let Some(lane) = ingest {
            WhipEndpoint::pump_outbound(socket, &mut lane.sessions, turn, now).await;
        }
        if let Some(lane) = serve {
            WhepServeEndpoint::pump_outbound(socket, &mut lane.viewers, turn, now).await;
        }
        if let Some(preview) = preview {
            if let Ok(out) = preview.drive_all(now) {
                for (source, dst, payload) in out {
                    crate::transport::relay_io::send_routed(
                        socket, turn, source, dst, &payload, now,
                    )
                    .await;
                }
            }
        }
        for lane in push.iter_mut() {
            lane.step(socket, turn, local_addr, now).await;
        }
    }

    /// Publish freshly-learned relays to each role's relay-candidate sink so the
    /// next negotiation offers them (WHIP/WHEP-serve via `learned_relays`, preview
    /// via `WhepEgress::learn_relay`).
    fn publish_relays(
        relays: &[SocketAddr],
        ingest: Option<&IngestLane>,
        serve: Option<&ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        local_addr: SocketAddr,
    ) {
        if let Some(lane) = ingest {
            lane.shared.push_relays(relays);
        }
        if let Some(lane) = serve {
            lane.shared.push_relays(relays);
        }
        if let Some(preview) = preview {
            for relay in relays {
                preview.learn_relay(*relay, local_addr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv6Addr, SocketAddrV6, UdpSocket};

    /// Invariant #10 / round-2 BLOCKER: under a **permanent** inbound flood the
    /// driver's servicing pass (commands + timers + outbound + GC) must keep pace
    /// with the receive budget — every exhausted budget is followed by a
    /// deterministic servicing pass **before** the next datagram is read, not by
    /// probabilistic `select!` fairness.
    ///
    /// The discriminating assertion is an **ordering** one, not a timing one
    /// (timing cannot tell a starved loop from a lucky scheduler): after the
    /// driver has run under a saturating flood, the number of servicing passes
    /// must be at least the number of exhausted receive budgets. The round-1 loop
    /// (drain as a bare statement, unbiased `select!`, no `yield_now`) serviced
    /// only when the tick arm happened to win, so `driver_services` lagged far
    /// behind `recv_budgets_exhausted`; the bounded-handoff fix services once per
    /// exhausted budget, so it keeps pace.
    ///
    /// The flood precondition is driven to a **fixed count**, not a wall-clock
    /// window: a real OS-thread flooder keeps the socket permanently readable, so
    /// the driver will exhaust budgets given enough scheduler time — but *how
    /// many* it exhausts in a fixed 400ms window depends on how much CPU the
    /// flooder and driver tasks actually got, which on a contended/starved CI
    /// runner can be ~none (the flooder thread never runs -> zero budgets
    /// exhausted -> the precondition spuriously fails even though nothing
    /// regressed). So instead of sleeping a fixed window and *hoping* the flood
    /// saturated, the test polls [`RunProbe::recv_budgets_exhausted`] and stops
    /// the driver only once it has genuinely exhausted `TARGET_EXHAUSTIONS`
    /// budgets — guaranteed regardless of CPU, so the saturation precondition is
    /// deterministic. A generous [`tokio::time::timeout`] still bounds the whole
    /// run, so a regression that wedges the loop (and so can never reach the
    /// target) fails deterministically rather than hanging.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn servicing_keeps_pace_with_the_receive_budget_under_flood() {
        use std::sync::atomic::AtomicBool;

        // How many receive budgets the flood must drive to exhaustion before we
        // stop the driver. Above 1 so the test exercises a *sustained* saturating
        // flood (many consecutive budget->handoff cycles), not a single lucky
        // trip; still tiny, so even a heavily CPU-starved runner reaches it
        // quickly once the flooder gets any time slice at all.
        const TARGET_EXHAUSTIONS: u64 = 8;

        let cfg = crate::config::EndpointConfig {
            udp_port: 0,
            advertised_addresses: vec!["::1".parse().expect("v6 literal")],
            ..crate::config::EndpointConfig::default()
        };
        let builder = UnifiedEndpoint::bind(cfg).expect("the single shared socket binds once");
        let bound = builder.local_addr().expect("a bound local addr");
        let (builder, _whip) = builder.with_ingest();
        let endpoint = builder.build();

        let probe = Arc::new(RunProbe::default());
        let stop = Arc::new(AtomicBool::new(false));

        // Saturating flood from a dedicated OS thread (not a tokio worker) so the
        // driver's `socket.readable()` is effectively always ready.
        let flood_stop = Arc::new(AtomicBool::new(false));
        let flooder = {
            let flood_stop = Arc::clone(&flood_stop);
            std::thread::spawn(move || {
                let s = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::LOCALHOST,
                    0,
                    0,
                    0,
                )))
                .expect("flooder binds a v6 loopback");
                let target =
                    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, bound.port(), 0, 0));
                let junk = [0u8; 64];
                while !flood_stop.load(Ordering::Relaxed) {
                    for _ in 0..1024 {
                        let _ = s.send_to(&junk, target);
                    }
                }
            })
        };

        // Run the driver under the flood.
        let driver = {
            let probe = Arc::clone(&probe);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move { endpoint.run_inner(stop, &probe).await })
        };

        // Drive the flood to a DETERMINISTIC number of exhausted budgets rather
        // than for a fixed wall-clock window: poll the counter until the flood has
        // genuinely saturated `TARGET_EXHAUSTIONS` budgets, so the precondition
        // holds regardless of how little CPU a contended runner gave the flooder.
        // The outer timeout only bounds a wedged regression that can never reach
        // the target (it would otherwise spin here forever).
        let saturated = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if probe.recv_budgets_exhausted.load(Ordering::Relaxed) >= TARGET_EXHAUSTIONS {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;

        stop.store(true, Ordering::Release);
        flood_stop.store(true, Ordering::Relaxed);
        let stopped = tokio::time::timeout(Duration::from_secs(5), driver).await;
        let _ = flooder.join();

        let exhausted = probe.recv_budgets_exhausted.load(Ordering::Relaxed);
        let services = probe.driver_services.load(Ordering::Relaxed);

        // The flood must actually have saturated the receive budget (else the test
        // proves nothing). Driving to a fixed exhaustion count makes this
        // deterministic — a timeout here means the driver could not reach
        // `TARGET_EXHAUSTIONS` even in 30s, i.e. it wedged or the flood never
        // landed (a real regression), not merely a slow runner.
        assert!(
            saturated.is_ok(),
            "the flood must exhaust at least {TARGET_EXHAUSTIONS} receive budgets \
             within the bound (got {exhausted}); the flood never saturated — the \
             driver wedged or inbound never landed"
        );
        assert!(
            exhausted >= TARGET_EXHAUSTIONS,
            "the flood must exhaust at least {TARGET_EXHAUSTIONS} receive budgets \
             (got {exhausted}); the flood was not saturating"
        );
        // The discriminating invariant: servicing keeps pace with the receive
        // budget. Each exhausted budget hands off to exactly one servicing pass
        // (plus the idle-tick passes), so services >= exhausted. A starved loop
        // (round-1) leaves services far below exhausted.
        assert!(
            services >= exhausted,
            "servicing must keep pace with the receive budget under flood: \
             {services} services < {exhausted} exhausted budgets — the rest of \
             the driver was starved (invariant #10 BLOCKER)"
        );
        assert!(
            matches!(stopped, Ok(Ok(Ok(())))),
            "the driver must honour stop within the bound under flood, got {stopped:?}"
        );
    }

    /// Invariant #10 / MAJOR #1: a flood of inbound datagrams CANNOT keep the
    /// driver in the inbound drain indefinitely. One `drain_inbound_budgeted` wake
    /// reads **at most** the budget and reports `budget_exhausted`, so it breaks to
    /// pump/yield instead of looping until the (never-arriving under flood)
    /// `WouldBlock`. Driven against a REAL loopback socket flooded well past the
    /// budget — deterministic, fast, kernel-backed.
    #[tokio::test]
    async fn an_inbound_flood_cannot_starve_the_driver() {
        // A real dual-stack loopback receiver with a large receive buffer so the
        // whole flood is queued (the test asserts the *budget* caps the drain, not
        // the OS buffer).
        let recv = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        recv.set_only_v6(false).unwrap();
        // Big enough for >> MAX_DATAGRAMS_PER_WAKE small datagrams.
        let _ = recv.set_recv_buffer_size(8 * 1024 * 1024);
        recv.set_nonblocking(true).unwrap();
        let bind = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0));
        recv.bind(&bind.into()).unwrap();
        let local_addr = recv.local_addr().unwrap().as_socket().unwrap();
        crate::transport::local_addr::enable_pktinfo(&recv).unwrap();

        let recv = tokio::net::UdpSocket::from_std(recv.into()).unwrap();

        // Flood: send well past the per-wake budget so a naive unbounded drain
        // would read them all in one go (and, under a real sustained flood, never
        // stop).
        let flood = MAX_DATAGRAMS_PER_WAKE * 3;
        let sender = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            0,
            0,
            0,
        )))
        .unwrap();
        let mut sent = 0usize;
        for _ in 0..flood {
            if sender.send_to(b"flood", local_addr).is_ok() {
                sent += 1;
            }
        }
        assert!(
            sent > MAX_DATAGRAMS_PER_WAKE,
            "test precondition: queued {sent} datagrams (> budget {MAX_DATAGRAMS_PER_WAKE})"
        );
        // Let the kernel enqueue them.
        recv.readable().await.unwrap();

        let mut buf = vec![0u8; RECV_BUFFER];
        let concrete = [local_addr];

        // ONE wake. A bounded drain returns promptly; an unbounded one would keep
        // reading every queued datagram (and spin forever on a live flood).
        let mut routed = 0usize;
        let started = Instant::now();
        let outcome = UnifiedEndpoint::drain_inbound_budgeted(
            &recv,
            &mut buf,
            local_addr,
            &concrete,
            |_src, _dst, _payload| {
                routed += 1;
            },
        );
        let elapsed = started.elapsed();

        // The drain stopped at the budget, NOT at the socket draining.
        assert!(
            outcome.budget_exhausted,
            "the flood must trip the per-wake budget (so the driver yields), \
             but the drain ran to WouldBlock: {outcome:?}"
        );
        assert_eq!(
            outcome.read_count, MAX_DATAGRAMS_PER_WAKE,
            "exactly the count budget is read before breaking to pump/yield"
        );
        assert_eq!(
            routed, MAX_DATAGRAMS_PER_WAKE,
            "every budgeted datagram was routed (none dropped beyond the cap)"
        );
        // Bounded work => fast; never the unbounded spin the panel flagged. Very
        // generous ceiling to stay non-flaky on a loaded CI box.
        assert!(
            elapsed < Duration::from_secs(1),
            "one budgeted wake must be bounded/fast, took {elapsed:?}"
        );

        // The deferred datagrams are NOT lost — a second wake drains more,
        // proving the budget defers rather than drops (the OS buffer held them).
        let mut routed2 = 0usize;
        let outcome2 = UnifiedEndpoint::drain_inbound_budgeted(
            &recv,
            &mut buf,
            local_addr,
            &concrete,
            |_src, _dst, _payload| {
                routed2 += 1;
            },
        );
        assert!(
            routed2 > 0 && outcome2.read_count > 0,
            "the next wake drains the datagrams deferred by the budget: {outcome2:?}"
        );
    }

    /// A drain over an EMPTY socket reports the socket drained (`WouldBlock`),
    /// reads nothing, and does NOT claim the budget was exhausted — so the driver
    /// parks on its timer rather than busy-looping (the no-flood steady state).
    #[tokio::test]
    async fn an_empty_socket_drains_without_exhausting_the_budget() {
        let recv = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        recv.set_only_v6(false).unwrap();
        recv.set_nonblocking(true).unwrap();
        let bind = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0));
        recv.bind(&bind.into()).unwrap();
        let local_addr = recv.local_addr().unwrap().as_socket().unwrap();
        crate::transport::local_addr::enable_pktinfo(&recv).unwrap();
        let recv = tokio::net::UdpSocket::from_std(recv.into()).unwrap();

        let mut buf = vec![0u8; RECV_BUFFER];
        let concrete = [local_addr];
        let mut routed = 0usize;
        let outcome = UnifiedEndpoint::drain_inbound_budgeted(
            &recv,
            &mut buf,
            local_addr,
            &concrete,
            |_s, _d, _p| routed += 1,
        );
        assert_eq!(routed, 0, "no datagrams queued => nothing routed");
        assert_eq!(outcome.read_count, 0);
        assert!(
            !outcome.budget_exhausted,
            "an empty socket drains to WouldBlock, it does not exhaust the budget"
        );
    }
}
