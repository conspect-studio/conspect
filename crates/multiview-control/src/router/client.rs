//! Bounded, off-clock TCP transport for router reports.
use super::{
    swp08::SwP08Message,
    transport::{read_ember, read_swp08, OutboundFrame, RouterLink},
    RouteTable, RouterRoute,
};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use socket2::{SockRef, TcpKeepalive};

/// Enable kernel TCP probes after ten idle seconds, every five seconds, with
/// three unanswered probes before the kernel reports a dead peer. The link can
/// remain apparently open for roughly twenty-five seconds after blackholing;
/// `last_route` remains exposed so a caller can detect a silent route mirror.
fn configure_keepalive(socket: &TcpStream) -> io::Result<()> {
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(5))
        .with_retries(3);
    SockRef::from(socket).set_tcp_keepalive(&keepalive)
}

/// Framing protocol of a TCP link.
#[derive(Clone, Copy)]
pub enum Protocol {
    /// SW-P-08 DLE framing.
    Swp08,
    /// Ember+ S101 framing (codec only; no standardized crosspoint mapping).
    Ember,
}
/// Start refusal for a protocol without a defined route-report mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    /// Ember+ `GlowNode` has no standard source/destination crosspoint schema.
    #[error("Ember+ route reports need a configured parameter mapping")]
    EmberRouteMappingRequired,
}

