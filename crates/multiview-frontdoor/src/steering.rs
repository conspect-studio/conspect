//! Validated-flow steering (design note "Slice 3 as built", issue #289).
//!
//! Two UDP sockets bound to the same `[::]:port` form a `SO_REUSEPORT` group.
//! Group index 0 is the **listener** (all unvalidated traffic), index 1 the
//! **established** socket. A classic BPF program attached with
//! `SO_ATTACH_REUSEPORT_CBPF` returns 1 for a source whose salted hash is in
//! the established set and 0 otherwise, so a flood of unvalidated traffic
//! fills the listener's kernel queue, never the one paying viewers use.
//!
//! The program sees `skb->data` at the UDP **payload** (the kernel pulls the
//! UDP header before running it), so every header field is read through
//! `SKF_NET_OFF` (relative to the network header). A load the kernel cannot
//! satisfy ends the program with 0: the listener, the safe default.
//!
//! Key: a salted 32-bit hash of (family, address words, source port), computed
//! identically here ([`steering_key`]) and in the program (ALU ops only: xor,
//! mul, rsh; classic BPF's A is 32-bit and `mul` wraps). An IPv4-mapped peer
//! hashes as IPv4, because an IPv4 datagram on the dual-stack socket carries an
//! IPv4 header. Lookup: a balanced binary search over the sorted keys, with a
//! 32-bit `ja` to each right subtree (conditional jumps are only 8-bit).
//!
//! A hash collision (an unvalidated source whose key equals a member's) only
//! shares the established *queue*: the dispatcher checks the exact
//! [`crate::EstablishedSet`] for every datagram from the established socket,
//! and a non-member pays the new-flow limits like any other new flow.
//!
//! Group membership: the kernel admits a socket into a reuseport group only
//! when it has the same effective uid as the group's first socket, so only a
//! process running as the engine's own user could join the group (and receive
//! a share of the traffic). Run the engine as a dedicated user.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io;
use std::net::{IpAddr, SocketAddr};

use crate::classify::canonical;
use crate::socket::RecvBufferReport;

/// One classic BPF instruction (`struct sock_filter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    /// Opcode.
    pub code: u16,
    /// Jump offset if true.
    pub jt: u8,
    /// Jump offset if false.
    pub jf: u8,
    /// Immediate / offset.
    pub k: u32,
}

/// `BPF_MAXINSNS`: the kernel rejects a classic program longer than this.
pub const BPF_MAXINSNS: usize = 4096;

/// Most peers one program steers: the largest set whose program fits in
/// [`BPF_MAXINSNS`]. The program length depends only on the number of keys
/// (the search tree's shape comes from the midpoint split), so the bound is
/// exact; the unit test `the_cap_is_the_largest_set_that_fits` checks both
/// sides of it. Peers beyond the cap stay on the listener queue (still
/// served, through the new-flow path, and their per-peer bucket) and are
/// counted (`steering_overflow`).
pub const STEERING_MAX_PEERS: usize = 1244;

/// `SKF_NET_OFF`: loads at `SKF_NET_OFF + n` read byte `n` of the network
/// header. Two's complement of -0x100000, as the kernel reads `k` as `int`.
const SKF_NET_OFF: u32 = 0xfff0_0000;

// Opcodes (linux/bpf_common.h).
const LD_W_ABS: u16 = 0x20;
const LD_H_ABS: u16 = 0x28;
const LD_B_ABS: u16 = 0x30;
const LD_H_IND: u16 = 0x48;
const LD_IMM: u16 = 0x00;
const LDX_MEM: u16 = 0x61;
const LDX_B_MSH: u16 = 0xb1;
const ST: u16 = 0x02;
const ALU_MUL_K: u16 = 0x24;
const ALU_RSH_K: u16 = 0x74;
const ALU_XOR_X: u16 = 0xac;
const JMP_JA: u16 = 0x05;
const JMP_JEQ_K: u16 = 0x15;
const JMP_JGT_K: u16 = 0x25;
const RET_K: u16 = 0x06;

/// Hash multipliers (odd, well mixed).
const K1: u32 = 0x9e37_79b1;
const K2: u32 = 0x85eb_ca77;
/// Family tags mixed into the salt.
const TAG_V4: u32 = 4;
const TAG_V6: u32 = 6;

const fn insn(code: u16, jt: u8, jf: u8, k: u32) -> Insn {
    Insn { code, jt, jf, k }
}

