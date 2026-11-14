//! Where a cast output may write (ADR-0111): pure path rules, no filesystem
//! access.
//!
//! A cast output removes its own stream files when a cast stops, so its
//! directory must not be a place another output writes to. The engine also
//! refuses, at run time, a directory that holds files it did not write; the
//! rules here catch the configuration mistakes that are visible in the
//! document itself.

use std::path::{Component, Path, PathBuf};

use crate::error::ConfigError;
use crate::schema::Output;

/// `path` with `.` removed and `..` resolved lexically (never against the
/// filesystem: a symlink is not followed). `..` at the root stays at the
/// root.
#[must_use]
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop the root or a prefix; a leading `..` of a
                // relative path is kept.
                let popped =
                    matches!(out.components().next_back(), Some(Component::Normal(_))) && out.pop();
                if !popped && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The directory an HLS / LL-HLS output writes its playlist and segments
/// to: the parent of a `.m3u8` path, else the path itself (the engine's
/// `hls_paths` rule).
fn hls_dir(path: &str) -> PathBuf {
    let path = Path::new(path);
    let is_playlist = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"));
    if is_playlist {
        path.parent().map_or_else(PathBuf::new, Path::to_path_buf)
    } else {
        path.to_path_buf()
    }
}

/// The directory `output` writes files to, normalised, when it is a
/// file-writing output with an absolute location.
fn write_dir(output: &Output) -> Option<PathBuf> {
    let dir = match output {
        Output::Hls { path, .. } | Output::LlHls { path, .. } => hls_dir(path),
        Output::Cast { dir: Some(dir), .. } => PathBuf::from(dir),
        _ => return None,
    };
    dir.is_absolute().then(|| normalize_lexically(&dir))
}

/// Validate every cast output's authored `dir`: absolute, not shared with
/// another cast output, and neither equal to, inside, nor around the
/// directory of another output that writes files.
///
/// # Errors
///
/// [`ConfigError::Validation`] naming both outputs.
pub fn validate_cast_dirs(outputs: &[Output]) -> Result<(), ConfigError> {
    for (index, output) in outputs.iter().enumerate() {
        let Output::Cast {
            id, dir: Some(dir), ..
        } = output
        else {
            continue;
        };
        if dir.is_empty() {
            // Reported by the per-output rule with its own message.
            continue;
        }
        if !Path::new(dir).is_absolute() {
            return Err(ConfigError::Validation(format!(
                "cast output {id:?}: dir {dir:?} must be an absolute path"
            )));
        }
        let own = normalize_lexically(Path::new(dir));
        for (other_index, other) in outputs.iter().enumerate() {
            if other_index == index {
                continue;
            }
            let Some(theirs) = write_dir(other) else {
                continue;
            };
            if !own.starts_with(&theirs) && !theirs.starts_with(&own) {
                continue;
            }
            let other_id = other.id();
            if matches!(other, Output::Cast { .. }) && own == theirs {
                return Err(ConfigError::Validation(format!(
                    "cast outputs {id:?} and {other_id:?} use the same dir {}; each cast output \
                     needs its own directory",
                    own.display()
                )));
            }
            return Err(ConfigError::Validation(format!(
                "cast output {id:?}: dir {} overlaps the directory {} that output {other_id:?} \
                 writes to; a cast output removes its stream files when a cast stops, so point \
                 `dir` at a directory of its own or remove `dir`",
                own.display(),
                theirs.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{hls_dir, normalize_lexically};
    use std::path::{Path, PathBuf};

    #[test]
    fn lexical_normalisation_resolves_dots_without_touching_the_filesystem() {
        let n = |text: &str| normalize_lexically(Path::new(text));
        assert_eq!(n("/a/b/../c/./d"), PathBuf::from("/a/c/d"));
        assert_eq!(n("/a/b/"), PathBuf::from("/a/b"));
        assert_eq!(n("/../a"), PathBuf::from("/a"));
        assert_eq!(n("/a/../.."), PathBuf::from("/"));
        assert_eq!(n("../a/./b"), PathBuf::from("../a/b"));
        assert_eq!(n("a/../../b"), PathBuf::from("../b"));
    }

    #[test]
    fn the_hls_directory_is_the_playlist_parent_or_the_path_itself() {
        assert_eq!(
            hls_dir("/srv/hls/p/index.m3u8"),
            PathBuf::from("/srv/hls/p")
        );
        assert_eq!(
            hls_dir("/srv/hls/p/INDEX.M3U8"),
            PathBuf::from("/srv/hls/p")
        );
        assert_eq!(hls_dir("/srv/hls/p"), PathBuf::from("/srv/hls/p"));
    }
}
