//! Sockets handed in by the service manager (systemd socket activation, the
//! `LISTEN_FDS` protocol; design note section 9, slice 6, issue #292).
//!
//! With `conspect.socket` (`deploy/systemd/`), systemd owns TCP 443, TCP 80 and
//! UDP 443 and passes them to the engine as file descriptors 3.. . The sockets
//! outlive an engine restart: the kernel queues TCP SYNs in the listen backlog
//! and UDP datagrams in the receive buffer while the new process starts, so a
//! browser's retry lands on the new engine instead of a refused port.
//!
//! [`ListenSockets`] matches each inherited socket to the listener that wants
//! it by **socket type and local port** (a TCP listener on 443 is the TCP door,
//! a UDP socket on 443 the UDP door, a TCP listener on 80 the redirect), so the
//! unit needs no `FileDescriptorName=` bookkeeping. A listener with no matching
//! inherited socket binds its own as before, so the engine runs the same with
//! and without socket activation.
//!
//! `unsafe` is forbidden in this workspace: the descriptor-to-socket
//! conversion is done by `listenfd` (MIT/Apache-2.0), which checks that each
//! descriptor really is a socket of the requested type before adopting it and
//! marks it close-on-exec, so a child process (`FFmpeg`, a helper) never
//! inherits the engine's public sockets.

use std::net::{SocketAddr, TcpListener, UdpSocket};

/// Inherited sockets not yet claimed by a listener.
#[derive(Debug, Default)]
pub struct ListenSockets {
    tcp: Vec<(SocketAddr, TcpListener)>,
    udp: Vec<(SocketAddr, UdpSocket)>,
    /// Descriptors that were passed but are neither a TCP listener nor a UDP
    /// socket (a unix socket, a FIFO): reported, never used.
    unusable: usize,
}

impl ListenSockets {
    /// No inherited sockets: every listener binds its own.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Adopt the sockets the service manager passed (`LISTEN_PID` /
    /// `LISTEN_FDS`), once per process.
    ///
    /// `listenfd` removes `LISTEN_PID` and `LISTEN_FDS` from the environment
    /// (as `sd_listen_fds(3)` does) so no child mistakes them for its own.
    /// Call this in `main` **before any other thread exists** (before the
    /// Tokio runtime is built): changing the environment while another thread
    /// reads it is not thread-safe on glibc.
    /// Without activation (no `LISTEN_FDS`, or a `LISTEN_PID` that is not this
    /// process) it returns [`ListenSockets::empty`].
    #[must_use]
    pub fn from_env() -> Self {
        let mut fds = listenfd::ListenFd::from_env();
        let mut sockets = Self::empty();
        for index in 0..fds.len() {
            // A listening TCP socket first; on a type mismatch listenfd leaves
            // the descriptor in place, so the UDP attempt sees it.
            match fds.take_tcp_listener(index) {
                Ok(Some(listener)) => {
                    sockets.push_tcp(listener);
                    continue;
                }
                Ok(None) => continue,
                Err(_) => {}
            }
            match fds.take_udp_socket(index) {
                Ok(Some(socket)) => sockets.push_udp(socket),
                Ok(None) => {}
                Err(_) => sockets.unusable += 1,
            }
        }
        sockets
    }

    /// Log what was inherited (call once logging is up; [`from_env`]
    /// runs before the runtime and logs nothing).
    ///
    /// [`from_env`]: Self::from_env
    pub fn log_summary(&self) {
        if self.unusable > 0 {
            tracing::warn!(
                unusable = self.unusable,
                "socket activation: passed descriptors that are neither a TCP listener nor a UDP socket were ignored"
            );
        }
        if !self.is_empty() {
            tracing::info!(
                tcp = ?self.tcp.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
                udp = ?self.udp.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
                "socket activation: inherited sockets from the service manager"
            );
        }
    }

    /// Add an inherited TCP listener (also the seam the tests use to pass a
    /// pre-bound socket, as systemd would).
    pub fn push_tcp(&mut self, listener: TcpListener) {
        match listener.local_addr() {
            Ok(addr) => self.tcp.push((addr, listener)),
            Err(error) => {
                tracing::warn!(%error, "socket activation: a TCP listener has no local address; ignored");
                self.unusable += 1;
            }
        }
    }