const fn mix(h: u32, w: u32) -> u32 {
    let a = (w ^ h).wrapping_mul(K1);
    a ^ (a >> 15)
}

const fn finish(h: u32, port: u32) -> u32 {
    let a = mix(h, port).wrapping_mul(K2);
    a ^ (a >> 13)
}

/// The steering key of a peer address under `salt`: exactly what the program
/// computes from the datagram's headers.
#[must_use]
pub fn steering_key(salt: u32, addr: SocketAddr) -> u32 {
    let addr = canonical(addr);
    let port = u32::from(addr.port());
    match addr.ip() {
        IpAddr::V4(v4) => finish(mix(mix(salt, TAG_V4), u32::from(v4)), port),
        IpAddr::V6(v6) => {
            let mut h = mix(salt, TAG_V6);
            for chunk in v6.octets().chunks_exact(4) {
                let word = chunk.try_into().map(u32::from_be_bytes).unwrap_or_default();
                h = mix(h, word);
            }
            finish(h, port)
        }
    }
}

/// A fresh random salt (std's `RandomState`, seeded per process from the OS,
/// with a per-instance counter: no new dependency).
#[must_use]
pub fn random_salt() -> u32 {
    let wide = RandomState::new().hash_one(0x5354_4545_u64);
    let b = wide.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// The set one program steers: a salt and the sorted, deduplicated keys of at
/// most [`STEERING_MAX_PEERS`] peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramSet {
    salt: u32,
    keys: Vec<u32>,
    steered: usize,
    overflow: usize,
}

impl ProgramSet {
    /// Build from `peers` (canonicalised and deduplicated; beyond
    /// [`STEERING_MAX_PEERS`] distinct peers the rest are counted in
    /// [`overflow`](Self::overflow)).
    #[must_use]
    pub fn build(salt: u32, peers: impl IntoIterator<Item = SocketAddr>) -> Self {
        let mut addrs: Vec<SocketAddr> = peers.into_iter().map(canonical).collect();
        addrs.sort_unstable();
        addrs.dedup();
        let steered = addrs.len().min(STEERING_MAX_PEERS);
        let overflow = addrs.len().saturating_sub(STEERING_MAX_PEERS);
        let mut keys: Vec<u32> = addrs
            .iter()
            .take(steered)
            .map(|a| steering_key(salt, *a))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        Self {
            salt,
            keys,
            steered,
            overflow,
        }
    }

    /// An empty set (the program returns 0 for every datagram).
    #[must_use]
    pub fn empty() -> Self {
        Self::build(0, std::iter::empty())
    }

    /// The salt.
    #[must_use]
    pub const fn salt(&self) -> u32 {
        self.salt
    }

    /// Peers steered (distinct addresses within the cap).
    #[must_use]
    pub const fn steered(&self) -> usize {
        self.steered
    }

    /// Peers not steered because of the cap.
    #[must_use]
    pub const fn overflow(&self) -> usize {
        self.overflow
    }

    /// Whether `addr` hashes to a steered key (members, and colliding
    /// non-members).
    #[must_use]
    pub fn matches(&self, addr: SocketAddr) -> bool {
        self.keys
            .binary_search(&steering_key(self.salt, addr))
            .is_ok()
    }

    /// The classic BPF program.
    #[must_use]
    pub fn program(&self) -> Vec<Insn> {
        compile(self.salt, &self.keys)
    }
}

/// Mix the word in A into the hash held in M[0]; leaves the result in M[0]
/// (and A).
fn emit_mix(out: &mut Vec<Insn>) {
    out.extend([
        insn(LDX_MEM, 0, 0, 0),
        insn(ALU_XOR_X, 0, 0, 0),
        insn(ALU_MUL_K, 0, 0, K1),
        insn(ST, 0, 0, 0),
        insn(ALU_RSH_K, 0, 0, 15),
        insn(LDX_MEM, 0, 0, 0),
        insn(ALU_XOR_X, 0, 0, 0),
        insn(ST, 0, 0, 0),
    ]);
}

