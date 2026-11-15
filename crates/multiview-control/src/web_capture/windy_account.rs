//! The deployment's Windy Premium account (issue #470).
//!
//! The email/password pair is stored only encrypted in the owner-bound
//! credential vault ([`multiview_credentials::Vault`], `ChaCha20Poly1305` under an
//! external master key), exactly like the managed Cloudflare credential. The
//! private state file next to it holds the opaque `vault:` reference and the
//! last verify verdict — never the email or password. Nothing here logs, audits
//! or returns the secret; the only consumer of the plaintext is the browser
//! host's private stdin (capture and verify jobs).
//!
//! Premium is available only after a passing verify **of the currently stored
//! credential**: replacing or clearing the credential resets it.
use super::{WindyAccountView, WindyVerdict};
use multiview_credentials::{Credential, SecretRef, Vault};
use multiview_webengine::capture::WindyCredentials;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, PoisonError,
    },
};

/// Vault owner bound into every ciphertext's AAD: the account is a single
/// deployment-wide integration, not an API key's private secret.
const OWNER: &str = "system:windy-account";
const STATE_FILE: &str = "windy-account.json";

/// Runs one bounded login check. The real one drives the browser host; tests
/// inject a fake. It must never log or retain the credential.
pub type Verifier = Arc<dyn Fn(WindyCredentials) -> WindyVerdict + Send + Sync>;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Persisted {
    /// Opaque vault reference of the current credential.
    secret_ref: Option<String>,
    /// The reference the last verify ran against (stale once replaced).
    verified_ref: Option<String>,
    /// Last verify verdict for `verified_ref`.
    verdict: Option<WindyVerdict>,
    /// Unix seconds of the last verify of `verified_ref`.
    verified_at: Option<i64>,
}

/// Why an account operation failed (sanitized; never carries input data).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountError {
    /// Malformed email or password.
    Invalid,
    /// No credential is stored.
    NotConfigured,
    /// A verify is already running.
    Busy,
    /// The private store could not be read or written.
    Storage,
}

/// Encrypted Windy account store plus its verify gate.
pub struct WindyAccount {
    vault: Vault,
    state_path: PathBuf,
    state: Mutex<Persisted>,
    verifying: AtomicBool,
    verifier: Verifier,
}

