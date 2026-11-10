//! Explicit opt-in startup for the real management capture service.
//! Environment values are deployment configuration, never accepted from HTTP.
use crate::web_capture::Provider;
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
    let journal = root.join("capture-service.journal");
    let artifacts = root.join("artifacts");
    std::fs::create_dir_all(&artifacts)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700))?;
    }
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
    Ok(Provider::configured(Arc::new(service)))
}
