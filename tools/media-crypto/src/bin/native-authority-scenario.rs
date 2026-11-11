//! Private harness bootstrap coordinator and offline file-boundary probe.
//! Bootstrap is NOT connected native media proof; that integration remains pending.
use conspect_media_crypto::bootstrap as harness;
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};
use zeroize::Zeroizing;

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

// Deliberately no Debug/error-source propagation: serde/io errors can include
// private paths, URLs or manifest data. Only static diagnostics reach stderr.
fn parse_private_manifest(path: &Path) -> Result<(), &'static str> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "manifest unavailable")?;
    let metadata = file
        .metadata()
        .map_err(|_| "manifest metadata unavailable")?;
    // fstat checks the opened object, not a potentially replaced path.
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > MAX_MANIFEST_BYTES
    {
        return Err("manifest must be a single-link regular file with mode 0600 and bounded size");
    }
    // SAFETY: geteuid has no arguments, ownership requirements or memory effects.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err("manifest owner differs from process user");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    (&mut file)
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "manifest read failed")?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("manifest exceeds size bound");
    }
    // IgnoredAny validates JSON without creating additional String copies of
    // secret fields. This is syntax validation, NOT schema agreement.
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    <serde::de::IgnoredAny as serde::Deserialize>::deserialize(&mut deserializer)
        .map_err(|_| "manifest JSON invalid")?;
    deserializer.end().map_err(|_| "manifest JSON invalid")?;
    Ok(())
}

fn bootstrap(args: &[std::ffi::OsString]) -> Result<(), &'static str> {
    use conspect_media_crypto::authority_http::VerifiedHttpsTransport;
    if args.len() != 6
        || args[0] != "--bootstrap"
        || args[2] != "--marker"
        || args[4] != "--expected"
    {
        return Err(
            "usage: --bootstrap MANIFEST_PATH --marker MARKER_PATH --expected EXPECTED_PATH",
        );
    }
    if args[1] == args[3] || args[1] == args[5] || args[3] == args[5] {
        return Err("paths must differ");
    }
    let manifest = harness::private_json(Path::new(&args[1]))?;
    let mut state = harness::Bootstrap::parse(&manifest)?;
    let ca = manifest["caPublic"].as_str().ok_or("CA missing")?;
    let transport =
        VerifiedHttpsTransport::from_harness_ca_pem(ca.as_bytes()).map_err(|_| "CA rejected")?;
    state.acquire(std::sync::Arc::new(transport.clone()))?;
    let result: Result<(), &'static str> = (|| {
        state.book(&transport)?;
        state.renew(&transport)?;
        state.mint_mapping(&transport)?;
        state.write_marker(Path::new(&args[3]))?;
        let fragment = state.wait_expected(&transport, Path::new(&args[5]))?;
        state.validate_expected(&fragment)?;
        Err("bootstrap mapping validated; native connected scenario NOT IMPLEMENTED; lease released")
    })();
    let release = state.release(&transport);
    result.and(release)
}
fn main() {
    let all: Vec<_> = std::env::args_os().skip(1).collect();
    if all.first().is_some_and(|s| s == "--bootstrap") {
        if let Err(message) = bootstrap(&all) {
            eprintln!("{message}");
            std::process::exit(1);
        }
        return;
    }
    let mut args = std::env::args_os().skip(1);
    let mode = args.next();
    let path = args.next();
    if mode.as_deref() != Some(std::ffi::OsStr::new("--check-manifest"))
        || path.is_none()
        || args.next().is_some()
    {
        eprintln!("usage: native-authority-scenario --check-manifest MANIFEST_PATH");
        std::process::exit(2);
    }
    if let Err(message) = parse_private_manifest(Path::new(&path.unwrap())) {
        eprintln!("{message}");
        std::process::exit(1);
    }
    println!("manifest syntax/private-file checks passed; schema and live scenario unsupported; no network calls");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn fixture(bytes: &[u8], mode: u32) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .unwrap();
        file
    }
    #[test]
    fn private_json_syntax_only() {
        let f = fixture(br#"{"test":"synthetic-not-a-real-secret"}"#, 0o600);
        assert!(parse_private_manifest(f.path()).is_ok());
    }
    #[test]
    fn rejects_public_mode_and_symlink() {
        let f = fixture(b"{}", 0o644);
        assert!(parse_private_manifest(f.path()).is_err());
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        symlink(f.path(), &link).unwrap();
        assert!(parse_private_manifest(&link).is_err());
    }
    #[test]
    fn rejects_hardlink_directory_and_oversize() {
        let f = fixture(b"{}", 0o600);
        let dir = tempfile::tempdir().unwrap();
        std::fs::hard_link(f.path(), dir.path().join("alias")).unwrap();
        assert!(parse_private_manifest(f.path()).is_err());
        assert!(parse_private_manifest(dir.path()).is_err());
        let large = fixture(&vec![b' '; MAX_MANIFEST_BYTES as usize + 1], 0o600);
        assert!(parse_private_manifest(large.path()).is_err());
    }
    #[test]
    fn parse_errors_never_include_input() {
        let f = fixture(b"private-url-and-secret invalid", 0o600);
        assert_eq!(
            parse_private_manifest(f.path()),
            Err("manifest JSON invalid")
        );
        let f = fixture(b"{} {}", 0o600);
        assert!(parse_private_manifest(f.path()).is_err());
    }
}
