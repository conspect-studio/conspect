//! Bounded, off-clock TCP transport for router reports.
use super::{
    transport::{read_ember, read_swp08, OutboundFrame},
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
                        st.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .connected = true;
                        let mut frame = Vec::with_capacity(4096);
                        let mut buf = [0u8; 512];
                        while !s.load(Ordering::Acquire) {
                            while let Ok(out) = rx.try_recv() {
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
                                            Protocol::Ember => b == 0xff,
                                            Protocol::Swp08 => {
                                                frame.len() >= 3
                                                    && frame[frame.len() - 3..frame.len() - 1]
                                                        == [0x10, 0x03]
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
                                                    if let Some(route) =
                                                        RouterRoute::from_swp08(&message)
                                                    {
                                                        table.apply(route);
                                                        st.lock()
                                                            .unwrap_or_else(
                                                                std::sync::PoisonError::into_inner,
                                                            )
                                                            .last_route = Some(Instant::now());
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
                thread::sleep(Duration::from_millis(250));
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
