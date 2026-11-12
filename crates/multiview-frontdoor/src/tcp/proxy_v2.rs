//! PROXY protocol version 2 (the haproxy `proxy-protocol.txt` spec, section
//! 2.2), the `behind_proxy` mode header (design note section 8).
//!
//! Fail closed: only the binary v2 form, command `PROXY`, families `TCP4` and
//! `TCP6`. Anything else (v1 text, `LOCAL`, UDP, unix, a bad signature, an
//! oversized length) is an error and the connection is closed before any byte
//! of the client's protocol is read. TLVs are skipped, never interpreted.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The 12-byte v2 signature.
pub const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// The fixed part of a v2 header: signature, version/command, family, length.
pub const FIXED_LEN: usize = 16;
/// The largest address + TLV block accepted. A proxy sends 12 (TCP4) or 36
/// (TCP6) bytes of addresses plus a few TLVs; anything larger is refused so the
/// pre-auth read stays tiny.
pub const MAX_BLOCK_LEN: usize = 536;

/// Why a header was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyError {
    /// The 12-byte signature does not match (includes the v1 text form).
    Signature,
    /// The version nibble is not 2.
    Version,
    /// The `LOCAL` command (a health check): no client address, refused.
    Local,
    /// A command other than `PROXY` / `LOCAL`.
    Command,
    /// A family other than `TCP4` / `TCP6`.
    Family,
    /// The declared length exceeds [`MAX_BLOCK_LEN`] or is too short for the
    /// family's addresses.
    Length,
}

/// Parse the fixed 16-byte part, returning the length of the block that
/// follows (addresses + TLVs) and the family byte.
///
/// # Errors
/// A [`ProxyError`] for anything that is not a v2 `PROXY` header over TCP.
pub fn parse_fixed(fixed: &[u8; FIXED_LEN]) -> Result<(u8, usize), ProxyError> {
    let (sig, rest) = fixed.split_at(12);
    if sig != SIGNATURE {
        return Err(ProxyError::Signature);
    }
    let &[ver_cmd, family, hi, lo] = rest else {
        return Err(ProxyError::Signature);
    };
    if ver_cmd >> 4 != 2 {
        return Err(ProxyError::Version);
    }
    match ver_cmd & 0x0f {
        0x1 => {}
        0x0 => return Err(ProxyError::Local),
        _ => return Err(ProxyError::Command),
    }
    let need = match family {
        0x11 => 12,
        0x21 => 36,
        _ => return Err(ProxyError::Family),
    };
    let len = usize::from(u16::from_be_bytes([hi, lo]));
    if len > MAX_BLOCK_LEN || len < need {
        return Err(ProxyError::Length);
    }
    Ok((family, len))
}

/// Parse the address block that follows the fixed part: the client (source)
/// address. TLVs after the addresses are skipped.
///
/// # Errors
/// [`ProxyError::Length`] if the block is shorter than the family needs,
/// [`ProxyError::Family`] for an unknown family.
pub fn parse_block(family: u8, block: &[u8]) -> Result<SocketAddr, ProxyError> {
    match family {
        0x11 => {
            let Some(&[a, b, c, d, _, _, _, _, ph, pl, _, _]) = block.get(..12) else {
                return Err(ProxyError::Length);
            };
            let ip = Ipv4Addr::new(a, b, c, d);
            Ok(SocketAddr::new(
                IpAddr::V4(ip),
                u16::from_be_bytes([ph, pl]),
            ))
        }
        0x21 => {
            let Some(src) = block.get(..16) else {
                return Err(ProxyError::Length);
            };
            let Some(&[ph, pl]) = block.get(32..34) else {
                return Err(ProxyError::Length);
            };
            let mut octets = [0u8; 16];
            octets.copy_from_slice(src);
            let ip = Ipv6Addr::from(octets);
            // An IPv4-mapped client is an IPv4 client.
            let ip = ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4);
            Ok(SocketAddr::new(ip, u16::from_be_bytes([ph, pl])))
        }
        _ => Err(ProxyError::Family),
    }
}

/// Parse a complete header held in one buffer (tests, fuzzing): the client
/// address and the total header length.
///
/// # Errors
/// A [`ProxyError`]; a truncated buffer is [`ProxyError::Length`].
pub fn parse(header: &[u8]) -> Result<(SocketAddr, usize), ProxyError> {
    let Some(fixed) = header.get(..FIXED_LEN) else {
        return Err(if SIGNATURE.starts_with(header) {
            ProxyError::Length
        } else {
            ProxyError::Signature
        });
    };
    let mut buf = [0u8; FIXED_LEN];
    buf.copy_from_slice(fixed);
    let (family, len) = parse_fixed(&buf)?;
    let block = header
        .get(FIXED_LEN..FIXED_LEN + len)
        .ok_or(ProxyError::Length)?;
    Ok((parse_block(family, block)?, FIXED_LEN + len))
}

