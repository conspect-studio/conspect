//! Hardware smoke check: load the host libsrt and bind a dual-stack SRT
//! listener on an ephemeral port (`[::]:0`), then drop it.
#![allow(clippy::print_stdout)]

use multiview_srt_sys::{ListenerOptions, SrtLib, SrtListener};

fn main() {
    match SrtLib::shared() {
        Ok(_) => println!("libsrt loaded"),
        Err(e) => {
            println!("LOAD_FAILED: {e}");
            std::process::exit(1);
        }
    }
    match SrtListener::bind(
        std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
        &ListenerOptions::sender(),
        4,
    ) {
        Ok(l) => println!("listening on {}", l.local_addr()),
        Err(e) => {
            println!("BIND_FAILED: {e}");
            std::process::exit(1);
        }
    }
}