impl WindyAccount {
    /// Open the store in an existing owner-private (0700) directory.
    /// # Errors
    /// A loose/missing directory or an unreadable/corrupt state file.
    pub fn open(dir: &Path, master_key: [u8; 32], verifier: Verifier) -> io::Result<Self> {
        let vault = Vault::open(dir, master_key)
            .map_err(|_| io::Error::other("Windy vault unavailable"))?;
        let state_path = dir.join(STATE_FILE);
        let state = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| io::Error::other("Windy account state invalid"))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Persisted::default(),
            Err(_) => return Err(io::Error::other("Windy account state unavailable")),
        };
        Ok(Self {
            vault,
            state_path,
            state: Mutex::new(state),
            verifying: AtomicBool::new(false),
            verifier,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Persisted> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Secret-free status: whether a credential is set and the last verdict.
    #[must_use]
    pub fn view(&self) -> WindyAccountView {
        view_of(&self.lock(), self.verifying.load(Ordering::Acquire))
    }

    /// True only when the current credential passed its latest verify.
    #[must_use]
    pub fn premium_available(&self) -> bool {
        premium_ok(&self.lock())
    }

    /// Replace the stored credential (resets the Premium verdict). Blocking.
    /// # Errors
    /// Invalid input or storage failure; the previous credential stays intact.
    pub fn set(&self, email: &str, password: &str) -> Result<WindyAccountView, AccountError> {
        let email = email.trim();
        if !email.contains('@') {
            return Err(AccountError::Invalid);
        }
        let credential = Credential::Password {
            username: email.to_owned(),
            password: password.to_owned(),
        };
        let reference = self.vault.store(OWNER, &credential).map_err(|e| match e {
            multiview_credentials::VaultError::Invalid => AccountError::Invalid,
            _ => AccountError::Storage,
        })?;
        let mut state = self.lock();
        let previous = state.secret_ref.clone();
        let next = Persisted {
            secret_ref: Some(reference.as_str().to_owned()),
            verified_ref: None,
            verdict: None,
            verified_at: None,
        };
        if self.persist(&next).is_err() {
            let _ = self.vault.delete_if_present(OWNER, &reference);
            return Err(AccountError::Storage);
        }
        *state = next;
        let view = view_of(&state, self.verifying.load(Ordering::Acquire));
        drop(state);
        self.forget(previous);
        Ok(view)
    }

    /// Remove the stored credential. Idempotent. Blocking.
    /// # Errors
    /// The state could not be persisted.
    pub fn clear(&self) -> Result<WindyAccountView, AccountError> {
        let mut state = self.lock();
        let previous = state.secret_ref.clone();
        let next = Persisted::default();
        self.persist(&next).map_err(|_| AccountError::Storage)?;
        *state = next;
        let view = view_of(&state, self.verifying.load(Ordering::Acquire));
        drop(state);
        self.forget(previous);
        Ok(view)
    }

    fn forget(&self, previous: Option<String>) {
        if let Some(reference) = previous.and_then(|r| SecretRef::parse(&r).ok()) {
            // Best effort: an orphan is still encrypted and unreferenced.
            let _ = self.vault.delete_if_present(OWNER, &reference);
        }
    }

    /// Resolve the current credential for a capture job, only when Premium is
    /// verified for it. The value goes straight into the job request (stdin).
    #[must_use]
    pub fn resolve_for_capture(&self) -> Option<WindyCredentials> {
        let reference = {
            let state = self.lock();
            if !premium_ok(&state) {
                return None;
            }
            state.secret_ref.clone()?
        };
        self.resolve(&reference)
    }

    fn resolve(&self, reference: &str) -> Option<WindyCredentials> {
        let reference = SecretRef::parse(reference).ok()?;
        match &self.vault.resolve(OWNER, &reference).ok()? {
            Credential::Password { username, password } => Some(WindyCredentials {
                username: username.clone(),
                password: password.clone(),
            }),
            _ => None,
        }
    }

    /// Run one bounded verify of the current credential and record the verdict
    /// against it (a credential replaced meanwhile is never marked verified).
    /// Blocking; call on a management blocking worker.
    /// # Errors
    /// No credential, a verify already running, or storage failure.
    pub fn verify(&self, now_unix: i64) -> Result<(WindyVerdict, WindyAccountView), AccountError> {
        if self.verifying.swap(true, Ordering::AcqRel) {
            return Err(AccountError::Busy);
        }
        let _reset = Reset(&self.verifying);
        let reference = self
            .lock()
            .secret_ref
            .clone()
            .ok_or(AccountError::NotConfigured)?;
        let verdict = match self.resolve(&reference) {
            Some(credentials) => (self.verifier)(credentials),
            None => return Err(AccountError::Storage),
        };
        let mut state = self.lock();
        if state.secret_ref.as_deref() == Some(reference.as_str()) {
            let mut next = state.clone();
            next.verified_ref = Some(reference);
            next.verdict = Some(verdict);
            next.verified_at = Some(now_unix);
            self.persist(&next).map_err(|_| AccountError::Storage)?;
            *state = next;
        }
        Ok((verdict, view_of(&state, true)))
    }

    /// Atomic 0600 snapshot: temp file, fsync, rename, directory fsync.
    fn persist(&self, state: &Persisted) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = self
            .state_path
            .parent()
            .ok_or_else(|| io::Error::other("state directory"))?;
        let temp = dir.join(format!(".windy-account-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec(state)?)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.state_path)?;
            std::fs::File::open(dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

struct Reset<'a>(&'a AtomicBool);
impl Drop for Reset<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn premium_ok(state: &Persisted) -> bool {
    state.secret_ref.is_some()
        && state.verified_ref == state.secret_ref
        && state.verdict == Some(WindyVerdict::Premium)
}

fn view_of(state: &Persisted, verifying: bool) -> WindyAccountView {
    let current = state.secret_ref.is_some() && state.verified_ref == state.secret_ref;
    WindyAccountView {
        set: state.secret_ref.is_some(),
        last_verified_at: if current {
            state.verified_at.and_then(|s| {
                chrono::DateTime::from_timestamp(s, 0)
                    .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            })
        } else {
            None
        },
        result: if current { state.verdict } else { None },
        premium_available: premium_ok(state),
        verifying,
    }
}

/// The production verifier: a bounded browser-host login check (90 s).
#[must_use]
pub fn browser_verifier(tools: multiview_webengine::capture::Tools) -> Verifier {
    Arc::new(move |credentials| {
        let cancelled = AtomicBool::new(false);
        let control = multiview_webengine::runner::Control {
            cancelled: &cancelled,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(90),
        };
        match multiview_webengine::runner::verify_premium(&tools, credentials, control) {
            Ok(multiview_webengine::runner::PremiumVerdict::Premium) => WindyVerdict::Premium,
            Ok(multiview_webengine::runner::PremiumVerdict::Rejected) => WindyVerdict::Rejected,
            Ok(multiview_webengine::runner::PremiumVerdict::NotPremium) => WindyVerdict::NotPremium,
            Ok(multiview_webengine::runner::PremiumVerdict::Unavailable) | Err(_) => {
                WindyVerdict::Unavailable
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn store(verdict: WindyVerdict) -> (tempfile::TempDir, WindyAccount) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let account = WindyAccount::open(dir.path(), [3; 32], Arc::new(move |_| verdict)).unwrap();
        (dir, account)
    }

    fn files_contain(dir: &Path, needle: &[u8]) -> bool {
        std::fs::read_dir(dir).unwrap().any(|e| {
            let bytes = std::fs::read(e.unwrap().path()).unwrap_or_default();
            bytes.windows(needle.len()).any(|w| w == needle)
        })
    }

    #[test]
    fn premium_requires_a_passing_verify_of_the_current_credential() {
        let (dir, account) = store(WindyVerdict::Premium);
        assert!(!account.premium_available());
        assert_eq!(account.verify(1), Err(AccountError::NotConfigured));
        account.set("ops@example.com", "pw-AAAA-secret").unwrap();
        assert!(!account.premium_available());
        assert!(
            account.resolve_for_capture().is_none(),
            "fail closed before verify"
        );
        let (verdict, view) = account.verify(1_700_000_000).unwrap();
        assert_eq!(verdict, WindyVerdict::Premium);
        assert!(view.premium_available && view.set);
        assert_eq!(
            view.last_verified_at.as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        let resolved = account.resolve_for_capture().unwrap();
        assert_eq!(resolved.password, "pw-AAAA-secret");
        // Changing the credential resets the verdict.
        let view = account.set("ops@example.com", "pw-BBBB-secret").unwrap();
        assert!(!view.premium_available && view.result.is_none());
        assert!(!account.premium_available());
        assert!(account.resolve_for_capture().is_none());
        // Nothing on disk carries the plaintext; the old ciphertext is gone.
        for needle in [
            &b"pw-AAAA-secret"[..],
            b"pw-BBBB-secret",
            b"ops@example.com",
        ] {
            assert!(!files_contain(dir.path(), needle));
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // Reopen: state survives a restart.
        account.verify(5).unwrap();
        drop(account);
        let reopened =
            WindyAccount::open(dir.path(), [3; 32], Arc::new(|_| WindyVerdict::Rejected)).unwrap();
        assert!(reopened.premium_available());
        reopened.clear().unwrap();
        assert!(!reopened.premium_available() && !reopened.view().set);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn wrong_password_is_a_typed_verdict_and_not_premium() {
        let (_dir, account) = store(WindyVerdict::Rejected);
        account.set("ops@example.com", "wrong").unwrap();
        let (verdict, view) = account.verify(1).unwrap();
        assert_eq!(verdict, WindyVerdict::Rejected);
        assert_eq!(view.result, Some(WindyVerdict::Rejected));
        assert!(!view.premium_available);
        assert!(account.resolve_for_capture().is_none());
    }

    #[test]
    fn invalid_input_and_concurrent_verify_are_refused() {
        let (_dir, account) = store(WindyVerdict::Premium);
        assert_eq!(account.set("no-at-sign", "x"), Err(AccountError::Invalid));
        assert_eq!(account.set("a@b", ""), Err(AccountError::Invalid));
        account.set("a@b", "x").unwrap();
        account.verifying.store(true, Ordering::Release);
        assert_eq!(account.verify(1), Err(AccountError::Busy));
    }

    #[test]
    fn a_credential_replaced_during_verify_is_not_marked_verified() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let slot: Arc<Mutex<Option<Arc<WindyAccount>>>> = Arc::new(Mutex::new(None));
        let inner = slot.clone();
        let account = Arc::new(
            WindyAccount::open(
                dir.path(),
                [3; 32],
                Arc::new(move |_| {
                    let a = inner.lock().unwrap().clone().unwrap();
                    a.set("ops@example.com", "replaced").unwrap();
                    WindyVerdict::Premium
                }),
            )
            .unwrap(),
        );
        *slot.lock().unwrap() = Some(account.clone());
        account.set("ops@example.com", "original").unwrap();
        let (verdict, view) = account.verify(1).unwrap();
        assert_eq!(verdict, WindyVerdict::Premium);
        assert!(!view.premium_available);
        assert!(!account.premium_available());
        *slot.lock().unwrap() = None;
    }
}
