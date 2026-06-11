//! AMWA **NMOS IS-10** ("Authorization") token model — OAuth 2.0 (RFC 6749) /
//! JWT (RFC 7519) — extending the crate's API-key auth (broadcast-multiviewer
//! brief §8).
//!
//! IS-10 secures the NMOS APIs with OAuth 2.0 **bearer JWTs**: an access token
//! carries the standard registered claims (`iss`, `aud`, `exp`, `iat`, `sub`)
//! plus the NMOS-private **`x-nmos-api`** claim granting per-API
//! `read`/`write`/`*` access. Multiview accepts such a token alongside its native
//! API keys and maps its NMOS permissions onto the crate's [`Role`].
//!
//! This module is the **pure claims model + validation logic**: the registered-
//! claim checks (issuer, audience, expiry) and the NMOS-permission → [`Role`]
//! mapping. The **signature** verification (RS256/ES256 against the auth
//! server's JWKS) is a cryptographic/transport concern done at the gated `nmos`
//! boundary with the deployment's key material — it is documented here and not
//! performed by this pure model (which therefore validates *claims*, never trusts
//! an unsigned token on its own).
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::auth::Role;

/// The NMOS access level granted for one API (the `x-nmos-api` value vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NmosAccess {
    /// Read-only access.
    Read,
    /// Read + write access.
    Write,
}

impl NmosAccess {
    /// Parse the `x-nmos-api` access string (`"read"` / `"write"`).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }
}

/// The NMOS-private `x-nmos-api` claim: per-API access grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NmosApiClaim {
    /// The token-format version (e.g. `"1.0"`).
    #[serde(default)]
    pub version: String,
    /// Per-API access map, e.g. `{"connection": "write", "query": "read"}`.
    #[serde(default)]
    pub access: BTreeMap<String, NmosAccess>,
}

impl NmosApiClaim {
    /// The access level granted for one named API, if any.
    #[must_use]
    pub fn access_for(&self, api: &str) -> Option<NmosAccess> {
        // An explicit per-API grant wins; otherwise a `*` wildcard applies.
        self.access
            .get(api)
            .copied()
            .or_else(|| self.access.get("*").copied())
    }
}

/// An IS-10 JWT **claims set** (the decoded token body).
///
/// Carries the registered claims IS-10 mandates plus the NMOS `x-nmos-api`
/// claim. Times are Unix seconds (the JWT `NumericDate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Is10Claims {
    /// The issuer (the authorization server) — `iss`.
    pub iss: String,
    /// The subject (the authenticated user/client) — `sub`.
    pub sub: String,
    /// The audience (the resource server this token is for) — `aud`.
    ///
    /// IS-10 allows a single audience or a list; a single value is modelled as a
    /// one-element list on decode by the caller.
    pub aud: Vec<String>,
    /// Expiry, Unix seconds — `exp`.
    pub exp: i64,
    /// Issued-at, Unix seconds — `iat`.
    #[serde(default)]
    pub iat: i64,
    /// The NMOS per-API access grant — `x-nmos-api`.
    #[serde(rename = "x-nmos-api", default)]
    pub x_nmos_api: NmosApiClaim,
}

/// Why an IS-10 token's claims were rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Is10Error {
    /// The token's `iss` did not match the expected authorization server.
    #[error("token issuer {found:?} does not match expected {expected:?}")]
    WrongIssuer {
        /// The issuer the token carried.
        found: String,
        /// The issuer the resource server requires.
        expected: String,
    },
    /// The token's `aud` did not include this resource server.
    #[error("token audience does not include {expected:?}")]
    WrongAudience {
        /// The audience this resource server requires.
        expected: String,
    },
    /// The token has expired (`exp` <= now).
    #[error("token expired at {exp} (now {now})")]
    Expired {
        /// The token's expiry (Unix seconds).
        exp: i64,
        /// The validation time (Unix seconds).
        now: i64,
    },
    /// The token granted no usable access to the requested API.
    #[error("token grants no access to API {api:?}")]
    NoAccess {
        /// The API name the token lacked a grant for.
        api: String,
    },
}

impl Is10Claims {
    /// Validate the registered claims against this resource server's policy.
    ///
    /// Checks issuer, audience, and expiry. **Signature verification is assumed
    /// to have already succeeded** at the gated transport boundary — this method
    /// validates the *claims* of an already-authenticated token.
    ///
    /// # Errors
    ///
    /// [`Is10Error::WrongIssuer`] / [`Is10Error::WrongAudience`] /
    /// [`Is10Error::Expired`] on the corresponding policy failure.
    pub fn validate(
        &self,
        now: i64,
        expected_issuer: &str,
        expected_audience: &str,
    ) -> Result<(), Is10Error> {
        if self.iss != expected_issuer {
            return Err(Is10Error::WrongIssuer {
                found: self.iss.clone(),
                expected: expected_issuer.to_owned(),
            });
        }
        if !self.aud.iter().any(|a| a == expected_audience) {
            return Err(Is10Error::WrongAudience {
                expected: expected_audience.to_owned(),
            });
        }
        if self.exp <= now {
            return Err(Is10Error::Expired { exp: self.exp, now });
        }
        Ok(())
    }

