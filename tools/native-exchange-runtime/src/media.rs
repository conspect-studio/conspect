//! FFmpeg CPU decoder observer. This does not claim device or QUIC coverage.
use crate::Flush;
use std::{collections::VecDeque, fs, path::Path, process::Command};
const CAP: usize = 4;
#[derive(Default)]
pub struct Output {
    encoded: VecDeque<u64>,
    decoded: VecDeque<u64>,
    render: VecDeque<u64>,
    audio: VecDeque<u64>,
    high_water: usize,
    dropped: usize,
    flushed: usize,
}
impl Output {
    fn push(q: &mut VecDeque<u64>, pts: u64) -> usize {
        let dropped = usize::from(q.len() == CAP);
        if dropped != 0 {
            q.pop_front();
        }
        q.push_back(pts);
        dropped
    }
    pub fn enqueue_encoded(&mut self, pts: u64) {
        self.dropped += Self::push(&mut self.encoded, pts);
        self.high_water = self.high_water.max(self.encoded.len());
    }
    pub fn enqueue_decoded(&mut self, pts: u64, audio: bool) {
        self.dropped += Self::push(&mut self.decoded, pts);
        self.dropped += Self::push(
            if audio {
                &mut self.audio
            } else {
                &mut self.render
            },
            pts,
        );
        self.high_water = self.high_water.max(self.decoded.len());
    }
    pub fn pending(&self) -> usize {
        self.encoded.len() + self.decoded.len() + self.render.len() + self.audio.len()
    }
}
/// Encrypted objects whose epoch grant is not accepted yet. Bounded: the oldest
/// is dropped on overflow and every drop is counted. It holds ciphertext only.
pub struct ParkQueue<T> {
    queue: VecDeque<T>,
    cap: usize,
    pub dropped: usize,
    pub parked_total: usize,
    pub drained: usize,
    pub stale: usize,
}
impl<T> ParkQueue<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            cap,
            dropped: 0,
            parked_total: 0,
            drained: 0,
            stale: 0,
        }
    }
    /// Park in order; at capacity the oldest item is dropped, counted and
    /// returned so the caller can account for the loss.
    pub fn park(&mut self, item: T) -> Option<T> {
        let dropped = if self.queue.len() >= self.cap {
            self.dropped += 1;
            self.queue.pop_front()
        } else {
            None
        };
        self.queue.push_back(item);
        self.parked_total += 1;
        dropped
    }
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.queue.iter()
    }
    /// Put an item back at the tail without counting it as newly parked.
    pub fn requeue(&mut self, item: T) {
        self.queue.push_back(item);
    }
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
    pub fn take_all(&mut self) -> Vec<T> {
        self.queue.drain(..).collect()
    }
    pub fn clear(&mut self) {
        self.queue.clear();
    }
}
impl Flush for Output {
    fn flush(&mut self) {
        self.flushed += self.pending();
        self.encoded.clear();
        self.decoded.clear();
        self.render.clear();
        self.audio.clear();
    }
}
type Result<T> = std::result::Result<T, &'static str>;
fn command(args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new(args[0])
        .args(&args[1..])
        .output()
        .map_err(|_| "FFmpeg command unavailable")?;
    if !out.status.success() {
        // Do not include command arguments or subprocess output in errors.
        return Err("FFmpeg command failed");
    }
    Ok(out)
}
pub struct Fixture {
    pub name: String,
    pub format: &'static str,
    pub packets: Vec<Vec<u8>>,
}
pub fn fixture(dir: &Path, audio: Option<usize>) -> Result<Fixture> {
    let name = audio.map_or_else(|| "video".into(), |i| format!("audio-{i}"));
    let format = if audio.is_some() { "aac" } else { "hevc" };
    let path = dir.join(format!("{name}.{format}"));
    let path_str = path.to_str().ok_or("media path")?;
    if let Some(i) = audio {
        command(&[
            "ffmpeg",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency={}:sample_rate=48000", 400 + i * 100),
            "-t",
            "1.0",
            "-c:a",
            "aac",
            "-threads",
            "1",
            "-b:a",
            "48k",
            "-f",
            "adts",
            path_str,
        ])?;
    } else {
        command(&[
            "ffmpeg",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x64:rate=10",
            "-t",
            "1.0",
            "-c:v",
            "libx265",
            "-threads",
            "1",
            "-x265-params",
            "pools=none:frame-threads=1:keyint=1:repeat-headers=1:log-level=error",
            "-f",
            "hevc",
            path_str,
        ])?;
    }
    let probe = command(&[
        "ffprobe",
        "-v",
        "error",
        "-show_packets",
        "-show_entries",
        "packet=pos,size",
        "-of",
        "csv=p=0",
        path_str,
    ])?;
    let file = fs::read(&path).map_err(|_| "fixture read")?;
    let packets = String::from_utf8(probe.stdout)
        .map_err(|_| "packet probe UTF8")?
        .lines()
        .map(|line| {
            let (size, pos) = line.split_once(',').ok_or("packet fields")?;
            let size = size.parse::<usize>().map_err(|_| "packet size")?;
            let pos = pos.parse::<usize>().map_err(|_| "packet position")?;
            let end = pos.checked_add(size).ok_or("packet overflow")?;
            Ok(file.get(pos..end).ok_or("packet bounds")?.to_vec())
        })
        .collect::<Result<Vec<_>>>()?;
    if packets.len() < 8 || packets.iter().map(Vec::len).sum::<usize>() != file.len() {
        return Err("fixture access units");
    }
    Ok(Fixture {
        name,
        format,
        packets,
    })
}
// Timestamp input comes ONLY from the actual FFmpeg decoder filter, never packet arrival.
#[derive(Clone, Copy, Debug)]
pub struct DecodedFrame {
    pub pts_ms: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<(u32, u32)>,
}
pub fn decode(
    dir: &Path,
    fixture: &Fixture,
    taker: usize,
    bytes: &[u8],
) -> Result<Vec<DecodedFrame>> {
    let path = dir.join(format!(
        "decoded-input-{taker}-{}.{}",
        fixture.name, fixture.format
    ));
    fs::write(&path, bytes).map_err(|_| "decode input write")?;
    let mut args = vec![
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "info",
        "-threads",
        "1",
    ];
    args.extend([
        "-f",
        fixture.format,
        "-i",
        path.to_str().ok_or("decode path")?,
    ]);
    if fixture.format == "hevc" {
        args.extend(["-vf", "showinfo"]);
    } else {
        args.extend(["-af", "ashowinfo"]);
    }
    args.extend(["-f", "null", "-"]);
    let out = command(&args)?;
    let log = String::from_utf8_lossy(&out.stderr);
    let fps = log.lines().find_map(|l| {
        let value = l.split("frame_rate:").nth(1)?.split_whitespace().next()?;
        let (n, d) = value.split_once('/')?;
        let (n, d): (u32, u32) = (n.parse().ok()?, d.parse().ok()?);
        (n > 0 && d > 0).then_some((n, d))
    });
    let times: Vec<DecodedFrame> = log
        .lines()
        .filter(|l| l.contains("showinfo") && l.contains("pts_time:"))
        .map(|l| {
            let time = l
                .split("pts_time:")
                .nth(1)
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0 && *v < u64::MAX as f64 / 1000.0)
                .ok_or("decoded timestamp")?;
            let size = l
                .split(" s:")
                .nth(1)
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.split_once('x'))
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
            Ok(DecodedFrame {
                pts_ms: (time * 1000.0).round() as u64,
                width: size.map(|s| s.0),
                height: size.map(|s| s.1),
                fps,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if times.len() < 3 || !times.windows(2).all(|w| w[1].pts_ms > w[0].pts_ms) {
        return Err("insufficient advancing decoded frames");
    }
    Ok(times)
}

#[cfg(test)]
mod decode_input_tests {
    use super::{decode, Fixture};
    /// The empty-input skip in the runtime is only for zero accepted bytes. A
    /// non-empty but malformed decoder input is still a hard decode failure.
    #[test]
    fn nonempty_malformed_input_still_fails() {
        let dir = tempfile::tempdir().unwrap();
        for format in ["hevc", "aac"] {
            let fixture = Fixture {
                name: format!("garbage-{format}"),
                format,
                packets: vec![],
            };
            let garbage = [0x5au8; 4096];
            assert!(
                decode(dir.path(), &fixture, 1, &garbage).is_err(),
                "{format}"
            );
        }
    }
}

#[cfg(test)]
mod park_tests {
    use super::ParkQueue;
    #[test]
    fn park_queue_drops_oldest_and_counts() {
        let mut q = ParkQueue::new(2);
        let dropped: Vec<_> = (0..5).filter_map(|n| q.park(n)).collect();
        assert_eq!(dropped, vec![0, 1, 2]);
        assert_eq!((q.dropped, q.parked_total, q.len()), (3, 5, 2));
        assert_eq!(q.take_all(), vec![3, 4]);
    }
}
