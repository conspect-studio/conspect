//! Encrypted, owner-bound credential references. The caller supplies a master key
//! from an external secret service; this crate never persists that key. Run file
//! operations on a management-plane blocking worker, never on the media clock.
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    ChaCha20Poly1305, Nonce,
};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const MAX_CIPHERTEXT: usize = 16384;
const ENCRYPTION_OVERHEAD: usize = 28;
fn valid_header(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    Bearer { token: String },
    GlobalKey { email: String, key: String },
}
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential([REDACTED])")
    }
}
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("credential store unavailable")]
    Storage,
    #[error("invalid credential or reference")]
    Invalid,
    #[error("credential unavailable for this owner or key")]
    Unavailable,
}
/// Safe to persist in application configuration. Not a bearer capability:
/// resolution also requires the authenticated owner and external master key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct SecretRef(String);
impl SecretRef {
    pub fn parse(value: &str) -> Result<Self, VaultError> {
        let id = value.strip_prefix("vault:").ok_or(VaultError::Invalid)?;
        if uuid::Uuid::parse_str(id)
            .map_err(|_| VaultError::Invalid)?
            .to_string()
            != id
        {
            return Err(VaultError::Invalid);
        }
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub struct Vault {
    root: PathBuf,
    key: Zeroizing<[u8; 32]>,
}
impl Vault {
    /// Root must already exist, be owned by this service, and be inaccessible to
    /// other users. No insecure cross-platform fallback is provided.
    #[cfg(unix)]
    pub fn open(root: impl AsRef<Path>, key: [u8; 32]) -> Result<Self, VaultError> {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(root.as_ref()).map_err(|_| VaultError::Storage)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(VaultError::Storage);
        }
        Ok(Self {
            root: root.as_ref().to_owned(),
            key: Zeroizing::new(key),
        })
    }
    fn path(&self, reference: &SecretRef) -> Result<PathBuf, VaultError> {
        SecretRef::parse(reference.as_str())?;
        Ok(self.root.join(&reference.0[6..]))
    }
    fn aad(owner: &str, reference: &SecretRef) -> Result<Vec<u8>, VaultError> {
        if owner.is_empty() || owner.len() > 256 {
            return Err(VaultError::Invalid);
        }
        Ok(format!("conspect-credential-v1:{}:{}", owner.len(), owner)
            .into_bytes()
            .into_iter()
            .chain(reference.0.bytes())
            .collect())
    }
    #[cfg(unix)]
    pub fn store(&self, owner: &str, credential: &Credential) -> Result<SecretRef, VaultError> {
        use std::os::unix::fs::OpenOptionsExt;
        let valid = match credential {
            Credential::Bearer { token } => valid_header(token) && token.len() <= 8192,
            Credential::GlobalKey { email, key } => {
                valid_header(email)
                    && email.contains('@')
                    && email.len() <= 320
                    && valid_header(key)
                    && key.len() <= 8192
            }
        };
        if !valid {
            return Err(VaultError::Invalid);
        }
        let reference = SecretRef(format!("vault:{}", uuid::Uuid::new_v4()));
        let aad = Self::aad(owner, &reference)?;
        let plain =
            Zeroizing::new(serde_json::to_vec(credential).map_err(|_| VaultError::Invalid)?);
        if plain.len() > MAX_CIPHERTEXT - ENCRYPTION_OVERHEAD {
            return Err(VaultError::Invalid);
        }
        let cipher =
            ChaCha20Poly1305::new_from_slice(self.key.as_ref()).map_err(|_| VaultError::Invalid)?;
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let encrypted = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plain,
                    aad: &aad,
                },
            )
            .map_err(|_| VaultError::Storage)?;
        let path = self.path(&reference)?;
        // A private staging file is not resolvable as a SecretRef. Publish only
        // after fsync; hard_link atomically creates without replacing a file.
        let staging = self.root.join(format!(".pending-{}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)
            .map_err(|_| VaultError::Storage)?;
        let result = file
            .write_all(&nonce)
            .and_then(|()| file.write_all(&encrypted))
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::hard_link(&staging, &path));
        // Both paths were constructed inside the validated private store.
        let cleanup = fs::remove_file(&staging);
        result
            .and(cleanup)
            .and_then(|()| fs::File::open(&self.root)?.sync_all())
            .map_err(|_| VaultError::Storage)?;
        Ok(reference)
    }
    pub fn resolve(&self, owner: &str, reference: &SecretRef) -> Result<Credential, VaultError> {
        let aad = Self::aad(owner, reference)?;
        let path = self.path(reference)?;
        let meta = fs::symlink_metadata(&path).map_err(|_| VaultError::Unavailable)?;
        if !meta.is_file() || meta.len() > MAX_CIPHERTEXT as u64 {
            return Err(VaultError::Unavailable);
        }
        let bytes = fs::read(path).map_err(|_| VaultError::Unavailable)?;
        if bytes.len() < ENCRYPTION_OVERHEAD {
            return Err(VaultError::Unavailable);
        }
        let cipher = ChaCha20Poly1305::new_from_slice(self.key.as_ref())
            .map_err(|_| VaultError::Unavailable)?;
        let plain = Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&bytes[..12]),
                    Payload {
                        msg: &bytes[12..],
                        aad: &aad,
                    },
                )
                .map_err(|_| VaultError::Unavailable)?,
        );
        serde_json::from_slice(&plain).map_err(|_| VaultError::Unavailable)
    }
    /// Proves owner binding before removal; caller still enforces object ACLs.
    pub fn delete(&self, owner: &str, reference: &SecretRef) -> Result<(), VaultError> {
        self.resolve(owner, reference)?;
        fs::remove_file(self.path(reference)?).map_err(|_| VaultError::Storage)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn encrypted_roundtrip_owner_binding_tamper_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let vault = Vault::open(dir.path(), [7; 32]).unwrap();
        let secret = Credential::GlobalKey {
            email: "me@example.com".into(),
            key: "super-secret".into(),
        };
        let reference = vault.store("alice", &secret).unwrap();
        let path = vault.path(&reference).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(!bytes.windows(12).any(|w| w == b"super-secret"));
        assert!(!format!("{secret:?}").contains("super-secret"));
        assert!(matches!(
            vault.resolve("alice", &reference).unwrap(),
            Credential::GlobalKey { .. }
        ));
        assert!(vault.resolve("bob", &reference).is_err());
        assert!(vault.delete("bob", &reference).is_err());
        assert!(Vault::open(dir.path(), [8; 32])
            .unwrap()
            .resolve("alice", &reference)
            .is_err());
        let mut damaged = bytes.clone();
        damaged[15] ^= 1;
        fs::write(&path, damaged).unwrap();
        assert!(vault.resolve("alice", &reference).is_err());
        fs::write(&path, bytes).unwrap();
        vault.delete("alice", &reference).unwrap();
        assert!(vault.resolve("alice", &reference).is_err());
    }
    #[test]
    fn serialized_bounds_and_control_characters() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let vault = Vault::open(dir.path(), [7; 32]).unwrap();
        for token in [
            "\\".repeat(8192),
            "\"".repeat(8192),
            format!("{}x", "\n".repeat(8191)),
        ] {
            let secret = Credential::Bearer { token };
            assert!(matches!(
                vault.store("alice", &secret),
                Err(VaultError::Invalid)
            ));
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        }
        assert!(vault
            .store(
                "alice",
                &Credential::Bearer {
                    token: "bad\r\nheader".into()
                }
            )
            .is_err());
        let reference = vault
            .store(
                "alice",
                &Credential::Bearer {
                    token: "x".repeat(8192),
                },
            )
            .unwrap();
        let resolved = vault.resolve("alice", &reference).unwrap();
        match &resolved {
            Credential::Bearer { token } => assert_eq!(token, &"x".repeat(8192)),
            _ => panic!("wrong credential kind"),
        }
        vault.delete("alice", &reference).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    #[test]
    fn rejects_traversal_and_public_directory() {
        use std::os::unix::fs::PermissionsExt;
        assert!(SecretRef::parse("vault:../../secret").is_err());
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Vault::open(dir.path(), [0; 32]).is_err());
    }
}
