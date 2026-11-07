//! A minimal anonymous passive-mode FTP `RETR` client for the public BOM
//! product server (`ftp.bom.gov.au`, path `/anon/gen/fwo/<product>.xml`).
//!
//! Management-plane only (tokio). Bounded everywhere: a connect timeout, a
//! per-read idle timeout, an overall deadline, a reply-line cap, and a
//! maximum transfer size. Passive mode tries `EPSV` first then falls back to
//! `PASV`; the data connection always dials the **control connection's peer
//! address** (the host in a `PASV` reply is ignored — no FTP-bounce / SSRF).

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Hard cap on one product transfer (8 MiB).
pub const MAX_PRODUCT_BYTES: usize = 8 * 1024 * 1024;

/// Bounds for one transfer.
#[derive(Clone, Copy, Debug)]
pub struct FtpLimits {
    /// TCP connect timeout (control and data).
    pub connect: Duration,
    /// Idle timeout for any single read.
    pub read: Duration,
    /// Overall deadline for the whole exchange.
    pub total: Duration,
    /// Maximum payload bytes.
    pub max_bytes: usize,
}

impl Default for FtpLimits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            read: Duration::from_secs(20),
            total: Duration::from_secs(90),
            max_bytes: MAX_PRODUCT_BYTES,
        }
    }
}

/// A transfer failure (stringly typed for the freshness report).
#[derive(Debug, thiserror::Error)]
#[error("ftp: {0}")]
pub struct FtpError(pub String);

fn err(msg: impl Into<String>) -> FtpError {
    FtpError(msg.into())
}

/// Maximum bytes of one reply line (the BOM banner lines are ~80 bytes).
const MAX_LINE: usize = 1024;
/// Maximum lines in one multi-line reply (the BOM banner is ~40 lines).
const MAX_REPLY_LINES: usize = 256;

struct Control {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    read_timeout: Duration,
}

impl Control {
    async fn line(&mut self) -> Result<String, FtpError> {
        let mut buf = Vec::new();
        let n = timeout(
            self.read_timeout,
            (&mut self.reader)
                .take(u64::try_from(MAX_LINE).unwrap_or(1024))
                .read_until(b'\n', &mut buf),
        )
        .await
        .map_err(|_| err("control read timed out"))?
        .map_err(|e| err(format!("control read: {e}")))?;
        if n == 0 {
            return Err(err("control connection closed"));
        }
        if buf.last() != Some(&b'\n') {
            return Err(err("control reply line too long"));
        }
        Ok(String::from_utf8_lossy(&buf).trim_end().to_owned())
    }

    /// Read one (possibly multi-line) reply: `(code, last line)`.
    async fn reply(&mut self) -> Result<(u16, String), FtpError> {
        let first = self.line().await?;
        let code = first
            .get(..3)
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| err(format!("malformed reply {first:?}")))?;
        if first.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            for _ in 0..MAX_REPLY_LINES {
                let line = self.line().await?;
                if line.starts_with(&end) || line == end.trim_end() {
                    return Ok((code, line));
                }
            }
            return Err(err("multi-line reply too long"));
        }
        Ok((code, first))
    }

    async fn cmd(&mut self, cmd: &str) -> Result<(u16, String), FtpError> {
        timeout(
            self.read_timeout,
            self.writer.write_all(format!("{cmd}\r\n").as_bytes()),
        )
        .await
        .map_err(|_| err("control write timed out"))?
        .map_err(|e| err(format!("control write: {e}")))?;
        self.reply().await
    }

    async fn expect(&mut self, cmd: &str, ok: impl Fn(u16) -> bool) -> Result<String, FtpError> {
        let (code, line) = self.cmd(cmd).await?;
        if ok(code) {
            Ok(line)
        } else {
            let verb = cmd.split(' ').next().unwrap_or(cmd);
            Err(err(format!("{verb} refused: {line}")))
        }
    }
}

/// Parse an `EPSV` reply's port: `229 Entering Extended Passive Mode (|||6446|)`.
#[must_use]
pub fn parse_epsv(line: &str) -> Option<u16> {
    let open = line.find('(')?;
    let inner = line.get(open + 1..)?;
    let close = inner.find(')')?;
    let body = inner.get(..close)?;
    let delim = body.chars().next()?;
    let parts: Vec<&str> = body.split(delim).collect();
    // `|||port|` splits into ["", "", "", "port", ""].
    parts.get(3)?.parse().ok()
}

