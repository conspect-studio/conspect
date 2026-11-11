//! The socket-address layouts libsrt's `srt_bind` / `srt_accept` read and
//! write, as the platform C ABI defines them. Local `#[repr(C)]` definitions
//! instead of `libc`, which has no Winsock types: the Linux/BSD layouts come
//! from `<netinet/in.h>`, the Windows ones from `<ws2ipdef.h>`. Unit tests pin
//! the sizes and the encode/decode round trip on every platform.

use std::os::raw::c_int;

/// `AF_INET` on every supported platform.
pub(crate) const AF_INET: c_int = 2;
/// `AF_INET6`: 10 on Linux, 30 on Apple, 23 on Windows.
#[cfg(target_os = "linux")]
pub(crate) const AF_INET6: c_int = 10;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const AF_INET6: c_int = 30;
#[cfg(windows)]
pub(crate) const AF_INET6: c_int = 23;

/// The address-family field type (`sa_family_t`): u8 on Apple (after the
/// length byte), u16 elsewhere.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) type Family = u8;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub(crate) type Family = u16;

/// `AF_INET6` as the family field value (same numbers as [`AF_INET6`]).
#[cfg(target_os = "linux")]
pub(crate) const AF_INET6_FAMILY: Family = 10;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const AF_INET6_FAMILY: Family = 30;
#[cfg(windows)]
pub(crate) const AF_INET6_FAMILY: Family = 23;

/// Opaque `struct sockaddr` (only ever passed by pointer).
#[repr(C)]
pub(crate) struct Sockaddr {
    _opaque: [u8; 0],
}

/// `struct sockaddr_in6` (28 bytes on every supported platform).
#[allow(clippy::struct_field_names)] // C field names, kept verbatim
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SockaddrIn6 {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(crate) sin6_len: u8,
    pub(crate) sin6_family: Family,
    pub(crate) sin6_port: u16,
    pub(crate) sin6_flowinfo: u32,
    pub(crate) sin6_addr: In6Addr,
    pub(crate) sin6_scope_id: u32,
}

/// `struct in6_addr`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct In6Addr {
    pub(crate) s6_addr: [u8; 16],
}

/// `struct sockaddr_in` (16 bytes).
#[allow(clippy::struct_field_names)] // C field names, kept verbatim
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SockaddrIn {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(crate) sin_len: u8,
    pub(crate) sin_family: Family,
    pub(crate) sin_port: u16,
    pub(crate) sin_addr: InAddr,
    pub(crate) sin_zero: [u8; 8],
}

/// `struct in_addr`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct InAddr {
    pub(crate) s_addr: u32,
}

/// `struct sockaddr_storage` (128 bytes, 8-byte aligned).
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(crate) struct SockaddrStorage {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(crate) ss_len: u8,
    pub(crate) ss_family: Family,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    _pad: [u8; 126],
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    _pad: [u8; 126],
}

#[cfg(test)]
mod tests {
    use super::{SockaddrIn, SockaddrIn6, SockaddrStorage};

    #[test]
    fn family_constants_agree() {
        assert_eq!(i32::from(super::AF_INET6_FAMILY), super::AF_INET6);
    }

    #[test]
    fn layouts_match_the_platform_abi() {
        assert_eq!(std::mem::size_of::<SockaddrIn6>(), 28);
        assert_eq!(std::mem::size_of::<SockaddrIn>(), 16);
        assert_eq!(std::mem::size_of::<SockaddrStorage>(), 128);
        assert_eq!(std::mem::align_of::<SockaddrStorage>(), 8);
    }

    #[cfg(unix)]
    #[test]
    fn layouts_equal_libc_on_unix() {
        assert_eq!(
            std::mem::size_of::<SockaddrIn6>(),
            std::mem::size_of::<libc::sockaddr_in6>()
        );
        assert_eq!(
            std::mem::size_of::<SockaddrIn>(),
            std::mem::size_of::<libc::sockaddr_in>()
        );
        assert_eq!(
            std::mem::size_of::<SockaddrStorage>(),
            std::mem::size_of::<libc::sockaddr_storage>()
        );
        assert_eq!(i32::from(super::AF_INET6_FAMILY), libc::AF_INET6);
        assert_eq!(super::AF_INET, libc::AF_INET);
    }
}
