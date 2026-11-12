//! `multiview tls ...` (ADR-0107 section 5a): certificate status and recovery
//! from the host, without a browser.
//!
//! Every online command talks plain HTTP/1.1 to the running engine's
//! `[control] listen` over **loopback only** (an unspecified `[::]`/`0.0.0.0`
//! listen is dialled on `[::1]`/`127.0.0.1`; any other address is refused),
//! with the admin bearer from `MULTIVIEW_CONTROL_TOKEN`. When the engine is
//! not running, `status` reads `state_dir/active.pem` offline and
//! `set-credential` writes `state_dir/secrets/<NAME>` (0600) directly.
//!
//! A credential value is read from stdin only (never argv), with echo off when
//! stdin is a terminal, and is never printed.

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use multiview_config::MultiviewConfig;

use crate::cli::{TlsCommand, TlsSetCredentialArgs, TlsStatusArgs};

/// Connect / read deadline for the loopback admin listener.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest response accepted from the admin listener.
const RESPONSE_CAP: u64 = 256 * 1024;

/// A `[network.tls]` config in a build without the `acme` feature (or on a
/// non-unix platform) cannot provide certificates: refuse at startup and in
/// `multiview validate`, mirroring the `[control.tls]`-without-`tls` error.
///
/// # Errors
/// The startup refusal message.
pub fn ensure_acme_supported(config: &MultiviewConfig) -> Result<(), String> {
    let configured = config.network.as_ref().is_some_and(|n| n.tls.is_some());
    if !configured || cfg!(all(feature = "acme", unix)) {
        return Ok(());
    }
    Err(
        "network.tls is configured but this `multiview` build lacks the `acme` feature (or \
         this platform is not unix); rebuild with --features acme (or a preset), or remove \
         [network.tls]"
            .to_owned(),
    )
}

/// Run one `multiview tls` subcommand; returns the text for stdout (the
/// binary prints it; this library never prints).
///
/// # Errors
/// An unreadable or invalid config, a refused (non-loopback) listener, or a
/// failed request.
pub fn run(command: &TlsCommand) -> anyhow::Result<String> {
    match command {
        TlsCommand::Status(args) => status(args),
        TlsCommand::SetupCode(args) => {
            let config = load(&args.config)?;
            let admin = AdminClient::from_config(&config)?;
            let (code, body) = admin.post("/api/v1/tls/setup-code", None)?;
            if code != 200 {
                anyhow::bail!("setup-code refused: HTTP {code}: {}", problem_title(&body));
            }
            let json: serde_json::Value = serde_json::from_slice(&body)?;
            Ok(format!(
                "{}\n(one-time setup code: valid 1 hour, single use)",
                json.get("code")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
            ))
        }
        TlsCommand::SetCredential(args) => set_credential(args),
        TlsCommand::Issue(args) => {
            let config = load(&args.config)?;
            issue(&AdminClient::from_config(&config)?)
        }
    }
}

fn load(path: &Path) -> anyhow::Result<MultiviewConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    MultiviewConfig::load_from_toml(&text).with_context(|| format!("parsing {}", path.display()))
}

fn tls_config(config: &MultiviewConfig) -> anyhow::Result<&multiview_config::NetworkTlsConfig> {
    config
        .network
        .as_ref()
        .and_then(|n| n.tls.as_ref())
        .ok_or_else(|| anyhow::anyhow!("this config has no [network.tls] section"))
}

/// The loopback address to dial for a `[control] listen` value, or an error
/// for a non-loopback, non-unspecified address.
///
/// # Errors
/// The listen address does not parse or is neither loopback nor unspecified.
pub fn loopback_target(listen: &str) -> anyhow::Result<SocketAddr> {
    let addr: SocketAddr = listen
        .parse()
        .with_context(|| format!("[control] listen {listen:?} is not a socket address"))?;
    let ip = match addr.ip() {
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        ip if ip.is_loopback() => ip,
        _ => anyhow::bail!(
            "[control] listen {listen} is not a loopback address: `multiview tls` only talks to \
             the engine over loopback"
        ),
    };
    Ok(SocketAddr::new(ip, addr.port()))
}

/// A minimal HTTP/1.1 client for the loopback admin listener.
struct AdminClient {
    addr: SocketAddr,
    token: Option<zeroize_string::Secret>,
}

mod zeroize_string {
    /// A string overwritten on drop (best effort, safe code, no extra
    /// dependency): the buffer is refilled with NULs in place before release.
    pub(super) struct Secret(pub(super) String);
    impl Drop for Secret {
        fn drop(&mut self) {
            let len = self.0.len();
            self.0.clear();
            self.0.extend(std::iter::repeat_n('\0', len));
            std::hint::black_box(&self.0);
        }
    }
}