/// Parse a `PASV` reply's port: `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`.
/// The host octets are deliberately discarded.
#[must_use]
pub fn parse_pasv(line: &str) -> Option<u16> {
    let open = line.find('(')?;
    let inner = line.get(open + 1..)?;
    let close = inner.find(')')?;
    let nums: Vec<u16> = inner
        .get(..close)?
        .split(',')
        .map(|n| n.trim().parse::<u16>().ok())
        .collect::<Option<_>>()?;
    if nums.len() != 6 || nums.iter().any(|n| *n > 255) {
        return None;
    }
    Some(nums.get(4)? * 256 + nums.get(5)?)
}

async fn connect(addr: SocketAddr, limit: Duration) -> Result<TcpStream, FtpError> {
    timeout(limit, TcpStream::connect(addr))
        .await
        .map_err(|_| err(format!("connect {addr} timed out")))?
        .map_err(|e| err(format!("connect {addr}: {e}")))
}

/// Retrieve `path` from `host:port` anonymously in binary passive mode.
///
/// # Errors
/// Any connect/protocol/timeout failure, or a payload over `limits.max_bytes`.
pub async fn retr(
    host: &str,
    port: u16,
    path: &str,
    limits: FtpLimits,
) -> Result<Vec<u8>, FtpError> {
    timeout(limits.total, retr_inner(host, port, path, limits))
        .await
        .map_err(|_| err("transfer deadline exceeded"))?
}

