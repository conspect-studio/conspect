//! Slice 6 (issue #292): certbot renews `[control.tls] mode = "static"` files
//! in place (`live/<host>/fullchain.pem` re-pointed at a new `archive/` file),
//! and the engine must serve the renewed certificate on the next handshake
//! without a restart. Exercised the way certbot does it: symlinks swapped to
//! new archive files, then a broken pair (key not yet written) that must not
//! replace the good certificate, then the completed pair.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_config::TlsConfig;
use multiview_control::certificate_source::{CertificateSource, StaticCertificateSource};
use multiview_control::{
    rustls_material_from_source, spawn_static_certificate_reload, static_certificate_source,
};

/// Write a fresh self-signed pair into `archive/<n>` and point `live/` at it.
fn renew(dir: &Path, n: u32, cn: &str) -> Vec<u8> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![cn.to_owned()]).unwrap();
    let archive = dir.join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    let chain = archive.join(format!("fullchain{n}.pem"));
    let key = archive.join(format!("privkey{n}.pem"));
    std::fs::write(&chain, cert.pem()).unwrap();
    std::fs::write(&key, signing_key.serialize_pem()).unwrap();
    let live = dir.join("live");
    std::fs::create_dir_all(&live).unwrap();
    for (link, target) in [("fullchain.pem", &chain), ("privkey.pem", &key)] {
        let tmp = live.join(format!(".{link}.new"));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(target, &tmp).unwrap();
        std::fs::rename(&tmp, live.join(link)).unwrap();
    }
    cert.der().to_vec()
}

fn leaf(source: &StaticCertificateSource) -> Vec<u8> {
    source.current().cert[0].as_ref().to_vec()
}

async fn wait_for(source: &StaticCertificateSource, want: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while leaf(source) != want {
        assert!(
            Instant::now() < deadline,
            "the renewed certificate never loaded"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certbot_style_renewal_is_served_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = renew(dir.path(), 1, "first.example");
    let tls = TlsConfig::Static {
        cert_file: dir.path().join("live/fullchain.pem"),
        key_file: dir.path().join("live/privkey.pem"),
    };
    let source = Arc::new(static_certificate_source(&tls).unwrap());
    assert_eq!(leaf(&source), first);
    // The plain listener's material reads the same source per handshake.
    let dyn_source: Arc<dyn CertificateSource> = source.clone();
    let _material = rustls_material_from_source(dyn_source).unwrap();
    let task = spawn_static_certificate_reload(
        tls.clone(),
        Arc::clone(&source),
        Duration::from_millis(20),
    );

    // Renewal 1 -> 2: picked up.
    let second = renew(dir.path(), 2, "second.example");
    wait_for(&source, &second).await;

    // A half-done renewal: a new chain whose key does not match (the key file
    // still points at renewal 2's key). Must NOT replace the served pair.
    let rcgen::CertifiedKey { cert, .. } =
        rcgen::generate_simple_self_signed(vec!["torn.example".to_owned()]).unwrap();
    let torn = dir.path().join("archive/fullchain3.pem");
    std::fs::write(&torn, cert.pem()).unwrap();
    let tmp = dir.path().join("live/.fullchain.pem.new");
    std::os::unix::fs::symlink(&torn, &tmp).unwrap();
    std::fs::rename(&tmp, dir.path().join("live/fullchain.pem")).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(leaf(&source), second, "a mismatched pair keeps the old one");

    // The renewal completes: served.
    let fourth = renew(dir.path(), 4, "fourth.example");
    wait_for(&source, &fourth).await;

    // Files deleted outright: the last good certificate keeps serving.
    std::fs::remove_file(dir.path().join("live/fullchain.pem")).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(leaf(&source), fourth);
    task.abort();
}

/// #360: a renewal that lands after the served pair was loaded but before the
/// reload task's first read must still be picked up (the task used to take
/// its baseline from the renewed files and never replace the old certificate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewal_between_load_and_first_poll_is_not_missed() {
    let dir = tempfile::tempdir().unwrap();
    let first = renew(dir.path(), 1, "first.example");
    let tls = TlsConfig::Static {
        cert_file: dir.path().join("live/fullchain.pem"),
        key_file: dir.path().join("live/privkey.pem"),
    };
    let source = Arc::new(static_certificate_source(&tls).unwrap());
    assert_eq!(leaf(&source), first);
    let second = renew(dir.path(), 2, "second.example");
    let task = spawn_static_certificate_reload(tls, Arc::clone(&source), Duration::from_millis(20));
    wait_for(&source, &second).await;
    task.abort();
}

/// A torn pair (new chain, old key) is retried until it loads even when the
/// completing write leaves every stamp (inode, size, mtime) identical to the
/// torn state: a failed stamp must not suppress parsing forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_torn_pair_completed_with_identical_stamps_is_still_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("chain.pem");
    let key_path = dir.path().join("key.pem");
    let a = rcgen::generate_simple_self_signed(vec!["a.example".to_owned()]).unwrap();
    let b = rcgen::generate_simple_self_signed(vec!["b.example".to_owned()]).unwrap();
    std::fs::write(&cert_path, a.cert.pem()).unwrap();
    std::fs::write(&key_path, a.signing_key.serialize_pem()).unwrap();
    let tls = TlsConfig::Static {
        cert_file: cert_path.clone(),
        key_file: key_path.clone(),
    };
    let source = Arc::new(static_certificate_source(&tls).unwrap());
    let task = spawn_static_certificate_reload(tls, Arc::clone(&source), Duration::from_millis(20));

    // Torn: new chain B, key still A's. Several polls see and reject it.
    std::fs::write(&cert_path, b.cert.pem()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(leaf(&source), a.cert.der().to_vec());

    // Complete it in place, forcing the key file's stamp to its torn value.
    let mtime = std::fs::metadata(&key_path).unwrap().modified().unwrap();
    let new_key = b.signing_key.serialize_pem();
    assert_eq!(new_key.len(), a.signing_key.serialize_pem().len());
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&key_path)
        .unwrap();
    std::io::Write::write_all(&mut &file, new_key.as_bytes()).unwrap();
    file.set_modified(mtime).unwrap();
    drop(file);
    wait_for(&source, b.cert.der().as_ref()).await;
    task.abort();
}
