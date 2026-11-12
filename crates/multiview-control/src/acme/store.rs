//! The private ACME state directory.
//!
//! The directory is 0700 and owned by the effective user; every file is 0600,
//! written atomically (temp file + fsync + rename + directory fsync), never
//! read through a symlink, and paths never contain `..`.
//!
//! Layout:
//! * `account.<directory-host>-<8 hex of sha256(directory url)>.json`: ACME
//!   account credentials per directory, so a CA change never reuses another
//!   CA's account and two directories on one host stay apart;
//! * `active.pem`: private key + chain + a `CONSPECT ACME META` section
//!   (non-secret metadata) in ONE file, so all three commit with one rename;
//! * `pending-txt.json`: the DNS-01 ledger: intent is recorded before
//!   `publish_txt`, the handle after it, the entry removed after `remove_txt`.
//! * `secrets/<NAME>`: host-written credentials (`multiview tls set-credential`).

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::dns::TxtHandle;
use super::AcmeError;

/// Most entries the pending-TXT ledger holds.
pub const MAX_LEDGER_ENTRIES: usize = 64;
/// Largest state file read (account, certificate bundle, metadata, ledger).
const READ_CAP: u64 = 1024 * 1024;

/// Ensure `path` is an owner-private 0700 directory (created if missing).
///
/// # Errors
/// [`AcmeError::Storage`] on `..`, a symlink, a foreign owner or wider mode.
pub fn private_dir(path: &Path) -> Result<(), AcmeError> {
    use rustix::fs::{FileType, Mode, OFlags};
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(AcmeError::Storage);
    }
    crate::web_capture_startup::reject_symlink(path).map_err(|_| AcmeError::Storage)?;
    create_missing_private(path)?;
    crate::web_capture_startup::reject_symlink(path).map_err(|_| AcmeError::Storage)?;
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| AcmeError::Storage)?;
    let stat = rustix::fs::fstat(&fd).map_err(|_| AcmeError::Storage)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o700
    {
        return Err(AcmeError::Storage);
    }
    Ok(())
}

/// Create every missing component of `path`, root first, each one 0700 by an
/// explicit `fchmod` (never trusting the process umask) and owned by the
/// effective user. Existing ancestors are left exactly as they are.
fn create_missing_private(path: &Path) -> Result<(), AcmeError> {
    use rustix::fs::{Mode, OFlags};
    let mut missing: Vec<&Path> = path
        .ancestors()
        .take_while(|p| std::fs::symlink_metadata(p).is_err())
        .collect();
    missing.reverse();
    for dir in missing {
        match rustix::fs::mkdir(dir, Mode::from_raw_mode(0o700)) {
            Ok(()) => {}
            // Created concurrently: the leaf check in `private_dir` judges it.
            Err(rustix::io::Errno::EXIST) => continue,
            Err(_) => return Err(AcmeError::Storage),
        }
        let fd = rustix::fs::open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| AcmeError::Storage)?;
        let stat = rustix::fs::fstat(&fd).map_err(|_| AcmeError::Storage)?;
        if stat.st_uid != rustix::process::geteuid().as_raw() {
            return Err(AcmeError::Storage);
        }
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700)).map_err(|_| AcmeError::Storage)?;
    }
    Ok(())
}

fn plain_name(name: &str) -> Result<(), AcmeError> {
    if name.is_empty()
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(AcmeError::Storage);
    }
    Ok(())
}

/// Atomically write `data` to `dir/name` with mode 0600.
///
/// # Errors
/// [`AcmeError::Storage`] on any I/O or permission failure.
pub fn write_private(dir: &Path, name: &str, data: &[u8]) -> Result<(), AcmeError> {
    use std::os::unix::fs::PermissionsExt;
    plain_name(name)?;
    private_dir(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|_| AcmeError::Storage)?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| AcmeError::Storage)?;
    temp.write_all(data).map_err(|_| AcmeError::Storage)?;
    temp.as_file().sync_all().map_err(|_| AcmeError::Storage)?;
    temp.persist(dir.join(name))
        .map_err(|_| AcmeError::Storage)?;
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|_| AcmeError::Storage)
}