fn compile(salt: u32, keys: &[u32]) -> Vec<Insn> {
    if keys.is_empty() {
        return vec![insn(RET_K, 0, 0, 0)];
    }
    // IPv6 block: next header must be UDP, then four address words, then the
    // source port (fixed 40-byte header). Ends with a `ja` to the common tail.
    let mut v6 = vec![
        insn(LD_B_ABS, 0, 0, SKF_NET_OFF + 6),
        insn(JMP_JEQ_K, 1, 0, 17),
        insn(RET_K, 0, 0, 0),
        insn(LD_IMM, 0, 0, mix(salt, TAG_V6)),
        insn(ST, 0, 0, 0),
    ];
    for off in [8u32, 12, 16, 20] {
        v6.push(insn(LD_W_ABS, 0, 0, SKF_NET_OFF + off));
        emit_mix(&mut v6);
    }
    v6.push(insn(LD_H_ABS, 0, 0, SKF_NET_OFF + 40));
    // IPv4 block: the address word, then the source port after the options.
    let mut v4 = vec![
        insn(LD_IMM, 0, 0, mix(salt, TAG_V4)),
        insn(ST, 0, 0, 0),
        insn(LD_W_ABS, 0, 0, SKF_NET_OFF + 12),
    ];
    emit_mix(&mut v4);
    v4.extend([
        insn(LDX_B_MSH, 0, 0, SKF_NET_OFF),
        insn(LD_H_IND, 0, 0, SKF_NET_OFF),
    ]);
    // v6 jumps over the v4 block to the tail.
    let skip_v4 = u32::try_from(v4.len()).unwrap_or(u32::MAX);
    v6.push(insn(JMP_JA, 0, 0, skip_v4));
    // Header: version nibble. v4 jumps over the v6 block (< 256 insns).
    let jt_v4 = u8::try_from(v6.len().saturating_add(2)).unwrap_or(u8::MAX);
    let mut out = vec![
        insn(LD_B_ABS, 0, 0, SKF_NET_OFF),
        insn(ALU_RSH_K, 0, 0, 4),
        insn(JMP_JEQ_K, jt_v4, 0, 4),
        insn(JMP_JEQ_K, 1, 0, 6),
        insn(RET_K, 0, 0, 0),
    ];
    // After the jeq #4 (index 2): +1 for the jeq #6, +1 for the ret 0, then v6.
    out.extend(v6);
    out.extend(v4);
    // Tail: A = port, M[0] = hash. Final mix, then the search.
    emit_mix(&mut out);
    out.extend([
        insn(ALU_MUL_K, 0, 0, K2),
        insn(ST, 0, 0, 0),
        insn(ALU_RSH_K, 0, 0, 13),
        insn(LDX_MEM, 0, 0, 0),
        insn(ALU_XOR_X, 0, 0, 0),
    ]);
    emit_search(&mut out, keys);
    out
}

/// A balanced binary search over sorted `keys`, value in A: ret 1 on a hit,
/// ret 0 otherwise.
fn emit_search(out: &mut Vec<Insn>, keys: &[u32]) {
    let mid = keys.len() / 2;
    let (Some(left), Some((&key, right))) = (
        keys.get(..mid),
        keys.get(mid..).and_then(<[u32]>::split_first),
    ) else {
        out.push(insn(RET_K, 0, 0, 0));
        return;
    };
    out.push(insn(JMP_JEQ_K, 0, 1, key));
    out.push(insn(RET_K, 0, 0, 1));
    match (left.is_empty(), right.is_empty()) {
        // Every key on the other side is greater (or smaller): searching just
        // the non-empty side still answers 0 for a value on the empty side.
        (true, _) => emit_search(out, right),
        (false, true) => emit_search(out, left),
        (false, false) => {
            let mut l = Vec::new();
            emit_search(&mut l, left);
            out.push(insn(JMP_JGT_K, 0, 1, key));
            out.push(insn(
                JMP_JA,
                0,
                0,
                u32::try_from(l.len()).unwrap_or(u32::MAX),
            ));
            out.extend(l);
            emit_search(out, right);
        }
    }
}

/// The validated-flow socket pair: index 0 listener, index 1 established,
/// both dual-stack on the same `[::]:port` with `SO_REUSEPORT`. Linux only.
/// [`crate::FrontDoor`] uses it; tests use it directly to exercise the kernel
/// mechanism.
#[derive(Debug)]
pub struct ReuseportGroup {
    listener: std::net::UdpSocket,
    established: std::net::UdpSocket,
    buffers: [RecvBufferReport; 2],
}

