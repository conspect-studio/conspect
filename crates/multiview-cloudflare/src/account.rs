//! User API token onboarding. No mutation runs as a consequence of an audit.
use crate::{parse, Client, Error, Secret};
use serde::{Deserialize, Serialize};
#[derive(Deserialize)]
pub(crate) struct PageInfo {
    #[serde(default)]
    pub page: u32,
    #[serde(default)]
    pub total_count: u32,
    pub count: Option<usize>,
    pub total: Option<usize>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct UserToken {
    pub id: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub policies: Option<serde_json::Value>,
    #[serde(default, skip_serializing)]
    pub value: Option<Secret>,
}
#[derive(Debug, Serialize)]
pub struct Audit {
    pub tokens: Vec<UserToken>,
    pub all_pages_loaded: bool,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct PermissionGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub is_selectable: Option<bool>,
    #[serde(default)]
    pub scopes: Vec<String>,
}
fn check_capacity(complete: bool, count: usize) -> Result<(), Error> {
    if !complete || count >= 50 {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}
impl Client {
    /// Hard cap bounds both memory and management traffic. Incomplete audits never
    /// claim capacity or suitability. Existing token secrets cannot be recovered.
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn audit_user_tokens(&self) -> Result<Audit, Error> {
        let mut tokens = Vec::new();
        for page in 1..=20 {
            let envelope = self.call_envelope(reqwest::Method::GET,format!("https://api.cloudflare.com/client/v4/user/tokens?page={page}&per_page=50&include_expired=true"),None).await?;
            let entries: Vec<UserToken> = parse(envelope.result)?;
            let count = entries.len();
            tokens.extend(entries);
            let Some(info) = envelope.result_info else {
                return Ok(Audit {
                    tokens,
                    all_pages_loaded: false,
                });
            };
            if info.page != page || count > 50 {
                return Err(Error::Response);
            }
            if tokens.len() >= usize::try_from(info.total_count).map_err(|_| Error::Response)? {
                return Ok(Audit {
                    tokens,
                    all_pages_loaded: true,
                });
            }
            if count == 0 {
                break;
            }
        }
        Ok(Audit {
            tokens,
            all_pages_loaded: false,
        })
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn permission_groups(&self) -> Result<Vec<PermissionGroup>, Error> {
        parse(
            self.call(
                reqwest::Method::GET,
                "https://api.cloudflare.com/client/v4/user/tokens/permission_groups".into(),
                None,
            )
            .await?,
        )
    }
    /// Explicit user choice only. Re-fetch live permission metadata and constrain
    /// resources to this account. Never accept arbitrary user-supplied policies.
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn create_scoped_token(
        &self,
        name: &str,
        permission_id: &str,
    ) -> Result<UserToken, Error> {
        if name.is_empty() || name.len() > 120 {
            return Err(Error::Invalid);
        }
        let audit = self.audit_user_tokens().await?;
        check_capacity(audit.all_pages_loaded, audit.tokens.len())?;
        let groups = self.permission_groups().await?;
        let group = groups
            .iter()
            .find(|g| g.id == permission_id)
            .ok_or(Error::Invalid)?;
        // Fail closed on unknown permission names/scopes; never guess an ID.
        if group.is_selectable != Some(true)
            || !matches!(group.name.as_str(), "MoQ Write" | "MoQ Edit")
            || !group
                .scopes
                .iter()
                .any(|s| s == "com.cloudflare.api.account")
        {
            return Err(Error::Invalid);
        }
        let resource = format!("com.cloudflare.api.account.{}", self.account);
        parse(self.call(reqwest::Method::POST,"https://api.cloudflare.com/client/v4/user/tokens".into(),Some(serde_json::json!({"name":name,"policies":[{"effect":"allow","permission_groups":[{"id":group.id}],"resources":{resource:"*"}}]}))).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_requires_complete_audit_and_fewer_than_fifty() {
        assert!(check_capacity(true, 49).is_ok());
        assert_eq!(check_capacity(true, 50), Err(Error::Invalid));
        assert_eq!(check_capacity(false, 0), Err(Error::Invalid));
    }
}