/// Read `dir/name` if it exists: a regular 0600 file owned by the effective
/// user, opened without following a symlink.
///
/// # Errors
/// [`AcmeError::Storage`] when the file is insecure or unreadable.
pub fn read_private(dir: &Path, name: &str) -> Result<Option<Vec<u8>>, AcmeError> {
    plain_name(name)?;
    private_dir(dir)?;
    read_owner_file(&dir.join(name))
}

/// Open a 0600, euid-owned, regular non-symlink file. `Ok(None)` if absent.
///
/// # Errors
/// [`AcmeError::Storage`] when the file is insecure, too large or unreadable.
pub fn read_owner_file(path: &Path) -> Result<Option<Vec<u8>>, AcmeError> {
    use rustix::fs::{FileType, Mode, OFlags};
    let fd = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(AcmeError::Storage),
    };
    let stat = rustix::fs::fstat(&fd).map_err(|_| AcmeError::Storage)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
    {
        return Err(AcmeError::Storage);
    }
    let mut data = Vec::new();
    std::fs::File::from(fd)
        .take(READ_CAP + 1)
        .read_to_end(&mut data)
        .map_err(|_| AcmeError::Storage)?;
    if u64::try_from(data.len()).unwrap_or(u64::MAX) > READ_CAP {
        return Err(AcmeError::Storage);
    }
    Ok(Some(data))
}

/// Remove `dir/name` if present (durably).
///
/// # Errors
/// [`AcmeError::Storage`] on an I/O failure other than "absent".
pub fn remove_private(dir: &Path, name: &str) -> Result<(), AcmeError> {
    plain_name(name)?;
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|_| AcmeError::Storage),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(AcmeError::Storage),
    }
}

/// The account file name for a directory URL.
///
/// # Errors
/// [`AcmeError::Config`] if the URL has no host.
pub fn account_file_name(directory_url: &str) -> Result<String, AcmeError> {
    let url = url::Url::parse(directory_url).map_err(|_| AcmeError::Config)?;
    let host: String = url
        .host_str()
        .ok_or(AcmeError::Config)?
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect();
    let digest = Sha256::digest(directory_url.as_bytes());
    let hex = digest.iter().take(4).fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    });
    Ok(format!("account.{host}-{hex}.json"))
}

/// Non-secret metadata of the active certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveMeta {
    /// Leaf `notBefore`, Unix seconds.
    pub not_before: i64,
    /// Leaf `notAfter`, Unix seconds.
    pub not_after: i64,
    /// RFC 9773 certificate identifier (`aki.serial`), when derivable.
    pub ari_cert_id: Option<String>,
    /// CA profile name.
    pub ca: String,
    /// Directory URL that issued it.
    pub directory_url: String,
    /// SAN set requested.
    pub names: Vec<String>,
}

/// `active.pem` file name.
pub const ACTIVE_PEM: &str = "active.pem";
/// `pending-txt.json` file name.
pub const LEDGER: &str = "pending-txt.json";

/// One DNS-01 record the engine intends to publish or has published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Provider name (`"cloudflare"`).
    pub provider: String,
    /// The `_acme-challenge` name.
    pub fqdn: String,
    /// Provider handle, once publish returned.
    #[serde(default)]
    pub handle: Option<TxtHandle>,
}

