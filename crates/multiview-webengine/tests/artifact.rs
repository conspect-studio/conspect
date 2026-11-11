use multiview_webengine::artifact::*;
use std::io::{Read, Write};
fn limits() -> Limits {
    Limits {
        bytes: 100_000,
        count: 30,
    }
}
fn provenance(generation: u64) -> Provenance {
    Provenance {
        owner: "owner".into(),
        fill: "fill".into(),
        job: format!("job{generation}"),
        generation,
    }
}
#[allow(clippy::unwrap_used)] // Fixture helper; failures must stop the test.
fn commit(store: &mut ArtifactStore, generation: u64) -> Artifact {
    let mut pending = store.begin(provenance(generation), 100, |_| true).unwrap();
    pending.write_all(b"actual disk bytes").unwrap();
    pending.complete().unwrap()
}
#[test]
fn disk_restart_refresh_and_pin() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(dir.path(), limits()).unwrap();
    assert!(ArtifactStore::open(dir.path(), limits()).is_err());
    let old = commit(&mut store, 1);
    let mut lease = store.pin(&old.id, "owner", |_| true).unwrap();
    let new = commit(&mut store, 2);
    assert_ne!(old.id, new.id);
    assert_eq!(
        store.remove(&old.id, "owner", |_| true).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let mut bytes = Vec::new();
    lease.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"actual disk bytes");
    drop(lease);
    store.remove(&old.id, "owner", |_| true).unwrap();
    drop(store);
    let store = ArtifactStore::open(dir.path(), limits()).unwrap();
    let mut lease = store.pin(&new.id, "owner", |_| true).unwrap();
    bytes.clear();
    lease.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"actual disk bytes");
    assert_eq!(lease.artifact().provenance.generation, 2);
}
#[test]
fn missing_partial_mutable_acl_and_paths_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(dir.path(), limits()).unwrap();
    assert!(store.begin(provenance(1), 1, |_| false).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    {
        let mut pending = store.begin(provenance(1), 100, |_| true).unwrap();
        pending.write_all(b"partial").unwrap();
    }
    let artifact = commit(&mut store, 1);
    assert!(store.pin(&artifact.id, "owner", |_| false).is_err());
    assert!(store.pin(&artifact.id, "other", |_| true).is_err());
    for path in [
        "../secret",
        "https://example.org",
        "/etc/passwd",
        "a000000000000000/",
        "a000000000000000g",
    ] {
        assert!(ArtifactId::parse(path).is_err());
    }
    let data = dir.path().join(format!("{}.data", artifact.id.as_str()));
    std::fs::write(&data, b"changed disk bytes").unwrap();
    assert!(store.pin(&artifact.id, "owner", |_| true).is_err());
    std::fs::remove_file(&data).unwrap();
    assert!(store.pin(&artifact.id, "owner", |_| true).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", &data).unwrap();
        assert!(store.pin(&artifact.id, "owner", |_| true).is_err());
    }
}
#[test]
fn quota_and_generation_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(
        dir.path(),
        Limits {
            bytes: 4200,
            count: 3,
        },
    )
    .unwrap();
    assert!(store.begin(provenance(1), 105, |_| true).is_err());
    let mut p = store.begin(provenance(1), 3, |_| true).unwrap();
    assert!(p.write_all(b"four").is_err());
    drop(p);
    let _ = commit(&mut store, 1);
    assert!(store.begin(provenance(1), 1, |_| true).is_err());
    assert!(store.begin(provenance(2), 1, |_| true).is_err());
}
fn fail_at(boundary: Boundary) -> std::io::Result<()> {
    if boundary == Boundary::MetadataPublish {
        Err(std::io::Error::other("injected"))
    } else {
        Ok(())
    }
}
#[test]
fn unpublished_bytes_never_recover_as_complete() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(dir.path(), limits()).unwrap();
    store.set_boundary_hook(fail_at);
    let mut p = store.begin(provenance(1), 100, |_| true).unwrap();
    let id = p.id().clone();
    p.write_all(b"real bytes").unwrap();
    assert!(p.complete().is_err());
    drop(store);
    let store = ArtifactStore::open(dir.path(), limits()).unwrap();
    assert!(store.pin(&id, "owner", |_| true).is_err());
}
#[test]
fn every_sync_edge_recovers_only_complete_bytes() {
    use Boundary::*;
    for boundary in [
        BytesSync,
        BytesPublish,
        BytesDirectorySync,
        MetadataSync,
        MetadataPublish,
        CommitDirectorySync,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ArtifactStore::open(dir.path(), limits()).unwrap();
        // Function-pointer hook deliberately has no global mutable test state.
        store.set_boundary_hook(match boundary {
            BytesSync => |b| {
                if b == BytesSync {
                    Err(std::io::Error::other("sync"))
                } else {
                    Ok(())
                }
            },
            BytesPublish => |b| {
                if b == BytesPublish {
                    Err(std::io::Error::other("publish"))
                } else {
                    Ok(())
                }
            },
            BytesDirectorySync => |b| {
                if b == BytesDirectorySync {
                    Err(std::io::Error::other("dir"))
                } else {
                    Ok(())
                }
            },
            MetadataSync => |b| {
                if b == MetadataSync {
                    Err(std::io::Error::other("meta"))
                } else {
                    Ok(())
                }
            },
            MetadataPublish => fail_at,
            CommitDirectorySync => |b| {
                if b == CommitDirectorySync {
                    Err(std::io::Error::other("commit"))
                } else {
                    Ok(())
                }
            },
        });
        let mut p = store.begin(provenance(1), 100, |_| true).unwrap();
        let id = p.id().clone();
        p.write_all(b"real bytes").unwrap();
        assert!(p.complete().is_err());
        drop(store);
        let store = ArtifactStore::open(dir.path(), limits()).unwrap();
        let result = store.pin(&id, "owner", |_| true);
        if boundary == CommitDirectorySync {
            let mut bytes = vec![];
            result.unwrap().read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"real bytes");
        } else {
            assert!(result.is_err());
        }
    }
}

