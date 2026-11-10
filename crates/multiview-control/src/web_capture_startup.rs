//! Explicit opt-in startup for the real management capture service.
//! Environment values are deployment configuration, never accepted from HTTP.
use crate::{
    web_capture::Provider,
    web_capture_store::{DurableTerms, Ledger},
};
use multiview_webengine::{
    artifact::{ArtifactStore, Limits},
    capture::Tools,
    service::{Config, Service},
};
use std::{io, path::PathBuf, sync::Arc};
/// Start capture only when the deployment explicitly enables it.
/// # Errors
/// Missing tools, unsafe paths or failed durable recovery abort configured startup.
pub fn from_environment() -> io::Result<Provider> {
    if std::env::var("CONSPECT_WEB_CAPTURE").as_deref() != Ok("1") {
        return Ok(Provider::default());
    }
    let path = |name: &str| -> io::Result<PathBuf> {
        let p = PathBuf::from(
            std::env::var_os(name)
                .ok_or_else(|| io::Error::other("capture deployment path missing"))?,
        );
        if !p.is_absolute() {
            return Err(io::Error::other("capture deployment path must be absolute"));
        }
        Ok(p)
    };
    let root = path("CONSPECT_WEB_CAPTURE_ROOT")?;
    let tools = Tools {
        node: path("CONSPECT_WEB_NODE")?,
        browser: path("CONSPECT_WEB_BROWSER")?,
        ffmpeg: path("CONSPECT_WEB_FFMPEG")?,
    };
    reject_symlink(&root)?;
    for t in [&tools.node, &tools.browser, &tools.ffmpeg] {
        reject_symlink(t)?;
    }
    let journal = root.join("capture-service.journal");
    let artifacts = root.join("artifacts");
    private_dir(&root)?;
    private_dir(&artifacts)?;
    let terms = Arc::new(DurableTerms::open(&root.join("capture-terms.log"))?);
    let ledger = Arc::new(Ledger::durable(&root.join("capture-idempotency.log"))?);
    let store = ArtifactStore::open(
        &artifacts,
        Limits {
            bytes: 2 * 1024 * 1024 * 1024,
            count: 256,
        },
    )?;
    let service = Service::start_durable(
        tools,
        store,
        Config {
            queue_capacity: 4,
            history: 64,
            max_fills: 32,
        },
        &journal,
    )?;
    Ok(Provider::configured(Arc::new(service), terms, ledger))
}
/// Refuse a symlink at the final component or any existing ancestor.
/// # Errors
/// A symlink or unreadable metadata.
pub fn reject_symlink(path: &std::path::Path) -> io::Result<()> {
    let mut cur = PathBuf::new();
    for c in path.components() {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(io::Error::other("capture path must not contain a symlink"))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
/// Create (mode 0700) or adopt a capture directory without a path-based chmod:
/// open it `O_DIRECTORY | O_NOFOLLOW`, then `fstat` that descriptor and require
/// owner == this process and mode exactly 0700.
fn private_dir(path: &std::path::Path) -> io::Result<()> {
    use rustix::fs::{FileType, Mode, OFlags};
    reject_symlink(path)?;
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
        {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o700
    {
        return Err(io::Error::other(
            "capture directory must be an owner-private 0700 directory",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::private_dir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn creates_0700_and_adopts_only_an_exact_0700_owned_directory() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a").join("b");
        private_dir(&p).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // No silent chmod repair: a loose existing directory is refused.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&p).is_err());
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn symlink_and_file_targets_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        private_dir(&real).unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(private_dir(&link).is_err());
        let file = d.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(private_dir(&file).is_err());
    }
}
