//! Capture producer to immutable artifact handoff. Management-worker calls only.
//!
//! Encoded bytes are streamed directly into a quota-bound `Pending` writer.
//! A child success marker and a successful exit are both required before commit.
//!
//! The whole transfer is one thread over nonblocking pipes polled against the
//! absolute deadline and the cancellation flag. There are no helper threads to
//! join, so a descendant that escapes the process group (`setsid`) and keeps a
//! pipe open can never stall the caller: after the leader exits, pipes are
//! drained only as far as data is already available.
use crate::{
    artifact::{Artifact, ArtifactId, ArtifactStore, Provenance},
    capture::{Capture, Tools},
    resource::{Monitor, ResourceSample},
};
use rustix::{
    event::{poll, PollFd, PollFlags, Timespec},
    fs::{fcntl_setfl, OFlags},
    process::{kill_process_group, waitid, Pid, Signal, WaitId, WaitIdOptions},
};
use std::{
    io::{self, Read, Write},
    os::unix::{fs::DirBuilderExt, process::CommandExt},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Longest completion line retained from the producer's status stream.
const MAX_STATUS_LINE: usize = 8192;
const CHUNK: usize = 64 * 1024;

/// Owns the producer's process group. The group id is the leader's pid, which
/// stays reserved while the leader is a zombie, so signals are only ever sent
/// before the leader is reaped (`reaped` is set by the single reaping call).
pub(crate) struct ChildGuard {
    pub(crate) child: Child,
    pub(crate) reaped: bool,
}
impl ChildGuard {
    fn pid(&self) -> Option<Pid> {
        Pid::from_raw(self.child.id().try_into().ok()?)
    }
    /// Inspect leader exit WITHOUT reaping it, keeping the pid/pgid reserved.
    pub(crate) fn leader_exited(&self) -> bool {
        !self.reaped
            && self.pid().is_some_and(|pid| {
                matches!(
                    waitid(
                        WaitId::Pid(pid),
                        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT
                    ),
                    Ok(Some(_))
                )
            })
    }
    pub(crate) fn sweep(&self) {
        if !self.reaped {
            if let Some(pid) = self.pid() {
                let _ = kill_process_group(pid, Signal::KILL);
            }
        }
    }
    /// Stop everything in the group, then reap the leader exactly once.
    pub(crate) fn terminate(&mut self) -> Option<std::process::ExitStatus> {
        if self.reaped {
            return None;
        }
        if let Some(pid) = self.pid() {
            if !self.leader_exited() {
                // Give the host a bounded opportunity to close the browser.
                let _ = kill_process_group(pid, Signal::TERM);
                for _ in 0..20 {
                    if self.leader_exited() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            // Leader is still unreaped here, so the group id cannot be reused.
            let _ = kill_process_group(pid, Signal::KILL);
        }
        let status = self.child.wait().ok();
        self.reaped = true;
        status
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Internal producer script, embedded to avoid loading mutable caller scripts.
const HOST: &str = include_str!("browser_host.cjs");

/// Cancellation and absolute deadline shared with the management scheduler.
#[derive(Clone, Copy)]
pub struct Control<'a> {
    /// Raised when the job is cancelled.
    pub cancelled: &'a AtomicBool,
    /// Absolute producer deadline.
    pub deadline: Instant,
}
impl Control<'_> {
    pub(crate) fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "capture cancelled",
            ))
        } else if Instant::now() >= self.deadline {
            Err(io::Error::new(io::ErrorKind::TimedOut, "capture expired"))
        } else {
            Ok(())
        }
    }
}

/// One bounded line assembled from a status stream.
pub(crate) enum Line<'a> {
    /// Complete line (no newline), at most [`MAX_STATUS_LINE`] bytes.
    Text(&'a [u8]),
    /// Line exceeded the bound; only a prefix was retained and it is withheld.
    Oversize,
}