#[test]
fn identity_never_reuses_deleted_content_and_lease_retains_root_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(dir.path(), limits()).unwrap();
    let old = commit(&mut store, 1);
    store.remove(&old.id, "owner", |_| true).unwrap();
    drop(store);
    let mut store = ArtifactStore::open(dir.path(), limits()).unwrap();
    let new = commit(&mut store, 2);
    assert_ne!(old.id, new.id);
    let lease = store.pin(&new.id, "owner", |_| true).unwrap();
    drop(store);
    assert!(ArtifactStore::open(dir.path(), limits()).is_err());
    drop(lease);
    assert!(ArtifactStore::open(dir.path(), limits()).is_ok());
    assert!(serde_json::from_str::<ArtifactId>("\"../escape\"").is_err());
}

#[test]
fn hard_linked_committed_content_is_refused_at_pin_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(dir.path(), limits()).unwrap();
    let artifact = commit(&mut store, 1);
    let data = dir.path().join(format!("{}.data", artifact.id.as_str()));
    let other = tempfile::tempdir().unwrap();
    let alias = other.path().join("alias");
    std::fs::hard_link(&data, &alias).unwrap();
    // A second directory entry makes the content unreadable (nlink != 1).
    assert!(store.pin(&artifact.id, "owner", |_| true).is_err());
    drop(store);
    assert!(ArtifactStore::open(dir.path(), limits()).is_err());
    std::fs::remove_file(&alias).unwrap();
    assert!(ArtifactStore::open(dir.path(), limits()).is_ok());
}

#[test]
fn loose_mode_content_and_symlinked_lock_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(dir.path(), limits()).unwrap();
    let artifact = commit(&mut store, 1);
    drop(store);
    let data = dir.path().join(format!("{}.data", artifact.id.as_str()));
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(ArtifactStore::open(dir.path(), limits()).is_err());
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600)).unwrap();
    let lock = dir.path().join("lock");
    let moved = dir.path().join("lock.real");
    std::fs::rename(&lock, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &lock).unwrap();
    assert!(ArtifactStore::open(dir.path(), limits()).is_err());
}

#[test]
fn enumeration_comes_from_the_opened_directory_not_the_path() {
    // Replace the path with a different directory after the fd was opened is a
    // race we cannot interleave deterministically; prove the observable half:
    // a root path that is itself a symlink is refused (fd opened O_NOFOLLOW).
    let real = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let link = parent.path().join("root");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    assert!(ArtifactStore::open(&link, limits()).is_err());
}