impl AdminClient {
    fn from_config(config: &MultiviewConfig) -> anyhow::Result<Self> {
        let control = config
            .control
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this config has no [control] section to talk to"))?;
        let addr = loopback_target(&control.listen)?;
        let token = std::env::var("MULTIVIEW_CONTROL_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(zeroize_string::Secret);
        Ok(Self { addr, token })
    }

    fn running(&self) -> bool {
        TcpStream::connect_timeout(&self.addr, Duration::from_secs(2)).is_ok()
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> anyhow::Result<(u16, Vec<u8>)> {
        let mut stream = TcpStream::connect_timeout(&self.addr, IO_TIMEOUT)
            .with_context(|| format!("connecting to the engine on {}", self.addr))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let host = match self.addr {
            SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()),
            SocketAddr::V4(v4) => v4.to_string(),
        };
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: application/json\r\n"
        );
        if let Some(token) = &self.token {
            // A token without the `admin.` key-id prefix is the bare secret.
            let bearer = if token.0.contains('.') {
                token.0.clone()
            } else {
                format!("admin.{}", token.0)
            };
            head.push_str("Authorization: Bearer ");
            head.push_str(&bearer);
            head.push_str("\r\n");
        }
        let body = body.unwrap_or_default();
        let _ = write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut out = zeroize_string::Secret(head);
        stream.write_all(out.0.as_bytes())?;
        stream.write_all(body)?;
        out.0.clear();
        let mut raw = Vec::new();
        stream.take(RESPONSE_CAP).read_to_end(&mut raw)?;
        parse_response(&raw)
    }

    fn get(&self, path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: Option<&[u8]>) -> anyhow::Result<(u16, Vec<u8>)> {
        self.request("POST", path, body)
    }
}

/// Parse a `Connection: close` HTTP/1.1 response (plain or chunked body).
fn parse_response(raw: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed response from the engine"))?;
    let head = std::str::from_utf8(raw.get(..split).unwrap_or_default())?;
    let body = raw.get(split + 4..).unwrap_or_default();
    let code: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("malformed status line from the engine"))?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if !chunked {
        return Ok((code, body.to_vec()));
    }
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow::anyhow!("malformed chunked body"))?;
        let size = usize::from_str_radix(
            std::str::from_utf8(rest.get(..line_end).unwrap_or_default())?
                .split(';')
                .next()
                .unwrap_or("0")
                .trim(),
            16,
        )?;
        rest = rest.get(line_end + 2..).unwrap_or_default();
        if size == 0 {
            return Ok((code, out));
        }
        out.extend_from_slice(
            rest.get(..size)
                .ok_or_else(|| anyhow::anyhow!("truncated chunked body"))?,
        );
        rest = rest.get(size + 2..).unwrap_or_default();
    }
}

fn problem_title(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "no detail".to_owned())
}

fn issue(admin: &AdminClient) -> anyhow::Result<String> {
    let (code, body) = admin.post("/api/v1/tls/issue", None)?;
    if code == 202 {
        Ok("issuance started; follow it with `multiview tls status`".to_owned())
    } else {
        anyhow::bail!("issue refused: HTTP {code}: {}", problem_title(&body))
    }
}

fn status(args: &TlsStatusArgs) -> anyhow::Result<String> {
    let config = load(&args.config.config)?;
    let tls = tls_config(&config)?;
    let online = AdminClient::from_config(&config)
        .ok()
        .filter(AdminClient::running);
    let value = if let Some(admin) = online {
        let (code, body) = admin.get("/api/v1/tls/status")?;
        if code != 200 {
            anyhow::bail!("status refused: HTTP {code}: {}", problem_title(&body));
        }
        let mut value: serde_json::Value = serde_json::from_slice(&body)?;
        if let Some(map) = value.as_object_mut() {
            map.insert("source".into(), "engine".into());
        }
        value
    } else {
        offline_status(tls)?
    };
    if args.json {
        Ok(serde_json::to_string_pretty(&value)?)
    } else {
        Ok(render_status(&value).trim_end().to_owned())
    }
}

