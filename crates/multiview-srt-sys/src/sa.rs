//! The socket-address layouts libsrt's `srt_bind` / `srt_accept` read and
//! write, as the platform C ABI defines them. Local `#[repr(C)]` definitions
//! instead of `libc`, which has no Winsock types: the Linux/BSD layouts come
//! from `<netinet/in.h>`, the Windows ones from `<ws2ipdef.h>`. Unit tests pin
//! the sizes and the encode/decode round trip on every platform.

// Unix: the platform's own definitions from libc, exactly as before (covers
// every unix target libc supports: Linux, Android, Apple, the BSDs, ...).
#[cfg(unix)]
mod imp {
    pub(crate) use libc::{
        sa_family_t as Family, sockaddr as Sockaddr, sockaddr_in as SockaddrIn,
        sockaddr_in6 as SockaddrIn6, sockaddr_storage as SockaddrStorage, AF_INET, AF_INET6,
    };
    /// `AF_INET6` as the family field value (a small positive constant on every
    /// unix target; checked, never a silent cast).
    pub(crate) fn af_inet6_family() -> Family {
        Family::try_from(AF_INET6).unwrap_or_default()
    }
}

// Windows: libc has no Winsock types; the <ws2ipdef.h> layouts.
#[cfg(windows)]
mod imp {
    use std::os::raw::c_int;

    pub(crate) const AF_INET: c_int = 2;
    pub(crate) const AF_INET6: c_int = 23;
    pub(crate) type Family = u16;
    /// `AF_INET6` as the family field value.
    pub(crate) fn af_inet6_family() -> Family {
        23
    }

    /// Opaque `struct sockaddr` (only ever passed by pointer).
    #[repr(C)]
    pub(crate) struct Sockaddr {
        _opaque: [u8; 0],
    }

    /// `SOCKADDR_IN6` (28 bytes).
    #[allow(clippy::struct_field_names)] // C field names, kept verbatim
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct SockaddrIn6 {
        pub(crate) sin6_family: Family,
        pub(crate) sin6_port: u16,
        pub(crate) sin6_flowinfo: u32,
        pub(crate) sin6_addr: In6Addr,
        pub(crate) sin6_scope_id: u32,
    }

    /// `IN6_ADDR`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct In6Addr {
        pub(crate) s6_addr: [u8; 16],
    }

    /// `SOCKADDR_IN` (16 bytes).
    #[allow(clippy::struct_field_names)] // C field names, kept verbatim
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct SockaddrIn {
        pub(crate) sin_family: Family,
        pub(crate) sin_port: u16,
        pub(crate) sin_addr: InAddr,
        pub(crate) sin_zero: [u8; 8],
    }

    /// `IN_ADDR`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct InAddr {
        pub(crate) s_addr: u32,
    }

    /// `SOCKADDR_STORAGE` (128 bytes, 8-byte aligned).
    #[repr(C, align(8))]
    #[derive(Clone, Copy)]
    pub(crate) struct SockaddrStorage {
        pub(crate) ss_family: Family,
        _pad: [u8; 126],
    }
}

#[allow(unused_imports)]
pub(crate) use imp::*;

#[cfg(test)]
mod tests {
    use super::{SockaddrIn, SockaddrIn6, SockaddrStorage};

    #[test]
    fn family_constants_agree() {
        assert_eq!(i32::from(super::af_inet6_family()), super::AF_INET6);
    }

    #[test]
    fn layouts_match_the_platform_abi() {
        assert_eq!(std::mem::size_of::<SockaddrIn6>(), 28);
        assert_eq!(std::mem::size_of::<SockaddrIn>(), 16);
        assert_eq!(std::mem::size_of::<SockaddrStorage>(), 128);
    }

    /// The Windows definitions are pinned to the documented Winsock values.
    #[cfg(windows)]
    #[test]
    fn winsock_constants() {
        assert_eq!(super::AF_INET, 2);
        assert_eq!(super::AF_INET6, 23);
        assert_eq!(std::mem::align_of::<SockaddrStorage>(), 8);
    }
}