/// Observable status of the last connection attempt.
#[derive(Clone, Debug, Default)]
pub struct LinkStatus {
    /// Socket currently open.
    pub connected: bool,
    /// Last transport or decoding error.
    pub last_error: Option<String>,
    /// Last accepted route report.
    pub last_route: Option<Instant>,
    /// Total successful TCP connections, including reconnects.
    pub connections: u64,
    /// Number of valid route reports accepted in the current connection.
    pub reports: u64,
}
/// Bounded producer handle; stop and join outside any output-clock path.
pub struct Client {
    tx: SyncSender<OutboundFrame>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<LinkStatus>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Client {
    /// Start a supervised SW-P-08 control-plane connection.
    ///
    /// # Errors
    ///
    /// Ember+ needs a router-specific Glow parameter-to-crosspoint mapping;
    /// the pure S101/BER codec exists, but no live route-report mapping does.
    #[allow(clippy::too_many_lines)] // One socket lifecycle: connect, bounded I/O, classify, reconnect.
    pub fn start(
        address: SocketAddr,
        protocol: Protocol,
        table: Arc<RouteTable>,
    ) -> Result<Self, StartError> {
        if matches!(protocol, Protocol::Ember) {
            return Err(StartError::EmberRouteMappingRequired);
        }
        let (tx, rx) = mpsc::sync_channel::<OutboundFrame>(32);
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(LinkStatus::default()));
        let s = Arc::clone(&stop);
        let st = Arc::clone(&status);
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                let result = TcpStream::connect_timeout(&address, Duration::from_secs(1)).and_then(
                    |mut socket| {
                        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
                        socket.set_write_timeout(Some(Duration::from_millis(100)))?;
                        configure_keepalive(&socket)?;
                        {
                            let mut status =
                                st.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            status.connected = true;
                            status.last_error = None;
                            status.connections = status.connections.saturating_add(1);
                            status.reports = 0;
                        }
                        let mut frame = Vec::with_capacity(4096);
                        let mut swp08_bcc_next = false;
                        let mut swp08_dle = false;
                        let mut buf = [0u8; 512];
                        while !s.load(Ordering::Acquire) {
                            // Drain at most the channel capacity per loop, so an
                            // active sender cannot starve inbound route reports.
                            for _ in 0..32 {
                                let Ok(out) = rx.try_recv() else { break };
                                if out.link != RouterLink::Tcp {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidInput,
                                        "serial frame sent to TCP router",
                                    ));
                                }
                                if out.bytes.len() > 4096 {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "outbound frame too large",
                                    ));
                                }
                                socket.write_all(&out.bytes)?;
                            }
                            match socket.read(&mut buf) {
                                Ok(0) => {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "router disconnected",
                                    ))
                                }
                                Ok(n) => {
                                    for &b in buf.get(..n).unwrap_or(&[]) {
                                        // S101 begins at BOF. Discard stray bytes from a
                                        // half frame or noisy peer without waiting for the
                                        // entire 4096-byte cap before accepting a good frame.
                                        if matches!(protocol, Protocol::Ember)
                                            && frame.is_empty()
                                            && b != 0xfe
                                        {
                                            continue;
                                        }
                                        frame.push(b);
                                        if frame.len() > 4096 {
                                            return Err(io::Error::new(
                                                io::ErrorKind::InvalidData,
                                                "router frame too large",
                                            ));
                                        }
                                        let complete = match protocol {
                                            Protocol::Ember => {
                                                frame.first() == Some(&0xfe) && b == 0xff
                                            }
                                            Protocol::Swp08 => {
                                                // DLE ETX terminates the body, but the NEXT
                                                // byte is the BCC. Never decode before it arrives.
                                                let complete = swp08_bcc_next;
                                                if !complete {
                                                    if b == 0x10 {
                                                        // A doubled DLE is body data. Only an
                                                        // unmatched DLE may introduce ETX.
                                                        swp08_dle = !swp08_dle;
                                                    } else {
                                                        swp08_bcc_next = swp08_dle && b == 0x03;
                                                        swp08_dle = false;
                                                    }
                                                }
                                                complete
                                            }
                                        };
                                        if complete {
                                            match protocol {
                                                Protocol::Swp08 => {
                                                    let message =
                                                        read_swp08(&frame).map_err(|e| {
                                                            io::Error::new(
                                                                io::ErrorKind::InvalidData,
                                                                e,
                                                            )
                                                        })?;
                                                    if matches!(
                                                        message,
                                                        SwP08Message::Connected { .. }
                                                    ) {
                                                        if let Some(route) =
                                                            RouterRoute::from_swp08(&message)
                                                        {
                                                            table.apply(route);
                                                            let mut status = st
                                                                .lock()
                                                                .unwrap_or_else(
                                                                std::sync::PoisonError::into_inner,
                                                            );
                                                            status.last_route =
                                                                Some(Instant::now());
                                                            status.reports =
                                                                status.reports.saturating_add(1);
                                                        }
                                                    }
                                                }
                                                Protocol::Ember => {
                                                    read_ember(&frame).map_err(|e| {
                                                        io::Error::new(
                                                            io::ErrorKind::InvalidData,
                                                            e,
                                                        )
                                                    })?;
                                                }
                                            }
                                            frame.clear();
                                            swp08_bcc_next = false;
                                            swp08_dle = false;
                                        }
                                    }
                                }
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                    ) => {}
                                Err(e) => return Err(e),
                            }
                        }
                        Ok(())
                    },
                );
                let mut status = st.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                status.connected = false;
                if let Err(e) = result {
                    status.last_error = Some(e.to_string());
                }
                drop(status);
                for _ in 0..25 {
                    if s.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        });
        Ok(Self {
            tx,
            stop,
            status,
            thread: Some(thread),
        })
    }
    /// Submit a bounded frame without waiting.
    ///
    /// # Errors
    ///
    /// A serial or oversized frame is rejected, as is a full/disconnected queue.
    pub fn try_send(&self, frame: OutboundFrame) -> Result<(), mpsc::TrySendError<OutboundFrame>> {
        if frame.bytes.len() > 4096 || frame.link != RouterLink::Tcp {
            return Err(mpsc::TrySendError::Full(frame));
        }
        self.tx.try_send(frame)
    }
    /// Snapshot the current connection status off-clock.
    #[must_use]
    pub fn status(&self) -> LinkStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::router::{swp08::SwP08Message, transport::frame_swp08};
    use std::net::TcpListener;

    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(condition(), "router state did not converge");
    }

    #[test]
    fn tcp_keepalive_is_enabled_with_bounded_kernel_probe_interval() {
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let connector =
            thread::spawn(move || TcpStream::connect(listener.local_addr().unwrap()).unwrap());
        let socket = connector.join().unwrap();
        configure_keepalive(&socket).unwrap();
        let options = SockRef::from(&socket);
        assert!(options.keepalive().unwrap(), "SO_KEEPALIVE must be active");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert_eq!(
                options.tcp_keepalive_time().unwrap(),
                Duration::from_secs(10)
            );
            assert_eq!(
                options.tcp_keepalive_interval().unwrap(),
                Duration::from_secs(5)
            );
            assert_eq!(options.tcp_keepalive_retries().unwrap(), 3);
        }
    }

    #[test]
    fn stub_router_reports_route_then_disconnects_and_reconnects() {
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let table = Arc::new(RouteTable::new());
        let client = Client::start(addr, Protocol::Swp08, Arc::clone(&table)).unwrap();
        let stub_table = Arc::clone(&table);
        let stub = thread::spawn(move || {
            for source in [7, 8] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let outbound = frame_swp08(
                    RouterLink::Tcp,
                    &SwP08Message::Interrogate {
                        matrix: 0,
                        level: 1,
                        destination: 42,
                    },
                )
                .unwrap();
                let mut request = vec![0; outbound.bytes.len()];
                socket.read_exact(&mut request).unwrap();
                assert_eq!(request, outbound.bytes);
                let report = frame_swp08(
                    RouterLink::Tcp,
                    &SwP08Message::Connected {
                        matrix: 0,
                        level: 1,
                        destination: 42,
                        source,
                    },
                )
                .unwrap();
                // Deliberately split the DLE ETX/BCC boundary and coalesce
                // successive chunks: the reader must not decode before BCC.
                let split = report.bytes.len() - 1;
                socket.write_all(&report.bytes[..split]).unwrap();
                thread::sleep(Duration::from_millis(10));
                socket.write_all(&report.bytes[split..]).unwrap();
                until(|| stub_table.source_of(1, 42) == Some(source));
                // Dropping the socket is a real EOF, not a test-internal event.
            }
        });
        until(|| client.status().connected);
        let query = frame_swp08(
            RouterLink::Tcp,
            &SwP08Message::Interrogate {
                matrix: 0,
                level: 1,
                destination: 42,
            },
        )
        .unwrap();
        client.try_send(query.clone()).unwrap();
        until(|| table.source_of(1, 42) == Some(7));
        until(|| !client.status().connected);
        let disconnected = client.status();
        assert!(disconnected
            .last_error
            .as_ref()
            .unwrap()
            .contains("disconnected"));
        until(|| client.status().connections >= 2);
        client.try_send(query).unwrap();
        until(|| table.source_of(1, 42) == Some(8) && client.status().reports == 1);
        let status = client.status();
        // The stub closes its second socket after the report. A disconnected
        // snapshot is correct if that EOF wins the race; the report and the
        // successful second connection prove the reconnect path completed.
        assert!(status.connections >= 2);
        assert!(status.last_route.is_some());
        assert_eq!(status.reports, 1);
        stub.join().unwrap();
    }

    #[test]
    fn stuffed_dle_then_etx_inside_body_does_not_end_the_frame() {
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let table = Arc::new(RouteTable::new());
        let client = Client::start(
            listener.local_addr().unwrap(),
            Protocol::Swp08,
            Arc::clone(&table),
        )
        .unwrap();
        let (mut socket, _) = listener.accept().unwrap();
        // Destination low byte DLE, then source high byte ETX. Byte stuffing
        // yields DLE DLE ETX *inside* the body; a suffix-only detector mistakes
        // this for the real DLE ETX marker and reads the next byte as a BCC.
        let report = frame_swp08(
            RouterLink::Tcp,
            &SwP08Message::Connected {
                matrix: 0,
                level: 1,
                destination: 0x10,
                source: 0x300,
            },
        )
        .unwrap();
        assert!(report.bytes.windows(3).any(|w| w == [0x10, 0x10, 0x03]));
        socket.write_all(&report.bytes).unwrap();
        until(|| table.source_of(1, 0x10) == Some(0x300) && client.status().reports == 1);
        assert!(client.status().connected);
        assert_eq!(client.status().reports, 1);
    }

    #[test]
    fn ember_without_route_mapping_is_rejected_before_opening_a_socket() {
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let result = Client::start(
            listener.local_addr().unwrap(),
            Protocol::Ember,
            Arc::new(RouteTable::new()),
        );
        assert!(matches!(result, Err(StartError::EmberRouteMappingRequired)));
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(listener.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    }

    #[test]
    fn outbound_queue_is_bounded_and_wrong_link_is_rejected() {
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let client = Client::start(
            listener.local_addr().unwrap(),
            Protocol::Swp08,
            Arc::new(RouteTable::new()),
        )
        .unwrap();
        assert!(client
            .try_send(OutboundFrame {
                link: RouterLink::Serial,
                bytes: vec![1]
            })
            .is_err());
        assert!(client
            .try_send(OutboundFrame {
                link: RouterLink::Tcp,
                bytes: vec![0; 4097]
            })
            .is_err());
        // A malicious sender cannot grow the bounded queue without limit.
        let mut refused = false;
        for _ in 0..1024 {
            if client
                .try_send(OutboundFrame {
                    link: RouterLink::Tcp,
                    bytes: vec![1],
                })
                .is_err()
            {
                refused = true;
                break;
            }
        }
        assert!(refused);
    }
}
