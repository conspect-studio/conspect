//! The crate's only `unsafe`: two Linux `setsockopt` calls that neither std
//! nor socket2 expose. Each is a safe function over a borrowed descriptor; the
//! pointers handed to the kernel point at locals that outlive the call, and the
//! kernel copies what it needs before returning. No callback crosses the FFI
//! boundary, so a Rust panic can never unwind through it.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use crate::steering::Insn;

/// `SO_ATTACH_REUSEPORT_CBPF`: attach a classic BPF program to the reuseport
/// group `fd` belongs to. The program's return value is the index of the
/// group member that receives the datagram. The kernel copies and validates
/// the program (`bpf_check_classic`) and swaps it in under RCU, so a
/// concurrent receive sees either the old or the new program.
///
/// # Errors
///
/// The `setsockopt` error (`EINVAL` for a program the kernel rejects), or
/// `InvalidInput` for an empty program or one longer than `u16::MAX`.
#[allow(unsafe_code)]
pub(crate) fn attach_reuseport_cbpf(fd: BorrowedFd<'_>, program: &[Insn]) -> io::Result<()> {
    let mut filters: Vec<libc::sock_filter> = program
        .iter()
        .map(|i| libc::sock_filter {
            code: i.code,
            jt: i.jt,
            jf: i.jf,
            k: i.k,
        })
        .collect();
    if filters.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty program"));
    }
    let len = u16::try_from(filters.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "program too long"))?;
    let fprog = libc::sock_fprog {
        len,
        filter: filters.as_mut_ptr(),
    };
    let optlen = libc::socklen_t::try_from(std::mem::size_of::<libc::sock_fprog>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sock_fprog size"))?;
    // SAFETY: `fd` is a live descriptor borrowed for the duration of the call.
    // `fprog` is a valid `sock_fprog` on this stack frame whose `filter` points
    // at `filters`, a live allocation of exactly `len` `sock_filter`s that is
    // not moved or dropped until after the call; `optlen` is its exact size.
    // The kernel only reads (copies) both during the call and keeps no pointer.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ATTACH_REUSEPORT_CBPF,
            std::ptr::from_ref(&fprog).cast::<libc::c_void>(),
            optlen,
        )
    };
    drop(filters);
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `SO_RCVBUFFORCE`: set the receive buffer past `net.core.rmem_max`. Needs
/// `CAP_NET_ADMIN`; without it the kernel answers `EPERM` and the caller falls
/// back to `SO_RCVBUF`.
///
/// # Errors
///
/// The `setsockopt` error, or `InvalidInput` when `bytes` exceeds `c_int`.
#[allow(unsafe_code)]
pub(crate) fn set_recv_buffer_force(fd: BorrowedFd<'_>, bytes: usize) -> io::Result<()> {
    let value = libc::c_int::try_from(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rcvbuf too large"))?;
    let optlen = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "c_int size"))?;
    // SAFETY: `fd` is a live descriptor borrowed for the call; the option value
    // points at `value`, a `c_int` on this stack frame, and `optlen` is its
    // exact size. The kernel reads it during the call only.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUFFORCE,
            std::ptr::from_ref(&value).cast::<libc::c_void>(),
            optlen,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
