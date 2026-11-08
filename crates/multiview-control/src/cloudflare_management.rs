//! Durable management provider. Never attached to an engine/media callback.
#![allow(missing_docs)]
use crate::routes::cloudflare::{CloudflareManagement, CloudflareRequest, CloudflareResult};
use multiview_cloudflare::{lifecycle::Registry, Client, Credential};
use multiview_credentials::{SecretRef, Vault};
use std::{collections::BTreeMap, io::Write, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Snapshot {
    records: BTreeMap<String, Vec<multiview_cloudflare::lifecycle::Record>>,
    /// Pending operations are never replayed automatically after a crash.
    pending: BTreeMap<String, String>,
    #[serde(default)]
    policies: BTreeMap<String, Policy>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Policy {
    mode: String,
    name: String,
    auth_domain: String,
    secret_ref: String,
}

pub struct ManagedCloudflare {
    vault: Arc<Vault>,
    journal: PathBuf,
    state: Mutex<Snapshot>,
}
impl ManagedCloudflare {
    /// Start one bounded, low-frequency management task. Busy ticks are dropped.
    /// Dropping the returned handle does not stop it; abort it during shutdown.
    pub fn spawn_reconciler(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(std::time::Duration::from_secs(60));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let work = match self.state.try_lock() {
                    Ok(state) => state
                        .policies
                        .iter()
                        .take(32)
                        .map(|(k, p)| (k.clone(), p.clone()))
                        .collect::<Vec<_>>(),
                    Err(_) => continue,
                };
                for (key, policy) in work {
                    if policy.mode != "auto" {
                        continue;
                    }
                    let Ok((owner, account_id)) = serde_json::from_str::<(String, String)>(&key)
                    else {
                        continue;
                    };
                    let _ = self
                        .run(
                            &owner,
                            "reconcile",
                            CloudflareRequest {
                                account_id,
                                secret_ref: policy.secret_ref,
                                relay_id: None,
                                name: Some(policy.name),
                                auth_domain: Some(policy.auth_domain),
                                permission_group_id: None,
                                token_jti: None,
                                mode: None,
                            },
                        )
                        .await;
                }
            }
        })
    }
    /// Explicit runtime injection from the service's external secret provider.
    /// No environment/default key or op:// pseudo-resolution is implemented.
    pub fn from_master_key(
        private_dir: &std::path::Path,
        master_key: [u8; 32],
    ) -> Result<Self, &'static str> {
        let vault = Vault::open(private_dir, master_key).map_err(|_| "vault unavailable")?;
        Self::open(Arc::new(vault), private_dir.join("cloudflare-journal.json"))
    }
    pub fn open(vault: Arc<Vault>, journal: PathBuf) -> Result<Self, &'static str> {
        let state = match std::fs::read(&journal) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| "invalid journal")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot::default(),
            Err(_) => return Err("journal unavailable"),
        };
        Ok(Self {
            vault,
            journal,
            state: Mutex::new(state),
        })
    }
    fn persist(&self, state: &Snapshot) -> Result<(), &'static str> {
        // Atomic snapshot + fsync. Parent is an operator-created private directory.
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let parent = self.journal.parent().ok_or("journal directory")?;
        let meta = std::fs::symlink_metadata(parent).map_err(|_| "journal directory")?;
        if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
            return Err("journal permissions");
        }
        let temp = parent.join(format!(".cloudflare-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|_| "journal write")?;
        file.write_all(&serde_json::to_vec(state).map_err(|_| "journal encode")?)
            .map_err(|_| "journal write")?;
        file.sync_all().map_err(|_| "journal sync")?;
        std::fs::rename(temp, &self.journal).map_err(|_| "journal rename")?;
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|_| "journal sync")
    }
    async fn run(
        &self,
        owner: &str,
        action: &str,
        r: CloudflareRequest,
    ) -> Result<CloudflareResult, &'static str> {
        // Refuse concurrent work rather than creating an unbounded wait queue.
        let mut state = self.state.try_lock().map_err(|_| "busy")?;
        if state.records.len() > 32 || state.policies.len() > 32 {
            return Err("account quota");
        }
        let key = serde_json::to_string(&(owner, &r.account_id)).map_err(|_| "owner")?;
        let credential = self
            .vault
            .resolve(
                owner,
                &SecretRef::parse(&r.secret_ref).map_err(|_| "reference")?,
            )
            .map_err(|_| "credential")?;
        let credential = match &credential {
            multiview_credentials::Credential::Bearer { token } => Credential::Bearer {
                token: token.clone(),
            },
            multiview_credentials::Credential::GlobalKey { email, key } => Credential::GlobalKey {
                email: email.clone(),
                key: key.clone(),
            },
        };
        let client = Client::new(r.account_id.clone(), credential).map_err(|_| "account")?;
        if action == "audit" {
            return Ok(CloudflareResult {
                data: serde_json::to_value(client.audit_user_tokens().await.map_err(|_| "audit")?)
                    .map_err(|_| "audit")?,
            });
        }
        if action == "permissions" {
            return Ok(CloudflareResult {
                data: serde_json::to_value(
                    client
                        .permission_groups()
                        .await
                        .map_err(|_| "permissions")?,
                )
                .map_err(|_| "permissions")?,
            });
        }
        if action == "set-mode" {
            if !state.policies.contains_key(&key) && state.policies.len() >= 32 {
                return Err("policy quota");
            }
            let mode = r.mode.as_deref().ok_or("mode")?;
            if !matches!(mode, "auto" | "manual") {
                return Err("mode");
            }
            let policy = Policy {
                mode: mode.to_owned(),
                name: r
                    .name
                    .clone()
                    .unwrap_or_else(|| "Conspect managed relay".into()),
                auth_domain: r.auth_domain.clone().unwrap_or_else(|| owner.to_owned()),
                secret_ref: r.secret_ref.clone(),
            };
            state.policies.insert(key.clone(), policy);
            self.persist(&state)?;
            return Ok(CloudflareResult {
                data: serde_json::json!({"mode":mode,"desired_transport":"moq","effective_transport":"unverified"}),
            });
        }
        let policy = state.policies.get(&key).cloned();
        let action = if action == "reconcile" {
            if policy.as_ref().is_some_and(|p| p.mode == "auto")
                && state
                    .records
                    .get(&key)
                    .is_none_or(|records| records.is_empty())
            {
                "provision"
            } else {
                "relays"
            }
        } else {
            action
        };
        let mut r = r;
        if action == "provision" {
            if let Some(policy) = &policy {
                if r.name.is_none() {
                    r.name = Some(policy.name.clone());
                }
                if r.auth_domain.is_none() {
                    r.auth_domain = Some(policy.auth_domain.clone());
                }
            }
        }
        if action == "relays" {
            return Ok(CloudflareResult {
                data: serde_json::json!({"records":state.records.get(&key),"pending":state.pending.get(&key),"mode":policy.as_ref().map(|p|p.mode.as_str()).unwrap_or("manual"),"desired_transport":"moq","effective_transport":"unverified"}),
            });
        }
        if action == "recover" {
            // Explicit read-only discovery: never guess that a timed-out POST failed.
            // Operator must inspect remote resources before separately acknowledging.
            let relays = client.list().await.map_err(|_| "recovery discovery")?;
            return Ok(CloudflareResult {
                data: serde_json::json!({"remote_relays":relays,"pending":state.pending.get(&key)}),
            });
        }
        if action == "acknowledge-recovery" {
            // Explicit operator acknowledgement; this does not recreate/delete anything.
            state.pending.remove(&key);
            self.persist(&state)?;
            return Ok(CloudflareResult {
                data: serde_json::json!({"acknowledged":true}),
            });
        }
        if let Some(id) = r.relay_id.as_deref() {
            for (other, records) in &state.records {
                let (_, account): (String, String) =
                    serde_json::from_str(other).map_err(|_| "journal owner")?;
                if other != &key
                    && account == r.account_id
                    && records.iter().any(|record| record.relay.uid == id)
                {
                    return Err("relay already allocated to another owner");
                }
            }
        }
        if state.pending.contains_key(&key) {
            return Err("manual recovery required");
        }
        if !matches!(
            action,
            "provision"
                | "import"
                | "teardown"
                | "observe"
                | "scoped-token"
                | "update"
                | "rotate"
                | "revoke"
                | "detach"
        ) {
            return Err("unsupported operation");
        }
        if !state.records.contains_key(&key) && state.records.len() >= 32 {
            return Err("account quota");
        }
        state.pending.insert(key.clone(), action.to_owned());
        self.persist(&state)?;
        let mut registry =
            Registry::restore(32, state.records.get(&key).cloned().unwrap_or_default())
                .map_err(|_| "registry")?;
        let domain = r.auth_domain.as_deref().unwrap_or(owner);
        let data = match action {
            "scoped-token" => {
                let token = client
                    .create_scoped_token(
                        r.name.as_deref().ok_or("name")?,
                        r.permission_group_id.as_deref().ok_or("permission")?,
                    )
                    .await
                    .map_err(|_| "create token")?;
                let value = token.value.ok_or("missing token secret")?;
                let reference = self
                    .vault
                    .store(
                        owner,
                        &multiview_credentials::Credential::Bearer {
                            token: value.expose().to_owned(),
                        },
                    )
                    .map_err(|_| "token persistence")?;
                serde_json::json!({"secret_ref":reference.as_str()})
            }
            "provision" => {
                let tokens = registry
                    .provision(&client, r.name.as_deref().ok_or("name")?, domain.to_owned())
                    .await
                    .map_err(|_| "provision")?;
                // Persist IDs before storing secrets: failures leave a recoverable relay.
                state
                    .records
                    .insert(key.clone(), registry.records().cloned().collect());
                self.persist(&state)?;
                let mut refs = Vec::new();
                for token in tokens {
                    if let Some(secret) = token.secret {
                        let reference = self
                            .vault
                            .store(
                                owner,
                                &multiview_credentials::Credential::Bearer {
                                    token: secret.expose().to_owned(),
                                },
                            )
                            .map_err(|_| "token persistence")?;
                        refs.push(
                            serde_json::json!({"jti":token.jti,"secret_ref":reference.as_str()}),
                        );
                    }
                }
                serde_json::json!({"tokens":refs})
            }
            "update" => {
                registry
                    .update(
                        &client,
                        r.relay_id.as_deref().ok_or("relay")?,
                        domain,
                        &multiview_cloudflare::RelayUpdate {
                            name: r.name.clone(),
                            config: None,
                        },
                    )
                    .await
                    .map_err(|_| "update")?;
                serde_json::json!({})
            }
            "detach" => {
                registry
                    .detach(r.relay_id.as_deref().ok_or("relay")?, domain)
                    .map_err(|_| "detach")?;
                serde_json::json!({})
            }
            "rotate" => {
                let id = r.relay_id.as_deref().ok_or("relay")?;
                let tokens = registry
                    .begin_rotation(
                        &client,
                        id,
                        domain,
                        r.token_jti.clone().ok_or("token")?,
                        &multiview_cloudflare::TokenRequest {
                            operations: vec![multiview_cloudflare::Operation::Subscribe],
                            expires: None,
                            label: Some("rotation".into()),
                        },
                    )
                    .await
                    .map_err(|_| "rotate")?;
                state
                    .records
                    .insert(key.clone(), registry.records().cloned().collect());
                self.persist(&state)?;
                let mut refs = Vec::new();
                for token in tokens {
                    if let Some(secret) = token.secret {
                        let reference = self
                            .vault
                            .store(
                                owner,
                                &multiview_credentials::Credential::Bearer {
                                    token: secret.expose().to_owned(),
                                },
                            )
                            .map_err(|_| "token persistence")?;
                        refs.push(
                            serde_json::json!({"jti":token.jti,"secret_ref":reference.as_str()}),
                        );
                    }
                }
                serde_json::json!({"tokens":refs,"old_token_still_valid":true})
            }
            "revoke" => {
                registry
                    .finish_rotation(&client, r.relay_id.as_deref().ok_or("relay")?, domain)
                    .await
                    .map_err(|_| "revoke")?;
                serde_json::json!({"connected_sessions_terminated":false})
            }
            "import" => {
                registry
                    .import(
                        &client,
                        r.relay_id.as_deref().ok_or("relay")?,
                        domain.to_owned(),
                    )
                    .await
                    .map_err(|_| "import")?;
                serde_json::json!({})
            }
            "teardown" => {
                registry
                    .delete_managed(&client, r.relay_id.as_deref().ok_or("relay")?, domain)
                    .await
                    .map_err(|_| "teardown")?;
                serde_json::json!({})
            }
            "observe" => {
                let health = registry
                    .observe(&client, r.relay_id.as_deref().ok_or("relay")?, domain)
                    .await
                    .map_err(|_| "observe")?;
                serde_json::json!({"health":health})
            }
            _ => return Err("unsupported operation"),
        };
        state
            .records
            .insert(key.clone(), registry.records().cloned().collect());
        state.pending.remove(&key);
        self.persist(&state)?;
        Ok(CloudflareResult { data })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn durable_policy_and_owner_bound_reference() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cf-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let manager = ManagedCloudflare::from_master_key(&dir, [7; 32]).unwrap();
        let reference = manager
            .save(
                "alice",
                crate::routes::cloudflare::SaveCredential {
                    account_id: "account".into(),
                    kind: "scoped_token".into(),
                    email: None,
                    key: None,
                    token: Some("test-only".into()),
                },
            )
            .unwrap();
        let request = |mode| CloudflareRequest {
            account_id: "account".into(),
            secret_ref: reference.clone(),
            relay_id: None,
            name: None,
            auth_domain: None,
            permission_group_id: None,
            token_jti: None,
            mode,
        };
        assert!(manager
            .run("bob", "set-mode", request(Some("auto".into())))
            .await
            .is_err());
        manager
            .run("alice", "set-mode", request(Some("manual".into())))
            .await
            .unwrap();
        drop(manager);
        let reopened = ManagedCloudflare::from_master_key(&dir, [7; 32]).unwrap();
        let result = reopened
            .run("alice", "relays", request(None))
            .await
            .unwrap();
        assert_eq!(result.data["mode"], "manual");
        assert_eq!(result.data["effective_transport"], "unverified");
        assert!(
            !std::fs::read_to_string(dir.join("cloudflare-journal.json"))
                .unwrap()
                .contains("test-only")
        );
        // No account mutations: manual reconcile remains local.
        reopened
            .run("alice", "reconcile", request(None))
            .await
            .unwrap();
    }
}

impl CloudflareManagement for ManagedCloudflare {
    fn save(
        &self,
        owner: &str,
        r: crate::routes::cloudflare::SaveCredential,
    ) -> Result<String, &'static str> {
        let credential = match r.kind.as_str() {
            "global_key" => multiview_credentials::Credential::GlobalKey {
                email: r.email.ok_or("email")?,
                key: r.key.ok_or("key")?,
            },
            "scoped_token" => multiview_credentials::Credential::Bearer {
                token: r.token.ok_or("token")?,
            },
            _ => return Err("kind"),
        };
        // Validate account shape without making an API request or retaining it in the secret.
        Client::new(
            r.account_id,
            Credential::Bearer {
                token: String::new(),
            },
        )
        .map_err(|_| "account")?;
        self.vault
            .store(owner, &credential)
            .map(|r| r.as_str().to_owned())
            .map_err(|_| "credential persistence")
    }
    fn execute<'a>(
        &'a self,
        owner: &'a str,
        action: &'a str,
        request: CloudflareRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CloudflareResult, &'static str>> + Send + 'a>,
    > {
        Box::pin(self.run(owner, action, request))
    }
}