impl ReuseportGroup {
    /// Bind the listener first (group index 0), read its port, then the
    /// established socket on the same port (index 1). Both non-blocking.
    ///
    /// # Errors
    ///
    /// Any socket, option or bind error; `Unsupported` off Linux.
    pub fn bind(addr: SocketAddr, recv_buffer_bytes: usize) -> io::Result<Self> {
        if !cfg!(target_os = "linux") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "validated-flow steering needs Linux SO_ATTACH_REUSEPORT_CBPF",
            ));
        }
        let (listener, l_report) = crate::socket::new_udp_socket(addr, true, recv_buffer_bytes)?;
        let port = listener.local_addr()?.port();
        let addr = SocketAddr::new(addr.ip(), port);
        let (established, e_report) = crate::socket::new_udp_socket(addr, true, recv_buffer_bytes)?;
        Ok(Self {
            listener,
            established,
            buffers: [l_report, e_report],
        })
    }

    /// Attach (or atomically replace) the group's steering program.
    ///
    /// # Errors
    ///
    /// The `setsockopt` error; `Unsupported` off Linux.
    pub fn attach(&self, set: &ProgramSet) -> io::Result<()> {
        attach_to(&self.listener, set)
    }

    /// The listener (group index 0).
    #[must_use]
    pub const fn listener(&self) -> &std::net::UdpSocket {
        &self.listener
    }

    /// The established socket (group index 1).
    #[must_use]
    pub const fn established(&self) -> &std::net::UdpSocket {
        &self.established
    }

    /// Receive-buffer reports, listener then established.
    #[must_use]
    pub const fn buffers(&self) -> [RecvBufferReport; 2] {
        self.buffers
    }

    /// The two sockets, listener first.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        std::net::UdpSocket,
        std::net::UdpSocket,
        [RecvBufferReport; 2],
    ) {
        (self.listener, self.established, self.buffers)
    }
}

/// Attach `set`'s program to the reuseport group `socket` belongs to.
#[cfg(target_os = "linux")]
pub(crate) fn attach_to(socket: &impl std::os::fd::AsFd, set: &ProgramSet) -> io::Result<()> {
    crate::sys::attach_reuseport_cbpf(socket.as_fd(), &set.program())
}

/// Attach is Linux only.
#[cfg(not(target_os = "linux"))]
pub(crate) fn attach_to<T>(_socket: &T, _set: &ProgramSet) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_ATTACH_REUSEPORT_CBPF is Linux only",
    ))
}