/// The pending-TXT ledger in `state_dir`.
#[derive(Debug, Clone)]
pub struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    /// Open the ledger in `dir`.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Read all entries (empty when absent).
    ///
    /// # Errors
    /// [`AcmeError::Storage`] on insecure or malformed state.
    pub fn load(&self) -> Result<Vec<LedgerEntry>, AcmeError> {
        let Some(raw) = read_private(&self.dir, LEDGER)? else {
            return Ok(Vec::new());
        };
        let entries: Vec<LedgerEntry> =
            serde_json::from_slice(&raw).map_err(|_| AcmeError::Storage)?;
        if entries.len() > MAX_LEDGER_ENTRIES {
            return Err(AcmeError::Storage);
        }
        Ok(entries)
    }

    /// Replace all entries.
    ///
    /// # Errors
    /// [`AcmeError::Storage`] on I/O failure or more than [`MAX_LEDGER_ENTRIES`].
    pub fn save(&self, entries: &[LedgerEntry]) -> Result<(), AcmeError> {
        if entries.len() > MAX_LEDGER_ENTRIES {
            return Err(AcmeError::Storage);
        }
        if entries.is_empty() {
            private_dir(&self.dir)?;
            return remove_private(&self.dir, LEDGER);
        }
        let raw = serde_json::to_vec(entries).map_err(|_| AcmeError::Storage)?;
        write_private(&self.dir, LEDGER, &raw)
    }

    /// Record an intent to publish (no handle yet). Returns its index.
    ///
    /// # Errors
    /// [`AcmeError::Storage`].
    pub fn record_intent(&self, provider: &str, fqdn: &str) -> Result<usize, AcmeError> {
        let mut entries = self.load()?;
        entries.push(LedgerEntry {
            provider: provider.to_owned(),
            fqdn: fqdn.to_owned(),
            handle: None,
        });
        self.save(&entries)?;
        Ok(entries.len().saturating_sub(1))
    }

    /// Attach the handle to the first handle-less entry for `fqdn`.
    ///
    /// # Errors
    /// [`AcmeError::Storage`].
    pub fn record_handle(&self, fqdn: &str, handle: &TxtHandle) -> Result<(), AcmeError> {
        let mut entries = self.load()?;
        if let Some(entry) = entries
            .iter_mut()
            .find(|e| e.fqdn == fqdn && e.handle.is_none())
        {
            entry.handle = Some(handle.clone());
        }
        self.save(&entries)
    }

    /// Drop the entry carrying `handle`.
    ///
    /// # Errors
    /// [`AcmeError::Storage`].
    pub fn forget_handle(&self, handle: &TxtHandle) -> Result<(), AcmeError> {
        let mut entries = self.load()?;
        entries.retain(|e| e.handle.as_ref() != Some(handle));
        self.save(&entries)
    }

    /// Drop a handle-less entry for `fqdn` (publish failed, nothing to remove).
    ///
    /// # Errors
    /// [`AcmeError::Storage`].
    pub fn forget_intent(&self, fqdn: &str) -> Result<(), AcmeError> {
        let mut entries = self.load()?;
        if let Some(pos) = entries
            .iter()
            .position(|e| e.fqdn == fqdn && e.handle.is_none())
        {
            entries.remove(pos);
        }
        self.save(&entries)
    }
}

