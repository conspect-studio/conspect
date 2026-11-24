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

/// Framing protocol of a TCP link.
#[derive(Clone, Copy)]
pub enum Protocol {
    /// SW-P-08 DLE framing.
    Swp08,
    /// Ember+ S101 framing.
    Ember,
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
    /// Start a supervised control-plane connection.
    pub fn start(address: SocketAddr, protocol: Protocol, table: Arc<RouteTable>) -> Self {
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
                                    for &b in &buf[..n] {
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
                                                swp08_bcc_next = !complete
                                                    && frame.len() >= 2
                                                    && frame[frame.len() - 2..] == [0x10, 0x03];
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
                                                    if let SwP08Message::Connected { .. } = message
                                                    {
                                                        let route =
                                                            RouterRoute::from_swp08(&message)
                                                                .expect(
                                                                "connected report carries a source",
                                                            );
                                                        table.apply(route);
                                                        let mut status = st.lock().unwrap_or_else(
                                                            std::sync::PoisonError::into_inner,
                                                        );
                                                        status.last_route = Some(Instant::now());
                                                        status.reports =
                                                            status.reports.saturating_add(1);
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
        Self {
            tx,
            stop,
            status,
            thread: Some(thread),
        }
    }
    /// Submit a bounded frame without waiting.
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
    fn stub_router_reports_route_then_disconnects_and_reconnects() {
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let table = Arc::new(RouteTable::new());
        let client = Client::start(addr, Protocol::Swp08, Arc::clone(&table));
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
                until(|| table.source_of(1, 42) == Some(source));
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
        until(|| table.source_of(1, 42) == Some(8));
        let status = client.status();
        assert!(status.connected);
        assert!(status.last_route.is_some());
        assert_eq!(status.reports, 1);
        stub.join().unwrap();
    }

    #[test]
    fn outbound_queue_is_bounded_and_wrong_link_is_rejected() {
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let client = Client::start(
            listener.local_addr().unwrap(),
            Protocol::Swp08,
            Arc::new(RouteTable::new()),
        );
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