/// Bounded line assembler: memory never exceeds [`MAX_STATUS_LINE`] however
/// noisy the stream. Blank lines are skipped.
#[derive(Default)]
pub(crate) struct LineSplit {
    current: Vec<u8>,
    overflow: bool,
}
impl LineSplit {
    pub(crate) fn push(&mut self, bytes: &[u8], mut on_line: impl FnMut(Line<'_>)) {
        for piece in bytes.split_inclusive(|b| *b == b'\n') {
            let newline = piece.last() == Some(&b'\n');
            let part = piece
                .get(..piece.len() - usize::from(newline))
                .unwrap_or(&[]);
            let room = MAX_STATUS_LINE - self.current.len();
            let keep = part.get(..room).unwrap_or(part);
            self.current.extend_from_slice(keep);
            self.overflow |= part.len() > room;
            if newline {
                self.end_line(&mut on_line);
            }
        }
    }
    /// Flush an unterminated final line.
    pub(crate) fn finish(&mut self, mut on_line: impl FnMut(Line<'_>)) {
        self.end_line(&mut on_line);
    }
    fn end_line(&mut self, on_line: &mut impl FnMut(Line<'_>)) {
        let blank = !self.overflow && self.current.iter().all(u8::is_ascii_whitespace);
        if !blank {
            if self.overflow {
                on_line(Line::Oversize);
            } else {
                on_line(Line::Text(&self.current));
            }
        }
        self.current.clear();
        self.overflow = false;
    }
}

/// Receiver of the producer's status stream (stderr).
pub(crate) trait StatusSink {
    /// Called with each chunk read; must stay bounded in memory.
    fn push(&mut self, bytes: &[u8]);
}

/// Final nonempty status line seen so far.
#[derive(Default)]
enum Last {
    #[default]
    Missing,
    Line(Vec<u8>),
    Oversize,
}

/// Keeps only the last nonempty status line, bounded; the rest is discarded.
#[derive(Default)]
pub(crate) struct LineTail {
    split: LineSplit,
    last: Last,
}
impl StatusSink for LineTail {
    fn push(&mut self, bytes: &[u8]) {
        let last = &mut self.last;
        self.split.push(bytes, |line| record(last, &line));
    }
}
fn record(last: &mut Last, line: &Line<'_>) {
    *last = match line {
        Line::Oversize => Last::Oversize,
        Line::Text(t) => Last::Line(t.to_vec()),
    };
}
impl LineTail {
    pub(crate) fn finish(mut self) -> Option<Vec<u8>> {
        let last = &mut self.last;
        self.split.finish(|line| record(last, &line));
        match self.last {
            Last::Line(line) => Some(line),
            _ => None,
        }
    }
}

fn set_nonblocking(fd: &impl std::os::fd::AsFd) -> io::Result<()> {
    fcntl_setfl(fd, OFlags::NONBLOCK).map_err(io::Error::from)
}

/// Read one chunk if available. `Ok(None)` is EOF; `Some(false)` is no data now.
fn read_some(
    stream: &mut impl Read,
    buffer: &mut [u8],
    mut sink: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<Option<bool>> {
    loop {
        return match stream.read(buffer) {
            Ok(0) => Ok(None),
            Ok(n) => sink(buffer.get(..n).unwrap_or_default()).map(|()| Some(true)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Some(false)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Err(e),
        };
    }
}

/// Nonblocking stdin/stdout/stderr transfer state of one producer.
pub(crate) struct Pump<S: StatusSink> {
    request: Vec<u8>,
    written: usize,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    status: Option<ChildStderr>,
    sink: S,
    buffer: Vec<u8>,
}
impl<S: StatusSink> Pump<S> {
    /// Close every pipe and hand back the status sink.
    fn into_sink(self) -> S {
        self.sink
    }
    /// Advance every stream without blocking. Returns whether any data moved.
    pub(crate) fn step(&mut self, pending: &mut impl Write) -> io::Result<bool> {
        let mut moved = false;
        if let Some(stdin) = self.input.as_mut() {
            let rest = self.request.get(self.written..).unwrap_or_default();
            match stdin.write(rest) {
                Ok(n) => {
                    moved |= n > 0;
                    self.written += n;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                // The host closed stdin early; its exit status decides.
                Err(_) => self.written = self.request.len(),
            }
            if self.written >= self.request.len() {
                self.input = None;
            }
        }
        if let Some(stream) = self.output.as_mut() {
            match read_some(stream, &mut self.buffer, |b| pending.write_all(b))? {
                None => self.output = None,
                Some(got) => moved |= got,
            }
        }
        if let Some(stream) = self.status.as_mut() {
            let sink = &mut self.sink;
            match read_some(stream, &mut self.buffer, |b| {
                sink.push(b);
                Ok(())
            })? {
                None => self.status = None,
                Some(got) => moved |= got,
            }
        }
        Ok(moved)
    }
    fn wait_ready(&self) -> io::Result<()> {
        let mut fds = Vec::with_capacity(3);
        if let Some(s) = self.input.as_ref() {
            fds.push(PollFd::new(s, PollFlags::OUT));
        }
        if let Some(s) = self.output.as_ref() {
            fds.push(PollFd::new(s, PollFlags::IN));
        }
        if let Some(s) = self.status.as_ref() {
            fds.push(PollFd::new(s, PollFlags::IN));
        }
        let wait = Timespec {
            tv_sec: 0,
            tv_nsec: 20_000_000,
        };
        match poll(&mut fds, Some(&wait)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Counts bytes forwarded to the pending writer.
pub(crate) struct Counted<'a, W: Write> {
    pub(crate) inner: &'a mut W,
    pub(crate) written: u64,
}
impl<W: Write> Write for Counted<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += u64::try_from(n).unwrap_or(0);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Pump until the leader exits, then drain only data already queued: a
/// descendant that escaped the group and holds a pipe open cannot stall us.
fn transfer<S: StatusSink>(
    guard: &ChildGuard,
    pump: &mut Pump<S>,
    pending: &mut impl Write,
    control: Control<'_>,
    observer: &mut dyn FnMut(ResourceSample),
) -> io::Result<()> {
    let mut exited = false;
    let mut monitor = Monitor::new(guard.child.id());
    loop {
        control.check()?;
        if !exited {
            if let Some(sample) = monitor.as_mut().and_then(|m| m.poll(Instant::now())) {
                observer(sample);
            }
        }
        if !exited && guard.leader_exited() {
            exited = true;
            // Final sample while the zombie leader is still readable and
            // before the group is swept.
            if let Some(m) = monitor.as_mut() {
                observer(m.finish(Instant::now()));
            }
            guard.sweep();
        }
        let moved = pump.step(pending)?;
        if exited {
            if moved {
                continue;
            }
            return Ok(());
        }
        if !moved {
            pump.wait_ready()?;
        }
    }
}

/// Outcome of one finished producer process.
pub(crate) struct ProducerRun<S> {
    /// Whether any media byte reached the pending writer.
    pub(crate) wrote: bool,
    /// Exact media byte count forwarded.
    pub(crate) bytes: u64,
    /// Status sink after the stream ended.
    pub(crate) sink: S,
    /// Reaped leader exit status.
    pub(crate) exit: std::process::ExitStatus,
}

/// Spawn `command` in its own process group with piped stdio, feed `request` on
/// stdin, forward stdout into `pending` and stderr into `sink`, all through the
/// bounded nonblocking pump. The group is always killed and the leader reaped
/// before this returns, on success and on every error path. The caller must
/// set the environment, working directory and arguments; no shell is involved.
pub(crate) fn run_producer<S: StatusSink>(
    mut command: Command,
    request: Vec<u8>,
    sink: S,
    pending: &mut impl Write,
    control: Control<'_>,
    observer: &mut dyn FnMut(ResourceSample),
) -> io::Result<ProducerRun<S>> {
    command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut guard = ChildGuard {
        child: command.spawn()?,
        reaped: false,
    };
    let mut pump = Pump {
        request,
        written: 0,
        input: guard.child.stdin.take(),
        output: guard.child.stdout.take(),
        status: guard.child.stderr.take(),
        sink,
        buffer: vec![0; CHUNK],
    };
    if let Some(s) = &pump.input {
        set_nonblocking(s)?;
    }
    if let Some(s) = &pump.output {
        set_nonblocking(s)?;
    }
    if let Some(s) = &pump.status {
        set_nonblocking(s)?;
    }
    let mut counted = Counted {
        inner: pending,
        written: 0,
    };
    transfer(&guard, &mut pump, &mut counted, control, observer)?;
    let bytes = counted.written;
    let sink = pump.into_sink();
    let exit = guard
        .terminate()
        .ok_or_else(|| io::Error::other("producer wait failed"))?;
    Ok(ProducerRun {
        wrote: bytes > 0,
        bytes,
        sink,
        exit,
    })
}

/// Marker payload of an error returned by the artifact commit itself: the
/// commit may or may not be durable, so the reserved ID needs reconciliation.
#[derive(Debug)]
pub struct CommitUncertain;
impl std::fmt::Display for CommitUncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("artifact commit outcome uncertain")
    }
}
impl std::error::Error for CommitUncertain {}
/// Whether `error` came from the artifact commit (see [`CommitUncertain`]).
#[must_use]
pub fn is_commit_uncertain(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<CommitUncertain>().is_some())
}

/// Run one bounded capture and commit only successful producer output.
///
/// `allow` authorizes the trusted producer identity before any process starts.
/// The caller retains the returned ID and acquires an artifact pin for handoff.
/// The producer runs with a cleared environment, a Rust-owned private
/// profile/home/temp/cwd that is removed only after every child is dead.
/// # Errors
/// Any cancellation (`Interrupted`), expiry (`TimedOut`), quota
/// (`StorageFull`), process, protocol or persistence error prevents success.
pub fn run_capture(
    store: &mut ArtifactStore,
    provenance: Provenance,
    maximum_bytes: u64,
    tools: &Tools,
    capture: &Capture,
    control: Control<'_>,
    allow: impl FnOnce(&Provenance) -> bool,
) -> io::Result<Artifact> {
    run_capture_with_pending(
        store,
        provenance,
        maximum_bytes,
        tools,
        capture,
        control,
        allow,
        |_| Ok(()),
    )
}

/// [`run_capture`] with a reservation hook: `reserved` is called with the
/// artifact ID IMMEDIATELY after the store reserved it and BEFORE any process is
/// spawned or any byte is written. If it fails, nothing runs and the unpublished
/// pending write is dropped. An error returned by the artifact commit carries
/// [`CommitUncertain`] (see [`is_commit_uncertain`]).
/// # Errors
/// As [`run_capture`], plus any error of `reserved`.
#[allow(clippy::too_many_arguments)]
pub fn run_capture_with_pending(
    store: &mut ArtifactStore,
    provenance: Provenance,
    maximum_bytes: u64,
    tools: &Tools,
    capture: &Capture,
    control: Control<'_>,
    allow: impl FnOnce(&Provenance) -> bool,
    reserved: impl FnMut(&ArtifactId) -> io::Result<()>,
) -> io::Result<Artifact> {
    run_capture_observed(
        store,
        provenance,
        maximum_bytes,
        tools,
        capture,
        control,
        allow,
        reserved,
        &mut |_| {},
    )
}

/// [`run_capture_with_pending`] that also reports a [`ResourceSample`] of the
/// producer's whole process tree (descendants included) at 1 Hz from the pump.
/// # Errors
/// As [`run_capture_with_pending`].
#[allow(clippy::too_many_arguments)]
pub fn run_capture_observed(
    store: &mut ArtifactStore,
    provenance: Provenance,
    maximum_bytes: u64,
    tools: &Tools,
    capture: &Capture,
    control: Control<'_>,
    allow: impl FnOnce(&Provenance) -> bool,
    mut reserved: impl FnMut(&ArtifactId) -> io::Result<()>,
    observer: &mut dyn FnMut(ResourceSample),
) -> io::Result<Artifact> {
    capture.validate()?;
    tools.validate()?;
    control.check()?;
    let mut pending = store.begin(provenance, maximum_bytes, allow)?;
    reserved(pending.id())?;
    // Declared before the guard: dropped after every child has been reaped.
    let private = tempfile::Builder::new().prefix("conspect-web-").tempdir()?;
    let mut dirs = std::fs::DirBuilder::new();
    dirs.mode(0o700);
    let (profile, home, temp, cwd) = (
        private.path().join("profile"),
        private.path().join("home"),
        private.path().join("tmp"),
        private.path().join("cwd"),
    );
    for dir in [&profile, &home, &temp, &cwd] {
        dirs.create(dir)?;
    }
    let source = capture.windy.as_ref().map_or_else(
        || serde_json::json!({"kind":"html","document":capture.document}),
        |view| serde_json::json!({"kind":"windy","view":view}),
    );
    let request = serde_json::to_vec(&serde_json::json!({
        "browser":tools.browser, "ffmpeg":tools.ffmpeg, "profile":profile,
        "width":capture.width,"height":capture.height,"fps":capture.fps,"frames":capture.frames,
        "source": source,
        "background": capture.background,
        "gl": tools.gl,
        "capture": capture.effective_capture(),
        "timing": capture.capture.timing, "frame_ms": capture.capture.frame_ms,
        "end_hold_ms": capture.capture.end_hold_ms, "max_steps": capture.capture.max_steps,
    }))?;
    let mut command = Command::new(&tools.node);
    command
        .args([
            "-e",
            &HOST
                .replace("/*WINDY_TEMPLATE*/", include_str!("template_windy.cjs"))
                .replace("/*MOV_COLOR*/", include_str!("mov_color.cjs"))
                .replace("/*STEP_PLAN*/", include_str!("step_plan.cjs")),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("TMPDIR", &temp)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .current_dir(&cwd);
    let run = run_producer(
        command,
        request,
        LineTail::default(),
        &mut pending,
        control,
        observer,
    )?;
    drop(private);
    let (wrote, tail, exit) = (run.wrote, run.sink, run.exit);
    if !exit.success() {
        return Err(io::Error::other("capture producer failed"));
    }
    let line = tail
        .finish()
        .ok_or_else(|| io::Error::other("capture completion missing"))?;
    let marker: serde_json::Value = serde_json::from_slice(&line)
        .map_err(|_| io::Error::other("capture completion missing"))?;
    // Realtime writes exactly `frames`; steps writes a producer-derived count
    // that is still bounded by the frame cap and reports at least two kept steps.
    let frames = marker.get("frames").and_then(serde_json::Value::as_u64);
    let frames_ok = match capture.effective_capture() {
        crate::capture::CaptureMode::Realtime => frames == Some(u64::from(capture.frames)),
        crate::capture::CaptureMode::Steps => {
            frames.is_some_and(|f| (2..=u64::from(crate::capture::MAX_FRAMES)).contains(&f))
                && marker
                    .get("steps_kept")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|k| k >= 2)
        }
    };
    if marker.get("event").and_then(serde_json::Value::as_str) != Some("capture_complete")
        || !frames_ok
        || (capture.background == crate::capture::Background::Transparent
            && marker.get("alpha").and_then(serde_json::Value::as_bool) != Some(true))
    {
        return Err(io::Error::other("capture completion invalid"));
    }
    control.check()?;
    // With no byte written `complete` refuses before touching storage, so only
    // a commit that had content can be uncertain.
    pending.complete().map_err(|e| {
        if wrote {
            io::Error::new(e.kind(), CommitUncertain)
        } else {
            e
        }
    })
}