/// Read and parse a header from `io`, consuming exactly the header's bytes.
///
/// # Errors
/// An I/O error (EOF included) or `InvalidData` wrapping the refusal.
pub async fn read<R: tokio::io::AsyncRead + Unpin>(io: &mut R) -> std::io::Result<SocketAddr> {
    use tokio::io::AsyncReadExt as _;
    let invalid =
        |e: ProxyError| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}"));
    let mut fixed = [0u8; FIXED_LEN];
    io.read_exact(&mut fixed).await?;
    let (family, len) = parse_fixed(&fixed).map_err(invalid)?;
    let mut block = [0u8; MAX_BLOCK_LEN];
    let block = block
        .get_mut(..len)
        .ok_or_else(|| invalid(ProxyError::Length))?;
    io.read_exact(block).await?;
    parse_block(family, block).map_err(invalid)
}

/// Build a v2 `PROXY` header (tests and the chaos harness).
#[must_use]
pub fn encode(src: SocketAddr, dst: SocketAddr, tlvs: &[u8]) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    out.push(0x21);
    let mut block = Vec::new();
    if let (SocketAddr::V4(s), SocketAddr::V4(d)) = (src, dst) {
        out.push(0x11);
        block.extend_from_slice(&s.ip().octets());
        block.extend_from_slice(&d.ip().octets());
    } else {
        let v6 = |a: SocketAddr| match a.ip() {
            IpAddr::V4(v4) => v4.to_ipv6_mapped(),
            IpAddr::V6(v6) => v6,
        };
        out.push(0x21);
        block.extend_from_slice(&v6(src).octets());
        block.extend_from_slice(&v6(dst).octets());
    }
    block.extend_from_slice(&src.port().to_be_bytes());
    block.extend_from_slice(&dst.port().to_be_bytes());
    block.extend_from_slice(tlvs);
    let len = u16::try_from(block.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&block);
    out
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn v4() -> (SocketAddr, SocketAddr) {
        (
            "203.0.113.7:51000".parse().unwrap(),
            "192.0.2.1:443".parse().unwrap(),
        )
    }

    #[test]
    fn valid_v4_and_v6() {
        let (s, d) = v4();
        let h = encode(s, d, &[]);
        assert_eq!(parse(&h), Ok((s, 28)));
        let s6: SocketAddr = "[2001:db8::7]:51000".parse().unwrap();
        let d6: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let h = encode(s6, d6, &[]);
        assert_eq!(parse(&h), Ok((s6, 52)));
        // A mapped IPv4 client in a TCP6 header is that IPv4 client.
        let h = encode("[::ffff:203.0.113.7]:1".parse().unwrap(), d6, &[]);
        assert_eq!(parse(&h).unwrap().0, "203.0.113.7:1".parse().unwrap());
    }

    #[test]
    fn tlvs_are_skipped() {
        let (s, d) = v4();
        // PP2_TYPE_AUTHORITY (0x02) "example" + a NOOP (0x04).
        let mut tlvs = vec![0x02, 0x00, 0x07];
        tlvs.extend_from_slice(b"example");
        tlvs.extend_from_slice(&[0x04, 0x00, 0x02, 0, 0]);
        let h = encode(s, d, &tlvs);
        assert_eq!(parse(&h), Ok((s, h.len())));
    }

    #[test]
    fn refusals() {
        let (s, d) = v4();
        let good = encode(s, d, &[]);
        for cut in 0..good.len() {
            assert!(parse(&good[..cut]).is_err(), "truncated at {cut}");
        }
        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(parse(&bad), Err(ProxyError::Signature));
        assert_eq!(
            parse(b"PROXY TCP4 203.0.113.7 192.0.2.1 51000 443\r\n"),
            Err(ProxyError::Signature)
        );
        let mut local = good.clone();
        local[12] = 0x20;
        assert_eq!(parse(&local), Err(ProxyError::Local));
        let mut cmd = good.clone();
        cmd[12] = 0x22;
        assert_eq!(parse(&cmd), Err(ProxyError::Command));
        let mut ver = good.clone();
        ver[12] = 0x11;
        assert_eq!(parse(&ver), Err(ProxyError::Version));
        for fam in [0x00, 0x12, 0x22, 0x31, 0x32] {
            let mut f = good.clone();
            f[13] = fam;
            assert_eq!(parse(&f), Err(ProxyError::Family), "{fam:#x}");
        }
        let mut big = good.clone();
        big[14..16].copy_from_slice(&u16::try_from(MAX_BLOCK_LEN + 1).unwrap().to_be_bytes());
        big.resize(FIXED_LEN + MAX_BLOCK_LEN + 1, 0);
        assert_eq!(parse(&big), Err(ProxyError::Length));
        let mut short = good;
        short[14..16].copy_from_slice(&8u16.to_be_bytes());
        assert_eq!(parse(&short), Err(ProxyError::Length));
    }

    #[test]
    fn read_consumes_exactly_the_header() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (s, d) = v4();
        let mut bytes = encode(s, d, &[1, 0, 0]);
        bytes.extend_from_slice(b"\x16rest");
        let mut cursor = std::io::Cursor::new(bytes);
        let got = rt.block_on(read(&mut cursor)).unwrap();
        assert_eq!(got, s);
        assert_eq!(
            &cursor.get_ref()[usize::try_from(cursor.position()).unwrap()..],
            b"\x16rest"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4096))]
        #[test]
        fn random_bytes_never_panic(data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..700)) {
            let _ = parse(&data);
            let mut with_sig = SIGNATURE.to_vec();
            with_sig.extend_from_slice(&data);
            let _ = parse(&with_sig);
        }
    }
}