/// How the front door receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteeringMode {
    /// A `SO_REUSEPORT` pair with the CBPF steering program attached.
    ReuseportCbpf,
    /// One socket, no group, no program.
    SingleSocket {
        /// Why (configuration, platform, or the error that forced it).
        reason: String,
    },
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// The kernel's `SKF_NET_OFF` window: loads in `[NET, NET + 0xfffff]`.
    const NET: i64 = -0x10_0000;

    /// A fake skb: the network header, the UDP header and the payload, with
    /// `data` at the payload (as `run_bpf_filter` presents it).
    struct Packet {
        bytes: Vec<u8>,
        payload_at: usize,
    }

    impl Packet {
        fn v4(src: Ipv4Addr, port: u16, ihl_words: u8) -> Self {
            let ihl = usize::from(ihl_words) * 4;
            let mut b = vec![0u8; ihl];
            b[0] = 0x40 | ihl_words;
            b[9] = 17;
            b[12..16].copy_from_slice(&src.octets());
            b[16..20].copy_from_slice(&[127, 0, 0, 1]);
            let mut udp = vec![0u8; 8];
            udp[0..2].copy_from_slice(&port.to_be_bytes());
            udp[2..4].copy_from_slice(&443u16.to_be_bytes());
            b.extend(udp);
            let payload_at = b.len();
            b.extend([0xc0u8; 32]);
            Self {
                bytes: b,
                payload_at,
            }
        }

        fn v6(src: Ipv6Addr, port: u16, next_header: u8) -> Self {
            let mut b = vec![0u8; 40];
            b[0] = 0x60;
            b[6] = next_header;
            b[8..24].copy_from_slice(&src.octets());
            b[24..40].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
            let mut udp = vec![0u8; 8];
            udp[0..2].copy_from_slice(&port.to_be_bytes());
            b.extend(udp);
            let payload_at = b.len();
            b.extend([0x40u8; 32]);
            Self {
                bytes: b,
                payload_at,
            }
        }

        /// A load of `size` bytes at `k` (signed, as the kernel reads it).
        fn load(&self, k: i64, size: usize) -> Option<u32> {
            let at = if k >= 0 {
                self.payload_at as i64 + k
            } else if (NET..NET + 0x10_0000).contains(&k) {
                k - NET
            } else {
                return None;
            };
            let at = usize::try_from(at).ok()?;
            let s = self.bytes.get(at..at.checked_add(size)?)?;
            Some(s.iter().fold(0u32, |a, &b| (a << 8) | u32::from(b)))
        }
    }

    /// Interpreter for exactly the opcodes `compile` emits. A failed load
    /// returns 0, as the kernel does.
    #[allow(clippy::many_single_char_names, reason = "the BPF register names")]
    fn run(prog: &[Insn], pkt: &Packet) -> u32 {
        let (mut a, mut x) = (0u32, 0u32);
        let mut mem = [0u32; 16];
        let mut pc = 0usize;
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps < 10_000, "no loops in classic BPF");
            let i = prog[pc];
            pc += 1;
            let k = i64::from(i.k as i32);
            match i.code {
                LD_W_ABS | LD_H_ABS | LD_B_ABS => {
                    let size = match i.code {
                        LD_W_ABS => 4,
                        LD_H_ABS => 2,
                        _ => 1,
                    };
                    let Some(v) = pkt.load(k, size) else { return 0 };
                    a = v;
                }
                LD_H_IND => {
                    let Some(v) = pkt.load(i64::from(x) + k, 2) else {
                        return 0;
                    };
                    a = v;
                }
                LDX_B_MSH => {
                    let Some(v) = pkt.load(k, 1) else { return 0 };
                    x = 4 * (v & 0xf);
                }
                LD_IMM => a = i.k,
                LDX_MEM => x = mem[i.k as usize],
                ST => mem[i.k as usize] = a,
                ALU_MUL_K => a = a.wrapping_mul(i.k),
                ALU_RSH_K => a >>= i.k,
                ALU_XOR_X => a ^= x,
                JMP_JA => pc += i.k as usize,
                JMP_JEQ_K => pc += usize::from(if a == i.k { i.jt } else { i.jf }),
                JMP_JGT_K => pc += usize::from(if a > i.k { i.jt } else { i.jf }),
                RET_K => return i.k,
                other => panic!("opcode {other:#x} not emitted by compile"),
            }
        }
    }

    fn packet_of(addr: SocketAddr) -> Packet {
        match canonical(addr) {
            SocketAddr::V4(v4) => Packet::v4(*v4.ip(), v4.port(), 5),
            SocketAddr::V6(v6) => Packet::v6(*v6.ip(), v6.port(), 17),
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn addr(&mut self) -> SocketAddr {
            let r = self.next();
            let port = (r >> 48) as u16;
            if r & 1 == 0 {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::from((r >> 8) as u32)), port)
            } else {
                let hi = u128::from(self.next()) << 64;
                SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::from(hi | u128::from(self.next()))),
                    port,
                )
            }
        }
    }

    fn members(n: usize, seed: u64) -> Vec<SocketAddr> {
        let mut rng = Rng(seed);
        (0..n).map(|_| rng.addr()).collect()
    }

    #[test]
    fn every_member_is_steered_and_random_non_members_only_by_true_collision() {
        let peers = members(STEERING_MAX_PEERS, 0x1234_5678_9abc_def1);
        let set = ProgramSet::build(random_salt(), peers.clone());
        let prog = set.program();
        for p in &peers {
            assert_eq!(run(&prog, &packet_of(*p)), 1, "member {p}");
        }
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        let (mut steered, mut collisions) = (0usize, 0usize);
        for _ in 0..100_000 {
            let a = rng.addr();
            if peers.contains(&a) {
                continue;
            }
            steered += usize::try_from(run(&prog, &packet_of(a))).unwrap();
            collisions += usize::from(set.matches(a));
        }
        assert_eq!(steered, collisions, "only real hash collisions are steered");
        // 100 000 x 1244 / 2^32: expected 0.03.
        assert!(collisions <= 2, "{collisions}");
    }

    #[test]
    fn mapped_v4_hashes_as_v4_and_v4_options_are_skipped() {
        let v4: SocketAddr = "192.0.2.10:5000".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:192.0.2.10]:5000".parse().unwrap();
        let salt = random_salt();
        assert_eq!(steering_key(salt, v4), steering_key(salt, mapped));
        let set = ProgramSet::build(salt, [mapped]);
        let prog = set.program();
        assert_eq!(
            run(&prog, &Packet::v4(Ipv4Addr::new(192, 0, 2, 10), 5000, 5)),
            1
        );
        // IP options (IHL 8): the port is still found through ldxb msh.
        assert_eq!(
            run(&prog, &Packet::v4(Ipv4Addr::new(192, 0, 2, 10), 5000, 8)),
            1
        );
        assert_eq!(
            run(&prog, &Packet::v4(Ipv4Addr::new(192, 0, 2, 10), 5001, 5)),
            0
        );
        assert_eq!(set.steered(), 1);
    }

    #[test]
    fn v6_with_an_extension_header_or_another_version_goes_to_the_listener() {
        let peer: SocketAddr = "[2001:db8::7]:4433".parse().unwrap();
        let set = ProgramSet::build(random_salt(), [peer]);
        let prog = set.program();
        let ip: Ipv6Addr = "2001:db8::7".parse().unwrap();
        assert_eq!(run(&prog, &Packet::v6(ip, 4433, 17)), 1);
        // Hop-by-hop (0) and fragment (44) headers: never steered.
        assert_eq!(run(&prog, &Packet::v6(ip, 4433, 0)), 0);
        assert_eq!(run(&prog, &Packet::v6(ip, 4433, 44)), 0);
        let mut odd = Packet::v6(ip, 4433, 17);
        odd.bytes[0] = 0x50;
        assert_eq!(run(&prog, &odd), 0);
    }

    #[test]
    fn truncated_packets_go_to_the_listener() {
        let peer: SocketAddr = "[2001:db8::7]:4433".parse().unwrap();
        let v4peer: SocketAddr = "192.0.2.1:9".parse().unwrap();
        let prog = ProgramSet::build(random_salt(), [peer, v4peer]).program();
        let full = packet_of(peer);
        for cut in 0..42 {
            let p = Packet {
                bytes: full.bytes[..cut].to_vec(),
                payload_at: cut,
            };
            assert_eq!(run(&prog, &p), 0, "v6 cut at {cut}");
        }
        let full = packet_of(v4peer);
        for cut in 0..22 {
            let p = Packet {
                bytes: full.bytes[..cut].to_vec(),
                payload_at: cut,
            };
            assert_eq!(run(&prog, &p), 0, "v4 cut at {cut}");
        }
    }

    #[test]
    fn empty_set_steers_nothing() {
        let prog = ProgramSet::empty().program();
        let mut rng = Rng(7);
        for _ in 0..1000 {
            assert_eq!(run(&prog, &packet_of(rng.addr())), 0);
        }
    }

    #[test]
    fn the_cap_is_the_largest_set_that_fits() {
        // Compile raw key lists (ProgramSet::build itself stops at the cap).
        let len_for = |n: usize| {
            let mut keys: Vec<u32> = members(n, 0x0bad_cafe_0000_0001 + n as u64)
                .into_iter()
                .map(|a| steering_key(1, a))
                .collect();
            keys.sort_unstable();
            keys.dedup();
            compile(1, &keys).len()
        };
        let at_cap = len_for(STEERING_MAX_PEERS);
        assert!(at_cap <= BPF_MAXINSNS, "{at_cap}");
        assert!(
            len_for(STEERING_MAX_PEERS + 1) > BPF_MAXINSNS,
            "the cap is the largest N that fits"
        );
        // The length depends on N only, not on the key values.
        assert_eq!(at_cap, {
            let keys: Vec<u32> = (0..STEERING_MAX_PEERS as u32).map(|k| k * 7).collect();
            compile(9, &keys).len()
        });
        // Beyond the cap peers are counted, not steered.
        let set = ProgramSet::build(1, members(STEERING_MAX_PEERS + 5, 99));
        assert_eq!(set.steered(), STEERING_MAX_PEERS);
        assert_eq!(set.overflow(), 5);
        assert!(set.program().len() <= BPF_MAXINSNS);
    }

    #[test]
    fn jumps_stay_inside_the_program() {
        let prog = ProgramSet::build(3, members(STEERING_MAX_PEERS, 5)).program();
        for (pc, i) in prog.iter().enumerate() {
            let targets: Vec<usize> = match i.code {
                JMP_JA => vec![pc + 1 + i.k as usize],
                JMP_JEQ_K | JMP_JGT_K => {
                    vec![pc + 1 + usize::from(i.jt), pc + 1 + usize::from(i.jf)]
                }
                _ => vec![],
            };
            for t in targets {
                assert!(t < prog.len(), "pc {pc} jumps to {t}");
            }
        }
        assert_eq!(prog.last().unwrap().code, RET_K);
    }
}