/// The status read from `state_dir` when the engine is not running.
///
/// # Errors
/// The stored state is insecure or malformed.
pub fn offline_status(
    tls: &multiview_config::NetworkTlsConfig,
) -> anyhow::Result<serde_json::Value> {
    let challenge = match tls.challenge {
        multiview_config::AcmeChallenge::Dns01 => "dns-01",
        multiview_config::AcmeChallenge::TlsAlpn01 => "tls-alpn-01",
    };
    let mut value = serde_json::json!({
        "source": "offline",
        "state": "setup",
        "staging": tls.staging,
        "challenge": challenge,
        "names": tls.domains,
    });
    #[cfg(all(feature = "acme", unix))]
    {
        let meta = multiview_control::acme::stored_meta(&tls.state_dir).map_err(|e| {
            anyhow::anyhow!(
                "stored certificate in {} unreadable ({})",
                tls.state_dir.display(),
                e.as_reason()
            )
        })?;
        let map = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("internal"))?;
        let mut reasons: Vec<String> = multiview_control::tls_setup::missing_credentials(tls);
        if let Some(meta) = meta {
            let now = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
            )
            .unwrap_or(i64::MAX);
            map.insert(
                "state".into(),
                if meta.not_after <= now {
                    "expired"
                } else {
                    "serving"
                }
                .into(),
            );
            map.insert("expires_at".into(), meta.not_after.into());
            map.insert("names".into(), meta.names.clone().into());
            if !meta.ca.is_empty() {
                map.insert("ca".into(), meta.ca.into());
            }
            reasons.clear();
        } else {
            reasons.push("no certificate stored yet".to_owned());
        }
        map.insert("reasons".into(), reasons.into());
    }
    #[cfg(not(all(feature = "acme", unix)))]
    {
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "reasons".into(),
                vec!["this build lacks the `acme` feature: no stored certificate can be read"]
                    .into(),
            );
        }
    }
    Ok(value)
}

fn field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    value.get(key).filter(|v| !v.is_null())
}