    /// Add an inherited UDP socket.
    pub fn push_udp(&mut self, socket: UdpSocket) {
        match socket.local_addr() {
            Ok(addr) => self.udp.push((addr, socket)),
            Err(error) => {
                tracing::warn!(%error, "socket activation: a UDP socket has no local address; ignored");
                self.unusable += 1;
            }
        }
    }

    /// Whether no socket is left.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tcp.is_empty() && self.udp.is_empty()
    }

    /// Take the TCP listener for `want` (matched by port; port 0 never
    /// matches). An address that differs from the configured one is used
    /// anyway, with a warning: the service manager is the authority on where
    /// the socket listens, and binding our own would fail with `EADDRINUSE`.
    pub fn take_tcp(&mut self, want: SocketAddr) -> Option<TcpListener> {
        let i = position(self.tcp.iter().map(|(a, _)| *a), want)?;
        let (addr, listener) = self.tcp.swap_remove(i);
        warn_if_moved(addr, want, "TCP");
        Some(listener)
    }

    /// Take the UDP socket for `want` (matched by port, as
    /// [`take_tcp`](Self::take_tcp)).
    pub fn take_udp(&mut self, want: SocketAddr) -> Option<UdpSocket> {
        let i = position(self.udp.iter().map(|(a, _)| *a), want)?;
        let (addr, socket) = self.udp.swap_remove(i);
        warn_if_moved(addr, want, "UDP");
        Some(socket)
    }

    /// Take every UDP socket for `want`'s port, in the order the service
    /// manager passed them (its bind order). Two sockets on one port are a
    /// `ReusePort=yes` pair for validated-flow steering: the first joined the
    /// reuseport group first, so it is group index 0 (the listener) and the
    /// second index 1 (the established socket).
    pub fn take_udp_all(&mut self, want: SocketAddr) -> Vec<UdpSocket> {
        let mut taken = Vec::new();
        while let Some(socket) = self.take_udp_in_order(want) {
            taken.push(socket);
        }
        taken
    }

    fn take_udp_in_order(&mut self, want: SocketAddr) -> Option<UdpSocket> {
        let i = position(self.udp.iter().map(|(a, _)| *a), want)?;
        let (addr, socket) = self.udp.remove(i);
        warn_if_moved(addr, want, "UDP");
        Some(socket)
    }

    /// Log every socket no listener claimed (its port is held by the service
    /// manager and nobody accepts on it: a configuration mismatch between the
    /// socket unit and `[network]`). The sockets are dropped here; systemd
    /// keeps its own copy.
    pub fn report_unclaimed(self) {
        for (addr, _) in &self.tcp {
            tracing::warn!(%addr, "socket activation: an inherited TCP socket matches no listener ([network] mode / tcp_port / http_redirect_port); connections to it queue unanswered");
        }
        for (addr, _) in &self.udp {
            tracing::warn!(%addr, "socket activation: an inherited UDP socket matches no listener ([network] https_port); datagrams to it are not read");
        }
    }
}

fn position(mut addrs: impl Iterator<Item = SocketAddr>, want: SocketAddr) -> Option<usize> {
    if want.port() == 0 {
        return None;
    }
    addrs.position(|a| a.port() == want.port())
}

fn warn_if_moved(got: SocketAddr, want: SocketAddr, kind: &str) {
    if got.ip() != want.ip() && !want.ip().is_unspecified() {
        tracing::warn!(%got, %want, kind, "socket activation: the inherited socket listens on a different address than [network] bind; using the inherited one");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_type_and_port_and_never_port_zero() {
        let t = TcpListener::bind("[::1]:0").unwrap();
        let u = UdpSocket::bind("[::1]:0").unwrap();
        let (tp, up) = (t.local_addr().unwrap().port(), u.local_addr().unwrap().port());
        let mut s = ListenSockets::empty();
        s.push_tcp(t);
        s.push_udp(u);
        assert!(s.take_tcp("[::]:0".parse().unwrap()).is_none());
        assert!(s.take_udp(SocketAddr::new("::".parse().unwrap(), tp)).is_none() || tp == up);
        let got = s.take_tcp(SocketAddr::new("::".parse().unwrap(), tp)).unwrap();
        assert_eq!(got.local_addr().unwrap().port(), tp);
        assert!(s.take_tcp(SocketAddr::new("::".parse().unwrap(), tp)).is_none());
        assert!(s.take_udp(SocketAddr::new("::".parse().unwrap(), up)).is_some());
        assert!(s.is_empty());
    }
}