/// Read a host-written secret `<state_dir>/secrets/<name>`: a 0600, euid-owned,
/// regular non-symlink file; one trailing newline is trimmed.
///
/// # Errors
/// [`AcmeError::Storage`] when the file is insecure or unreadable.
pub fn read_secret_file(
    state_dir: &Path,
    name: &str,
) -> Result<Option<zeroize::Zeroizing<String>>, AcmeError> {
    plain_name(name)?;
    let dir = state_dir.join("secrets");
    crate::web_capture_startup::reject_symlink(&dir).map_err(|_| AcmeError::Storage)?;
    let Some(raw) = read_owner_file(&dir.join(name))? else {
        return Ok(None);
    };
    let raw = zeroize::Zeroizing::new(raw);
    let text = std::str::from_utf8(&raw).map_err(|_| AcmeError::Storage)?;
    let trimmed = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    Ok(Some(zeroize::Zeroizing::new(trimmed.to_owned())))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::case_sensitive_file_extension_comparisons
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn private_tmp() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("state");
        (t, dir)
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ledger_round_trip_and_permissions() {
        let (_t, dir) = private_tmp();
        let ledger = Ledger::new(dir.clone());
        assert_eq!(ledger.load().unwrap(), Vec::new());
        ledger
            .record_intent("cloudflare", "_acme-challenge.a.example")
            .unwrap();
        ledger
            .record_intent("cloudflare", "_acme-challenge.a.example")
            .unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(LEDGER)), 0o600);
        let h1 = TxtHandle("z1/r1".into());
        let h2 = TxtHandle("z1/r2".into());
        ledger
            .record_handle("_acme-challenge.a.example", &h1)
            .unwrap();
        ledger
            .record_handle("_acme-challenge.a.example", &h2)
            .unwrap();
        let entries = ledger.load().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].handle.as_ref(), Some(&h1));
        assert_eq!(entries[1].handle.as_ref(), Some(&h2));
        ledger.forget_handle(&h1).unwrap();
        assert_eq!(ledger.load().unwrap().len(), 1);
        ledger.forget_handle(&h2).unwrap();
        assert_eq!(ledger.load().unwrap(), Vec::new());
        assert!(!dir.join(LEDGER).exists());
        ledger.record_intent("cloudflare", "x").unwrap();
        ledger.forget_intent("x").unwrap();
        assert_eq!(ledger.load().unwrap(), Vec::new());
    }

    #[test]
    fn ledger_is_bounded() {
        let (_t, dir) = private_tmp();
        let ledger = Ledger::new(dir);
        for i in 0..MAX_LEDGER_ENTRIES {
            ledger
                .record_intent("cloudflare", &format!("n{i}"))
                .unwrap();
        }
        assert_eq!(
            ledger.record_intent("cloudflare", "overflow").unwrap_err(),
            AcmeError::Storage
        );
    }

    #[test]
    fn refuses_wide_dirs_symlinks_dotdot_and_wide_files() {
        let t = tempfile::tempdir().unwrap();
        let wide = t.path().join("wide");
        std::fs::create_dir(&wide).unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&wide).is_err());
        assert!(private_dir(&t.path().join("a").join("..").join("b")).is_err());
        assert!(private_dir(Path::new("relative/dir")).is_err());
        let (_t2, dir) = private_tmp();
        write_private(&dir, "f.json", b"{}").unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(read_private(&link, "f.json").is_err());
        std::fs::set_permissions(dir.join("f.json"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(read_private(&dir, "f.json").is_err());
        assert!(write_private(&dir, "../x", b"").is_err());
        assert!(write_private(&dir, ".hidden", b"").is_err());
    }

    #[test]
    fn every_created_ancestor_is_0700_whatever_the_umask() {
        let t = tempfile::tempdir().unwrap();
        let old = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
        let leaf = t.path().join("var").join("lib").join("tls");
        let result = private_dir(&leaf);
        rustix::process::umask(old);
        result.unwrap();
        for dir in [
            t.path().join("var"),
            t.path().join("var").join("lib"),
            leaf.clone(),
        ] {
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
        // A pre-existing ancestor is left alone.
        let mode = std::fs::metadata(t.path()).unwrap().permissions().mode() & 0o777;
        assert_ne!(mode, 0, "tempdir untouched");
    }

    #[test]
    fn secret_file_must_be_0600_regular_non_symlink() {
        let (_t, dir) = private_tmp();
        private_dir(&dir).unwrap();
        let secrets = dir.join("secrets");
        private_dir(&secrets).unwrap();
        let path = secrets.join("CF_TOKEN");
        std::fs::write(&path, b"tok-123\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_secret_file(&dir, "CF_TOKEN").is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_secret_file(&dir, "CF_TOKEN")
                .unwrap()
                .unwrap()
                .as_str(),
            "tok-123"
        );
        let link = secrets.join("LINKED");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_secret_file(&dir, "LINKED").is_err());
        assert!(read_secret_file(&dir, "ABSENT").unwrap().is_none());
        assert!(read_secret_file(&dir, "../CF_TOKEN").is_err());
    }

    #[test]
    fn account_file_is_per_directory_url() {
        let a = account_file_name("https://localhost:14000/dir").unwrap();
        let b = account_file_name("https://localhost:14001/dir").unwrap();
        assert!(
            a.starts_with("account.localhost-") && a.ends_with(".json"),
            "{a}"
        );
        assert_eq!(a.len(), "account.localhost-".len() + 8 + ".json".len());
        assert_ne!(a, b);
        let v6 = account_file_name("https://[::1]:14000/dir").unwrap();
        assert!(plain_name(&v6).is_ok(), "{v6}");
    }
}
