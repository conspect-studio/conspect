//! Pinned private directory capability. All leaf operations are fd-relative.
use crate::{Error, Result};
use rustix::fs::{open, openat, renameat, unlinkat, AtFlags, Mode, OFlags};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Component, Path},
};

pub struct PrivateDir(File);
impl PrivateDir {
    pub fn open(path: &Path) -> Result<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut dir = File::from(
            open(
                if path.is_absolute() { "/" } else { "." },
                flags,
                Mode::empty(),
            )
            .map_err(|_| Error::Denied)?,
        );
        for part in path.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    dir = File::from(
                        openat(&dir, name, flags, Mode::empty()).map_err(|_| Error::Denied)?,
                    )
                }
                _ => return Err(Error::Denied),
            }
        }
        let meta = dir.metadata().map_err(|_| Error::Denied)?;
        if !meta.is_dir()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.permissions().mode() & 0o7777 != 0o700
        {
            return Err(Error::Denied);
        }
        Ok(Self(dir))
    }
    fn name<'a>(&self, path: &'a Path) -> Result<&'a std::ffi::OsStr> {
        path.file_name()
            .filter(|n| *n != "." && *n != "..")
            .ok_or(Error::Invalid)
    }
    fn checked(&self, file: File, limit: u64) -> Result<File> {
        let m = file.metadata().map_err(|_| Error::Denied)?;
        if !m.is_file()
            || m.uid() != rustix::process::geteuid().as_raw()
            || m.nlink() != 1
            || m.permissions().mode() & 0o7777 != 0o600
            || m.len() > limit
        {
            return Err(Error::Denied);
        }
        Ok(file)
    }
    pub fn file(&self, path: &Path, create: bool, exclusive: bool) -> Result<File> {
        let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if create {
            flags |= OFlags::CREATE;
        }
        if exclusive {
            flags |= OFlags::EXCL;
        }
        let fd = openat(&self.0, self.name(path)?, flags, Mode::RUSR | Mode::WUSR)
            .map_err(|_| Error::Denied)?;
        self.checked(File::from(fd), u64::MAX)
    }
    pub fn read(&self, path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
        let fd = match openat(
            &self.0,
            self.name(path)?,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(Error::Denied),
        };
        let file = self.checked(File::from(fd), limit)?;
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Denied)?;
        if bytes.len() as u64 > limit {
            return Err(Error::Denied);
        }
        Ok(Some(bytes))
    }
    /// Any existing entry, including a symlink, blocks recovery.
    pub fn exists(&self, path: &Path) -> Result<bool> {
        match rustix::fs::statat(&self.0, self.name(path)?, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(_) => Err(Error::Denied),
        }
    }
    pub fn sync(&self) -> Result<()> {
        self.0.sync_all().map_err(|_| Error::Denied)
    }
    pub fn remove(&self, path: &Path) -> Result<()> {
        unlinkat(&self.0, self.name(path)?, AtFlags::empty()).map_err(|_| Error::Denied)
    }
    pub fn replace(&self, stage: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
        let mut file = self.file(stage, true, true)?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| Error::Denied)?;
        renameat(&self.0, self.name(stage)?, &self.0, self.name(path)?)
            .map_err(|_| Error::Denied)?;
        self.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    #[test]
    fn rejects_links_modes_oversize_and_directory_substitution() {
        let outer = tempfile::tempdir().unwrap();
        let path = outer.path().join("private");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let dir = PrivateDir::open(&path).unwrap();
        let leaf = Path::new("state");
        dir.replace(Path::new("stage"), leaf, b"old").unwrap();
        assert!(dir.read(leaf, 2).is_err());
        symlink(path.join("state"), path.join("symlink")).unwrap();
        assert!(dir.read(Path::new("symlink"), 64).is_err());
        assert!(dir.file(Path::new("symlink"), true, false).is_err());
        std::fs::hard_link(path.join("state"), path.join("hardlink")).unwrap();
        assert!(dir.read(leaf, 64).is_err());
        // Verified absolute target is the disposable test hardlink only.
        std::fs::remove_file(path.join("hardlink")).unwrap();
        std::fs::set_permissions(path.join("state"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(dir.read(leaf, 64).is_err());
        std::fs::set_permissions(path.join("state"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        // Rename only the known disposable private test directory.
        let moved = outer.path().join("moved");
        std::fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        assert!(PrivateDir::open(&path).is_err());
        dir.replace(Path::new("next"), leaf, b"pinned").unwrap();
        assert_eq!(std::fs::read(moved.join("state")).unwrap(), b"pinned");
        let alias = outer.path().join("alias");
        symlink(outer.path(), &alias).unwrap();
        assert!(PrivateDir::open(&alias.join("moved")).is_err());
    }
}
