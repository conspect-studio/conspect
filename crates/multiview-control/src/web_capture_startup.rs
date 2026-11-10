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
fn private_dir(path: &std::path::Path) -> io::Result<()> {
    reject_symlink(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(io::Error::other("capture directory is not a directory")),
        Err(_) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir_all(path)?;
        }
    }
    reject_symlink(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