async fn retr_inner(
    host: &str,
    port: u16,
    path: &str,
    limits: FtpLimits,
) -> Result<Vec<u8>, FtpError> {
    if path.contains(['\r', '\n']) {
        return Err(err("invalid path"));
    }
    let addrs: Vec<SocketAddr> = timeout(limits.connect, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| err("dns timed out"))?
        .map_err(|e| err(format!("dns {host}: {e}")))?
        .collect();
    let mut last = err(format!("no address for {host}"));
    let mut stream = None;
    for addr in addrs {
        match connect(addr, limits.connect).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let stream = stream.ok_or(last)?;
    let peer = stream
        .peer_addr()
        .map_err(|e| err(format!("peer addr: {e}")))?;
    let (r, w) = stream.into_split();
    let mut ctl = Control {
        reader: BufReader::new(r),
        writer: w,
        read_timeout: limits.read,
    };
    let (code, line) = ctl.reply().await?;
    if code != 220 {
        return Err(err(format!("greeting: {line}")));
    }
    let (code, line) = ctl.cmd("USER anonymous").await?;
    match code {
        230 => {}
        331 => {
            ctl.expect("PASS anonymous@", |c| c == 230 || c == 202)
                .await?;
        }
        _ => return Err(err(format!("USER refused: {line}"))),
    }
    ctl.expect("TYPE I", |c| c == 200).await?;
    let data_port = if let (229, line) = ctl.cmd("EPSV").await? {
        parse_epsv(&line).ok_or_else(|| err(format!("bad EPSV reply {line:?}")))?
    } else {
        let line = ctl.expect("PASV", |c| c == 227).await?;
        parse_pasv(&line).ok_or_else(|| err(format!("bad PASV reply {line:?}")))?
    };
    let mut data = connect(SocketAddr::new(peer.ip(), data_port), limits.connect).await?;
    ctl.expect(&format!("RETR {path}"), |c| c == 150 || c == 125)
        .await?;
    let mut out = Vec::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let n = timeout(limits.read, data.read(&mut chunk))
            .await
            .map_err(|_| err("data read timed out"))?
            .map_err(|e| err(format!("data read: {e}")))?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > limits.max_bytes {
            return Err(err(format!("payload exceeds {} bytes", limits.max_bytes)));
        }
        out.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
    drop(data);
    let (code, line) = ctl.reply().await?;
    if code != 226 && code != 250 {
        return Err(err(format!("transfer not confirmed: {line}")));
    }
    let _ = ctl.cmd("QUIT").await;
    Ok(out)
}

/// The BOM product path for a product id (`/anon/gen/fwo/IDN11060.xml`).
#[must_use]
pub fn bom_product_path(product: &str) -> String {
    format!("/anon/gen/fwo/{product}.xml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_passive_replies() {
        assert_eq!(
            parse_epsv("229 Entering Extended Passive Mode (|||6446|)"),
            Some(6446)
        );
        assert_eq!(
            parse_pasv("227 Entering Passive Mode (134,178,253,145,19,137)"),
            Some(19 * 256 + 137)
        );
        assert_eq!(parse_pasv("227 nope"), None);
        assert_eq!(parse_epsv("229 (|||x|)"), None);
    }

    /// A scripted single-session FTP server on `[::1]`.
    async fn fake_server(payload: Vec<u8>, epsv: bool) -> u16 {
        let ctl = TcpListener::bind("[::1]:0").await.unwrap();
        let port = ctl.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (sock, _) = ctl.accept().await.unwrap();
            let (r, mut w) = sock.into_split();
            let mut r = BufReader::new(r);
            w.write_all(b"220-Welcome\r\n220-disclaimer\r\n220 \r\n")
                .await
                .unwrap();
            let data = TcpListener::bind("[::1]:0").await.unwrap();
            let dport = data.local_addr().unwrap().port();
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).await.unwrap() == 0 {
                    return;
                }
                let cmd = line.trim_end().to_owned();
                let reply = if cmd.starts_with("USER") {
                    "331 pw\r\n".to_owned()
                } else if cmd.starts_with("PASS") {
                    "230 ok\r\n".to_owned()
                } else if cmd.starts_with("TYPE") {
                    "200 binary\r\n".to_owned()
                } else if cmd == "EPSV" {
                    if epsv {
                        format!("229 Entering Extended Passive Mode (|||{dport}|)\r\n")
                    } else {
                        "500 no\r\n".to_owned()
                    }
                } else if cmd == "PASV" {
                    // Advertise a bogus host: the client must ignore it.
                    format!(
                        "227 Entering Passive Mode (10,9,8,7,{},{})\r\n",
                        dport / 256,
                        dport % 256
                    )
                } else if cmd.starts_with("RETR") {
                    w.write_all(b"150 opening\r\n").await.unwrap();
                    let (mut d, _) = data.accept().await.unwrap();
                    d.write_all(&payload).await.unwrap();
                    drop(d);
                    "226 done\r\n".to_owned()
                } else {
                    w.write_all(b"221 bye\r\n").await.unwrap();
                    return;
                };
                w.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        port
    }

    #[tokio::test]
    async fn retrieves_over_epsv_and_pasv() {
        for epsv in [true, false] {
            let port = fake_server(b"<product/>".to_vec(), epsv).await;
            let got = retr(
                "::1",
                port,
                "/anon/gen/fwo/IDN11060.xml",
                FtpLimits::default(),
            )
            .await
            .unwrap();
            assert_eq!(got, b"<product/>");
        }
    }

    #[tokio::test]
    async fn enforces_max_bytes() {
        let port = fake_server(vec![b'x'; 4096], true).await;
        let limits = FtpLimits {
            max_bytes: 1000,
            ..FtpLimits::default()
        };
        assert!(retr("::1", port, "/x", limits).await.is_err());
    }

    /// Live check against ftp.bom.gov.au, only when `BOM_LIVE_FTP=1`.
    #[tokio::test]
    async fn live_bom_fetch_when_enabled() {
        if std::env::var("BOM_LIVE_FTP").as_deref() != Ok("1") {
            return;
        }
        let bytes = retr(
            "ftp.bom.gov.au",
            21,
            &bom_product_path("IDN11060"),
            FtpLimits::default(),
        )
        .await
        .unwrap();
        let f = crate::parse_forecast_with_limits(
            &bytes,
            crate::Limits {
                max_bytes: MAX_PRODUCT_BYTES,
                max_nodes: 400_000,
                ..crate::Limits::default()
            },
        )
        .unwrap();
        let s = crate::normalize_forecast(&f, "NSW_PT131").unwrap();
        assert_eq!(s.days.len(), 7);
    }
}
