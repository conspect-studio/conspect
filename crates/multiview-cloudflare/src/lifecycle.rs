//! Explicit operator-driven lifecycle, independent of media connectivity.
//! Persist records after every successful step. Unknown POST outcomes require
//! operator reconciliation; never repeat a create automatically.
use crate::{Client, Error, Relay, RelayUpdate, Token, TokenRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Ownership {
    Managed,
    Imported,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Health {
    Unknown,
    Reachable,
    Unavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub relay: Relay,
    pub ownership: Ownership,
    /// This is the complete authorization set, not a canvas namespace prefix.
    pub auth_domain: String,
    pub health: Health,
    pub pending_revoke: Option<String>,
}
/// Registry enforces exclusive relay allocation to an authorization domain.
/// A domain must contain only resources accessible to every member of that domain.
/// Callers must authorize domain membership before returning any token.
#[derive(Debug)]
pub struct Registry {
    records: BTreeMap<String, Record>,
    quota: usize,
}
impl Registry {
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    #[must_use]
    pub fn new(quota: usize) -> Self {
        Self {
            records: BTreeMap::new(),
            quota,
        }
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub fn restore(quota: usize, records: Vec<Record>) -> Result<Self, Error> {
        let mut registry = Self::new(quota);
        for record in records {
            registry.insert(record)?;
        }
        Ok(registry)
    }
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub fn record(&self, id: &str, domain: &str) -> Result<&Record, Error> {
        self.records
            .get(id)
            .filter(|r| r.auth_domain == domain)
            .ok_or(Error::Isolation)
    }
    fn insert(&mut self, record: Record) -> Result<(), Error> {
        if record.auth_domain.is_empty()
            || self.records.contains_key(&record.relay.uid)
            || self.records.len() >= self.quota
        {
            return Err(Error::Isolation);
        }
        self.records.insert(record.relay.uid.clone(), record);
        Ok(())
    }
    /// Manual import verifies existence without changing the Cloudflare resource.
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn import(&mut self, client: &Client, id: &str, domain: String) -> Result<(), Error> {
        if domain.is_empty() || self.records.contains_key(id) || self.records.len() >= self.quota {
            return Err(Error::Isolation);
        }
        let relay = client.get(id).await?;
        self.insert(Record {
            relay,
            ownership: Ownership::Imported,
            auth_domain: domain,
            health: Health::Reachable,
            pending_revoke: None,
        })
    }
    /// Explicit product choice only. No scheduler invokes provisioning automatically.
    /// Return auto-created secrets once so the caller can persist them in its vault.
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn provision(
        &mut self,
        client: &Client,
        name: &str,
        domain: String,
    ) -> Result<Vec<Token>, Error> {
        if domain.is_empty() || self.records.len() >= self.quota {
            return Err(Error::Isolation);
        }
        let mut relay = client.create(name).await?;
        let tokens = std::mem::take(&mut relay.issuers)
            .into_iter()
            .filter(|i| i.kind == "cloudflare_jwt")
            .flat_map(|i| i.cloudflare_tokens)
            .collect();
        self.insert(Record {
            relay,
            ownership: Ownership::Managed,
            auth_domain: domain,
            health: Health::Reachable,
            pending_revoke: None,
        })?;
        Ok(tokens)
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn update(
        &mut self,
        client: &Client,
        id: &str,
        domain: &str,
        update: &RelayUpdate,
    ) -> Result<(), Error> {
        self.record(id, domain)?;
        let relay = client.update(id, update).await?;
        self.records.get_mut(id).ok_or(Error::Isolation)?.relay = relay;
        Ok(())
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn delete_managed(
        &mut self,
        client: &Client,
        id: &str,
        domain: &str,
    ) -> Result<(), Error> {
        if self.record(id, domain)?.ownership != Ownership::Managed {
            return Err(Error::Imported);
        }
        // Preserve the record on timeout so recovery can inspect remote existence.
        match client.delete(id).await {
            Ok(()) | Err(Error::Api(404)) => {}
            Err(e) => return Err(e),
        }
        self.records.remove(id);
        Ok(())
    }
    /// Forget imported ownership only; never delete a user's pre-existing relay.
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub fn detach(&mut self, id: &str, domain: &str) -> Result<(), Error> {
        if self.record(id, domain)?.ownership != Ownership::Imported {
            return Err(Error::Invalid);
        }
        self.records.remove(id);
        Ok(())
    }
    /// Rotation phase one: mint a replacement. The old token remains valid.
    /// Caller must save the new secret, install it, and persist `pending_revoke`
    /// before calling `finish_rotation`. Repeating this phase is not automatic.
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn begin_rotation(
        &mut self,
        client: &Client,
        id: &str,
        domain: &str,
        old_jti: String,
        request: &TokenRequest,
    ) -> Result<Vec<Token>, Error> {
        if self.record(id, domain)?.pending_revoke.is_some() {
            return Err(Error::Invalid);
        }
        let tokens = client.create_token(id, request).await?;
        self.records
            .get_mut(id)
            .ok_or(Error::Isolation)?
            .pending_revoke = Some(old_jti);
        Ok(tokens)
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn finish_rotation(
        &mut self,
        client: &Client,
        id: &str,
        domain: &str,
    ) -> Result<(), Error> {
        let jti = self
            .record(id, domain)?
            .pending_revoke
            .clone()
            .ok_or(Error::Invalid)?;
        match client.revoke_token(id, &jti).await {
            Ok(()) | Err(Error::Api(404)) => {}
            Err(e) => return Err(e),
        }
        self.records
            .get_mut(id)
            .ok_or(Error::Isolation)?
            .pending_revoke = None;
        Ok(())
    }
    /// One bounded observation, with no create/delete/retry and no media callbacks.
    /// API reachability is not a claim about on-air media health.
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn observe(
        &mut self,
        client: &Client,
        id: &str,
        domain: &str,
    ) -> Result<Health, Error> {
        self.record(id, domain)?;
        let result = client.get(id).await;
        let record = self.records.get_mut(id).ok_or(Error::Isolation)?;
        match result {
            Ok(relay) => {
                record.relay = relay;
                record.health = Health::Reachable;
            }
            Err(_) => record.health = Health::Unavailable,
        }
        Ok(record.health)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, domain: &str) -> Record {
        Record {
            relay: Relay {
                uid: id.into(),
                name: "test".into(),
                config: Default::default(),
                status: None,
                issuers: vec![],
            },
            ownership: Ownership::Imported,
            auth_domain: domain.into(),
            health: Health::Unknown,
            pending_revoke: None,
        }
    }
    #[test]
    fn isolation_and_quota_fail_closed() {
        let mut r = Registry::new(1);
        r.insert(record("one", "alice")).unwrap();
        assert!(r.record("one", "bob").is_err());
        assert_eq!(r.insert(record("one", "bob")), Err(Error::Isolation));
        assert_eq!(r.insert(record("two", "alice")), Err(Error::Isolation));
    }
    #[test]
    fn restore_enforces_isolation() {
        assert!(Registry::restore(10, vec![record("one", "alice"), record("one", "bob")]).is_err());
    }
    #[test]
    fn detach_checks_domain() {
        let mut r = Registry::restore(2, vec![record("one", "alice")]).unwrap();
        assert_eq!(r.detach("one", "bob"), Err(Error::Isolation));
        r.detach("one", "alice").unwrap();
        assert_eq!(r.records().count(), 0);
    }
}
