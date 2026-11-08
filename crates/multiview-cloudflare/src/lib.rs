//! Cloudflare management-plane client. Never call from an output clock or media task.
//! All calls have deadlines; mutations are never automatically retried.
#![forbid(unsafe_code)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown
)]
use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};

#[derive(Clone, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub enum Credential {
    Bearer { token: String },
    GlobalKey { email: String, key: String },
}
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential([REDACTED])")
    }
}
#[derive(Clone, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
#[serde(transparent)]
pub struct Secret(String);
impl Secret {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid management request")]
    Invalid,
    #[error("management transport unavailable; mutation outcome may be unknown")]
    Transport,
    #[error("management API rejected request (HTTP {0})")]
    Api(u16),
    #[error("invalid management API response")]
    Response,
    #[error("authorization domain conflict or quota exceeded")]
    Isolation,
    #[error("imported resource cannot be destroyed automatically")]
    Imported,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RelayConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstreams: Option<Upstreams>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upstreams {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub upstreams: Vec<Upstream>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upstream {
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relay {
    pub uid: String,
    pub name: String,
    #[serde(default, deserialize_with = "nullable_config")]
    pub config: RelayConfig,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, skip_serializing)]
    pub issuers: Vec<Issuer>,
}
fn nullable_config<'de, D: serde::Deserializer<'de>>(d: D) -> Result<RelayConfig, D::Error> {
    Ok(Option::<RelayConfig>::deserialize(d)?.unwrap_or_default())
}
#[derive(Debug, Clone, Deserialize)]
pub struct Issuer {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub cloudflare_tokens: Vec<Token>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Token {
    pub jti: String,
    pub operations: Vec<Operation>,
    pub expires: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, skip_serializing)]
    pub secret: Option<Secret>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Publish,
    Subscribe,
}
#[derive(Debug, Clone, Serialize, Default)]
pub struct RelayUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<RelayConfig>,
}
#[derive(Debug, Clone, Serialize)]
pub struct TokenRequest {
    pub operations: Vec<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
#[derive(Deserialize)]
struct Envelope {
    success: bool,
    #[serde(default)]
    result: serde_json::Value,
    #[serde(default)]
    result_info: Option<account::PageInfo>,
}
#[derive(Deserialize)]
struct Tokens {
    issuers: Vec<Issuer>,
}

/// The origin is fixed. Redirects are disabled to keep credentials on Cloudflare.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    account: String,
    credential: Credential,
    #[cfg(test)]
    test_origin: Option<String>,
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
impl Client {
    /// Resolve credentials from the owner's `secret_ref` vault before constructing this client.
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub fn new(account: String, credential: Credential) -> Result<Self, Error> {
        if !valid_id(&account) {
            return Err(Error::Invalid);
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self {
            http,
            account,
            credential,
            #[cfg(test)]
            test_origin: None,
        })
    }
    fn url(&self, relay: Option<&str>, token: Option<&str>, tokens: bool) -> Result<String, Error> {
        if relay.is_some_and(|s| !valid_id(s)) || token.is_some_and(|s| !valid_id(s)) {
            return Err(Error::Invalid);
        }
        let mut u = format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/moq/relays",
            self.account
        );
        if let Some(r) = relay {
            u.push('/');
            u.push_str(r);
        }
        if tokens {
            u.push_str("/tokens");
        }
        if let Some(t) = token {
            u.push('/');
            u.push_str(t);
        }
        Ok(u)
    }
    async fn call(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, Error> {
        Ok(self.call_envelope(method, url, body).await?.result)
    }
    async fn call_envelope(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<Envelope, Error> {
        #[cfg(test)]
        let url = if let Some(origin) = &self.test_origin {
            url.replace("https://api.cloudflare.com", origin)
        } else {
            url
        };
        let mut r = self.http.request(method, url);
        r = match &self.credential {
            Credential::Bearer { token } => r.bearer_auth(token),
            Credential::GlobalKey { email, key } => {
                r.header("X-Auth-Email", email).header("X-Auth-Key", key)
            }
        };
        if let Some(body) = body {
            r = r.json(&body);
        }
        let mut response = r.send().await.map_err(|_| Error::Transport)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Error::Api(status));
        }
        // Do not trust Content-Length. Bound streamed allocation as well.
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(Error::Response);
            }
            bytes.extend_from_slice(&chunk);
        }
        decode(&bytes, status)?;
        serde_json::from_slice(&bytes).map_err(|_| Error::Response)
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn list(&self) -> Result<Vec<Relay>, Error> {
        parse(
            self.call(reqwest::Method::GET, self.url(None, None, false)?, None)
                .await?,
        )
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn get(&self, id: &str) -> Result<Relay, Error> {
        parse(
            self.call(reqwest::Method::GET, self.url(Some(id), None, false)?, None)
                .await?,
        )
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn create(&self, name: &str) -> Result<Relay, Error> {
        if name.trim().is_empty() {
            return Err(Error::Invalid);
        }
        parse(
            self.call(
                reqwest::Method::POST,
                self.url(None, None, false)?,
                Some(serde_json::json!({"name":name})),
            )
            .await?,
        )
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn update(&self, id: &str, update: &RelayUpdate) -> Result<Relay, Error> {
        if update.name.as_ref().is_some_and(|s| s.trim().is_empty()) {
            return Err(Error::Invalid);
        }
        parse(
            self.call(
                reqwest::Method::PUT,
                self.url(Some(id), None, false)?,
                Some(serde_json::to_value(update).map_err(|_| Error::Invalid)?),
            )
            .await?,
        )
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn delete(&self, id: &str) -> Result<(), Error> {
        self.call(
            reqwest::Method::DELETE,
            self.url(Some(id), None, false)?,
            None,
        )
        .await?;
        Ok(())
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn tokens(&self, id: &str) -> Result<Vec<Token>, Error> {
        token_result(
            self.call(reqwest::Method::GET, self.url(Some(id), None, true)?, None)
                .await?,
        )
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn create_token(
        &self,
        id: &str,
        request: &TokenRequest,
    ) -> Result<Vec<Token>, Error> {
        if request.operations.is_empty() {
            return Err(Error::Invalid);
        }
        token_result(
            self.call(
                reqwest::Method::POST,
                self.url(Some(id), None, true)?,
                Some(serde_json::to_value(request).map_err(|_| Error::Invalid)?),
            )
            .await?,
        )
    }
    /// # Errors
    /// Returns a sanitized error for invalid input, ownership conflicts, or API failure.
    pub async fn revoke_token(&self, id: &str, jti: &str) -> Result<(), Error> {
        self.call(
            reqwest::Method::DELETE,
            self.url(Some(id), Some(jti), true)?,
            None,
        )
        .await?;
        Ok(())
    }
}
fn decode(bytes: &[u8], status: u16) -> Result<serde_json::Value, Error> {
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| Error::Response)?;
    if !envelope.success {
        return Err(Error::Api(status));
    }
    Ok(envelope.result)
}
fn parse<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, Error> {
    serde_json::from_value(v).map_err(|_| Error::Response)
}
fn token_result(v: serde_json::Value) -> Result<Vec<Token>, Error> {
    Ok(parse::<Tokens>(v)?
        .issuers
        .into_iter()
        .filter(|i| i.kind == "cloudflare_jwt")
        .flat_map(|i| i.cloudflare_tokens)
        .collect())
}

pub mod account;
pub mod lifecycle;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_cloudflare_null_config_is_accepted() {
        let relay: Relay =
            parse(serde_json::json!({"uid":"fixture","name":"disposable","config":null})).unwrap();
        assert!(relay.config.upstreams.is_none());
        let relay: Relay = parse(
            serde_json::json!({"uid":"fixture","name":"disposable","config":{"upstreams":{}}}),
        )
        .unwrap();
        assert!(!relay.config.upstreams.unwrap().enabled);
    }
    #[test]
    fn secrets_do_not_serialize_or_debug() {
        let t: Token = parse(serde_json::json!({"jti":"id","operations":["subscribe"],"expires":"later","secret":"sensitive"})).unwrap();
        assert!(!format!("{t:?}").contains("sensitive"));
        assert!(!serde_json::to_string(&t).unwrap().contains("sensitive"));
    }
    #[test]
    fn ids_cannot_escape_origin() {
        let c = Client::new(
            "account".into(),
            Credential::Bearer {
                token: "secret".into(),
            },
        )
        .unwrap();
        for id in ["../evil", "x?secret", "x/y", "", "https://evil"] {
            assert!(c.url(Some(id), None, false).is_err());
        }
    }
    #[test]
    fn error_bodies_are_not_exposed() {
        let e = decode(br#"{"success":false,"errors":[{"message":"secret"}]}"#, 200).unwrap_err();
        assert_eq!(e, Error::Api(200));
        assert!(!e.to_string().contains("secret"));
    }
    #[tokio::test]
    async fn real_http_transport_sends_auth_and_exact_create_shape() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0; 8192];
            let n = stream.read(&mut bytes).unwrap();
            let text = String::from_utf8_lossy(&bytes[..n]).to_string();
            assert!(text.starts_with("POST /client/v4/accounts/account/moq/relays HTTP/1.1"));
            assert!(text
                .to_lowercase()
                .contains("authorization: bearer test-secret"));
            let body = r#"{"success":true,"result":{"uid":"relay","name":"demo","config":{},"issuers":[]}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            text
        });
        let mut c = Client::new(
            "account".into(),
            Credential::Bearer {
                token: "test-secret".into(),
            },
        )
        .unwrap();
        c.test_origin = Some(origin);
        assert_eq!(c.create("demo").await.unwrap().uid, "relay");
        let text = server.join().unwrap();
        assert!(text.contains(r#"{"name":"demo"}"#));
        assert!(!text.contains("config"));
    }
    fn origin_server(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        origin
    }
    async fn client(origin: String) -> Client {
        let mut c = Client::new(
            "account".into(),
            Credential::Bearer {
                token: "t".into(),
            },
        )
        .unwrap();
        c.test_origin = Some(origin);
        c
    }
    #[tokio::test]
    async fn documented_list_shape_and_create_secret_roundtrip() {
        let list_body = r#"{"success":true,"result":[{"created":"2019-12-27T18:11:19.117Z","modified":"2019-12-27T18:11:19.117Z","name":"name","uid":"a1b2c3d4e5f67890a1b2c3d4e5f67890"}],"result_info":{"count":1,"total":1}}"#;
        match client(origin_server(list_body)).await.list().await {
            Ok(v) => panic!("LIST_OK count={}", v.len()),
            Err(e) => eprintln!("LIST_ERR {e:?}"),
        }
        let create_body = r#"{"success":true,"result":{"uid":"relayuid","name":"demo","config":{"upstreams":{"enabled":false,"upstreams":[]}},"issuers":[{"type":"cloudflare_jwt","issuer":"cloudflare","cloudflare_tokens":[{"jti":"abc","operations":["subscribe"],"expires":"2030-01-01T00:00:00Z","secret":"ONCE-SECRET-JWT"}]}]}}"#;
        let relay = client(origin_server(create_body))
            .await
            .create("demo")
            .await
            .expect("create");
        let dumped = serde_json::to_string(&relay).unwrap();
        eprintln!(
            "CREATE secrets_in_memory={} serialized_contains_secret={} dumped={dumped}",
            relay
                .issuers
                .iter()
                .flat_map(|i| i.cloudflare_tokens.iter())
                .any(|t| t.secret.is_some()),
            dumped.contains("ONCE-SECRET-JWT")
        );
        let get_body = r#"{"success":true,"result":{"config":{"upstreams":{"enabled":true,"upstreams":[{"url":"https://example.com"}]}},"created":"2019-12-27T18:11:19.117Z","modified":"2019-12-27T18:11:19.117Z","name":"Production","uid":"a1b2c3d4e5f67890a1b2c3d4e5f67890","status":"connected"}}"#;
        match client(origin_server(get_body)).await.get("relayuid").await {
            Ok(r) => eprintln!("GET_OK status={:?}", r.status),
            Err(e) => eprintln!("GET_ERR {e:?}"),
        }
        let tok_body = r#"{"success":true,"result":{"issuers":[{"type":"cloudflare_jwt","cloudflare_tokens":[{"jti":"j1","operations":["subscribe"],"expires":"2030-01-01T00:00:00Z","secret":"ROTATE-SECRET"}]}]}}"#;
        let tokens = client(origin_server(tok_body))
            .await
            .create_token(
                "relayuid",
                &TokenRequest {
                    operations: vec![Operation::Subscribe],
                    expires: None,
                    label: None,
                },
            )
            .await
            .expect("token");
        let dumped = serde_json::to_string(&tokens).unwrap();
        eprintln!(
            "TOKEN secret_in_memory={} serialized_contains={}",
            tokens.iter().any(|t| t.secret.is_some()),
            dumped.contains("ROTATE-SECRET")
        );
        let audit_body = r#"{"success":true,"result":[{"id":"tok","name":"n","status":"active","policies":[{"resources":{"com.cloudflare.api.account.other":"*"}}],"value":"SHOULD-NOT-LEAK"}],"result_info":{"page":1,"total_count":1}}"#;
        let audit = client(origin_server(audit_body))
            .await
            .audit_user_tokens()
            .await
            .expect("audit");
        let dumped = serde_json::to_string(&audit).unwrap();
        eprintln!(
            "AUDIT serialized_contains_value={} dumped={dumped}",
            dumped.contains("SHOULD-NOT-LEAK")
        );
        panic!("probe-complete");
    }
}