/// `YYYY-MM-DD HH:MM:SSZ` for Unix seconds (proleptic Gregorian, UTC).
#[must_use]
pub fn utc_text(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

fn unix_text(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(serde_json::Value::as_i64)
        .map_or_else(|| "-".to_owned(), utc_text)
}

/// Render the status JSON as the human table.
#[must_use]
pub fn render_status(value: &serde_json::Value) -> String {
    let text = |key: &str| {
        field(value, key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-")
            .to_owned()
    };
    let mut ca = text("ca");
    if field(value, "staging").and_then(serde_json::Value::as_bool) == Some(true) {
        ca.push_str(" (staging)");
    }
    let names = field(value, "names")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let mut out = String::new();
    let source = text("source");
    if source == "offline" {
        out.push_str("engine not running: read from state_dir (offline)\n");
    }
    let rows = [
        ("state", text("state")),
        ("ca", ca),
        ("challenge", text("challenge")),
        ("names", names),
        ("expires", unix_text(field(value, "expires_at"))),
        ("renewal due", unix_text(field(value, "renewal_due_at"))),
        ("next attempt", unix_text(field(value, "next_attempt_at"))),
        ("fingerprint", text("bootstrap_fingerprint")),
        ("alarm", text("alarm")),
        ("last error", text("last_error")),
    ];
    for (k, v) in rows {
        let _ = writeln!(out, "{k:<13} {v}");
    }
    for key in ["reasons", "hints"] {
        if let Some(list) = field(value, key).and_then(serde_json::Value::as_array) {
            for item in list.iter().filter_map(serde_json::Value::as_str) {
                let _ = writeln!(out, "{:<13} {item}", key.trim_end_matches('s'));
            }
        }
    }
    out
}

/// Read a secret from stdin: echo off on a terminal, one line, no trailing
/// newline. Never from argv.
fn read_secret_stdin(name: &str) -> anyhow::Result<zeroize_string::Secret> {
    let stdin = std::io::stdin();
    #[cfg(not(all(unix, feature = "acme")))]
    let _ = writeln!(
        std::io::stderr(),
        "value for {name} (read from stdin; echo cannot be disabled in this build):"
    );
    #[cfg(all(unix, feature = "acme"))]
    let restore = {
        use rustix::termios::{tcgetattr, tcsetattr, LocalModes, OptionalActions};
        match tcgetattr(&stdin) {
            Ok(saved) => {
                let mut err = std::io::stderr();
                let _ = write!(err, "value for {name} (input hidden): ");
                let _ = err.flush();
                let mut quiet = saved.clone();
                quiet.local_modes.remove(LocalModes::ECHO);
                let _ = tcsetattr(&stdin, OptionalActions::Now, &quiet);
                Some(saved)
            }
            Err(_) => None,
        }
    };
    let mut line = zeroize_string::Secret(String::new());
    let read = stdin.read_line(&mut line.0);
    #[cfg(all(unix, feature = "acme"))]
    if let Some(saved) = restore {
        let _ = rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &saved);
        let _ = writeln!(std::io::stderr());
    }
    read.context("reading the value from stdin")?;
    let trimmed = line.0.trim_end_matches(['\r', '\n']).to_owned();
    let value = zeroize_string::Secret(trimmed);
    if value.0.is_empty() {
        anyhow::bail!("empty value on stdin");
    }
    Ok(value)
}

fn set_credential(args: &TlsSetCredentialArgs) -> anyhow::Result<String> {
    let config = load(&args.config.config)?;
    let tls = tls_config(&config)?;
    if !tls.secret_names().contains(&args.name.as_str()) {
        anyhow::bail!(
            "{} is not referenced by [network.tls] (expected one of: {})",
            args.name,
            tls.secret_names().join(", ")
        );
    }
    let value = read_secret_stdin(&args.name)?;
    let online = AdminClient::from_config(&config)
        .ok()
        .filter(AdminClient::running);
    if let Some(admin) = online {
        let body = zeroize_string::Secret(
            serde_json::json!({"name": args.name, "value": value.0}).to_string(),
        );
        let (code, response) = admin.post("/api/v1/tls/credential", Some(body.0.as_bytes()))?;
        if code != 204 {
            anyhow::bail!(
                "credential refused: HTTP {code}: {}",
                problem_title(&response)
            );
        }
        let issued = issue(&admin)?;
        Ok(format!(
            "{} stored in the engine's state directory (0600)\n{issued}",
            args.name
        ))
    } else {
        write_offline(tls, &args.name, value.0.as_bytes())?;
        Ok(format!(
            "{} stored in {}/secrets/ (0600); the engine was not running: it is used at the next start",
            args.name,
            tls.state_dir.display()
        ))
    }
}

#[cfg(all(feature = "acme", unix))]
fn write_offline(
    tls: &multiview_config::NetworkTlsConfig,
    name: &str,
    value: &[u8],
) -> anyhow::Result<()> {
    multiview_control::tls_setup::write_secret(&tls.state_dir, name, value).map_err(|e| {
        anyhow::anyhow!(
            "could not write {}/secrets/{name} ({})",
            tls.state_dir.display(),
            e.as_reason()
        )
    })
}

#[cfg(not(all(feature = "acme", unix)))]
fn write_offline(
    _tls: &multiview_config::NetworkTlsConfig,
    _name: &str,
    _value: &[u8],
) -> anyhow::Result<()> {
    anyhow::bail!("this build lacks the `acme` feature; rebuild with --features acme")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn loopback_target_dials_loopback_only() {
        assert_eq!(
            loopback_target("[::]:8080").unwrap(),
            "[::1]:8080".parse().unwrap()
        );
        assert_eq!(
            loopback_target("0.0.0.0:9").unwrap(),
            "127.0.0.1:9".parse().unwrap()
        );
        assert_eq!(
            loopback_target("[::1]:8080").unwrap(),
            "[::1]:8080".parse().unwrap()
        );
        assert!(loopback_target("[2001:db8::1]:8080").is_err());
        assert!(loopback_target("192.0.2.1:8080").is_err());
    }

    #[test]
    fn parses_plain_and_chunked_responses() {
        let plain = b"HTTP/1.1 202 Accepted\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(parse_response(plain).unwrap(), (202, b"{}".to_vec()));
        let chunked =
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), (200, b"abcde".to_vec()));
        assert!(parse_response(b"garbage").is_err());
    }

    #[test]
    fn formats_utc_times() {
        assert_eq!(utc_text(0), "1970-01-01 00:00:00Z");
        assert_eq!(utc_text(1_709_210_096), "2024-02-29 12:34:56Z");
    }

    #[test]
    fn renders_the_status_table() {
        let value = serde_json::json!({
            "source": "offline", "state": "setup", "ca": "letsencrypt", "staging": true,
            "challenge": "dns-01", "names": ["a.example.org"], "expires_at": null,
            "alarm": "none", "bootstrap_fingerprint": "AB:CD",
            "reasons": ["credential-missing: CF_TOKEN"],
        });
        let text = render_status(&value);
        assert!(text.starts_with("engine not running"));
        assert!(text.contains("letsencrypt (staging)"));
        assert!(text.contains("AB:CD"));
        assert!(text.contains("reason        credential-missing: CF_TOKEN"));
    }
}
