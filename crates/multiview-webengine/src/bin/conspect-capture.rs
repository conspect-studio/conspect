//! Internal one-job service worker: JSON request on stdin, opaque artifact on stdout.
//! This is not a public API. Its caller must authenticate provenance before launch.
#[cfg(unix)]
mod unix_main {
    use multiview_webengine::{
        artifact::{ArtifactStore, Limits, Provenance},
        capture::{Capture, Tools},
        runner::{run_capture, Control},
    };
    use serde::Deserialize;
    use std::{
        io::{self, Read, Write},
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        root: PathBuf,
        tools: Tools,
        capture: Capture,
        provenance: Provenance,
        maximum_bytes: u64,
        store_bytes: u64,
        store_files: usize,
        timeout_seconds: u64,
    }

    fn run(cancelled: &AtomicBool) -> io::Result<()> {
        let mut bytes = Vec::new();
        let stdin = io::stdin();
        let flags = rustix::fs::fcntl_getfl(&stdin)?;
        rustix::fs::fcntl_setfl(&stdin, flags | rustix::fs::OFlags::NONBLOCK)?;
        let input_deadline = Instant::now() + Duration::from_secs(30);
        let mut input = stdin.lock();
        let mut chunk = [0_u8; 8192];
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "request cancelled",
                ));
            }
            if Instant::now() >= input_deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "request deadline"));
            }
            match input.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(chunk.get(..count).unwrap_or_default()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
            if bytes.len() > 1024 * 1024 {
                return Err(io::Error::other("capture request too large"));
            }
        }
        drop(input);
        if bytes.len() > 1024 * 1024 {
            return Err(io::Error::other("capture request too large"));
        }
        let request: Request = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::other("invalid capture request"))?;
        if !(1..=120).contains(&request.timeout_seconds) || request.maximum_bytes == 0 {
            return Err(io::Error::other("invalid capture limits"));
        }
        let mut store = ArtifactStore::open(
            &request.root,
            Limits {
                bytes: request.store_bytes,
                count: request.store_files,
            },
        )?;
        let artifact = run_capture(
            &mut store,
            request.provenance,
            request.maximum_bytes,
            &request.tools,
            &request.capture,
            Control {
                cancelled,
                deadline: Instant::now() + Duration::from_secs(request.timeout_seconds),
            },
            |_| true,
        )?;
        // The receiver must acquire its own pin before promotion or playback.
        // The completed immutable object remains durable after this worker exits.
        let mut output = io::stdout().lock();
        serde_json::to_writer(&mut output, &artifact)?;
        output.write_all(b"\n")?;
        output.flush()?;
        Ok(())
    }
    pub(super) fn main() {
        let result = (|| -> io::Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let cancelled = Arc::new(AtomicBool::new(false));
            runtime.block_on(async {
            let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let flag = cancelled.clone();
            let mut capture = tokio::task::spawn_blocking(move || run(&flag));
            tokio::select! {
                result = &mut capture => result.map_err(|_| io::Error::other("capture worker failed"))?,
                _ = terminate.recv() => {
                    cancelled.store(true, Ordering::Release);
                    capture.await.map_err(|_| io::Error::other("capture worker failed"))?
                }
                _ = interrupt.recv() => {
                    cancelled.store(true, Ordering::Release);
                    capture.await.map_err(|_| io::Error::other("capture worker failed"))?
                }
            }
        })
        })();
        if result.is_err() {
            // Do not expose paths, URLs, page data or credentials in service logs.
            let _ = io::stderr().write_all(b"capture job failed\n");
            std::process::exit(1);
        }
    }
}

#[cfg(unix)]
fn main() {
    unix_main::main();
}

/// The capture worker needs unix process groups; other platforms refuse.
#[cfg(not(unix))]
fn main() {
    std::process::exit(2);
}