    /// Map the NMOS access granted for `api` onto the crate's [`Role`].
    ///
    /// `write` → [`Role::Operator`] (read + write, no destructive admin); `read`
    /// → [`Role::Viewer`]. There is deliberately no path from an IS-10 token to
    /// [`Role::Admin`]: administrative key management stays on the native API-key
    /// surface (least privilege).
    ///
    /// # Errors
    ///
    /// [`Is10Error::NoAccess`] if the token grants no access to `api`.
    pub fn role_for(&self, api: &str) -> Result<Role, Is10Error> {
        match self.x_nmos_api.access_for(api) {
            Some(NmosAccess::Write) => Ok(Role::Operator),
            Some(NmosAccess::Read) => Ok(Role::Viewer),
            None => Err(Is10Error::NoAccess {
                api: api.to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{Is10Claims, Is10Error, NmosAccess, NmosApiClaim};
    use crate::auth::Role;
    use std::collections::BTreeMap;

    fn claims() -> Is10Claims {
        let mut access = BTreeMap::new();
        access.insert("connection".to_owned(), NmosAccess::Write);
        access.insert("query".to_owned(), NmosAccess::Read);
        Is10Claims {
            iss: "https://auth.facility.example".to_owned(),
            sub: "operator-7".to_owned(),
            aud: vec!["multiview".to_owned()],
            exp: 2_000_000_000,
            iat: 1_700_000_000,
            x_nmos_api: NmosApiClaim {
                version: "1.0".to_owned(),
                access,
            },
        }
    }

    #[test]
    fn claims_round_trip_through_json_with_the_nmos_claim_name() {
        let c = claims();
        let json = serde_json::to_value(&c).unwrap();
        // The NMOS claim serialises under its dotted-private name.
        assert_eq!(json["x-nmos-api"]["access"]["connection"], "write");
        assert_eq!(json["aud"][0], "multiview");
        let back: Is10Claims = serde_json::from_value(json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn valid_token_passes_issuer_audience_expiry() {
        let c = claims();
        assert!(c
            .validate(1_800_000_000, "https://auth.facility.example", "multiview")
            .is_ok());
    }

    #[test]
    fn wrong_issuer_is_rejected() {
        let err = claims()
            .validate(1_800_000_000, "https://evil.example", "multiview")
            .unwrap_err();
        assert!(matches!(err, Is10Error::WrongIssuer { .. }), "{err:?}");
    }

    #[test]
    fn wrong_audience_is_rejected() {
        let err = claims()
            .validate(
                1_800_000_000,
                "https://auth.facility.example",
                "other-server",
            )
            .unwrap_err();
        assert!(matches!(err, Is10Error::WrongAudience { .. }), "{err:?}");
    }

    #[test]
    fn expired_token_is_rejected() {
        let err = claims()
            .validate(2_000_000_001, "https://auth.facility.example", "multiview")
            .unwrap_err();
        assert!(matches!(err, Is10Error::Expired { .. }), "{err:?}");
    }

    #[test]
    fn write_access_maps_to_operator_and_read_to_viewer() {
        let c = claims();
        assert_eq!(c.role_for("connection").unwrap(), Role::Operator);
        assert_eq!(c.role_for("query").unwrap(), Role::Viewer);
    }

    #[test]
    fn missing_api_grant_is_no_access() {
        let err = claims().role_for("registration").unwrap_err();
        assert!(matches!(err, Is10Error::NoAccess { .. }), "{err:?}");
    }

    #[test]
    fn wildcard_grant_applies_to_any_api() {
        let mut access = BTreeMap::new();
        access.insert("*".to_owned(), NmosAccess::Read);
        let claim = NmosApiClaim {
            version: "1.0".to_owned(),
            access,
        };
        assert_eq!(claim.access_for("anything"), Some(NmosAccess::Read));
    }

    #[test]
    fn nmos_access_parses_the_wire_vocabulary() {
        assert_eq!(NmosAccess::parse("read"), Some(NmosAccess::Read));
        assert_eq!(NmosAccess::parse("write"), Some(NmosAccess::Write));
        assert_eq!(NmosAccess::parse("admin"), None);
    }
}
