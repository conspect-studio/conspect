//! Typed validation of source/output/overlay/probe resource bodies (ADR-W015).
//!
//! The resource stores keep bodies as opaque JSON so optional fields round-trip
//! losslessly, but the API boundary refuses documents the engine could never
//! run: every `POST`/`PUT` body must deserialize against the canonical
//! `multiview_config` type for its collection. Failures become
//! `422 /problems/validation` with the offending field path
//! (`serde_path_to_error`), so a client knows exactly which field to fix.
//!
//! The resource id is part of the contract: a body may omit `id` (the path id
//! is injected) but a present, different `id` is rejected — one document, one
//! address.
use axum::http::{header::HeaderName, HeaderValue};
use axum::response::Response;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::ControlError;

/// The response header declaring how a stored mutation takes effect
/// (ADR-W015 §4, ADR-W018). `live` when the change was handed to the running
/// engine (the engine applies it at a frame boundary); `restart` when the
/// stored document only takes effect via config export + restart.
pub(crate) const APPLY_HEADER: HeaderName = HeaderName::from_static("x-multiview-apply");

/// How a stored mutation reaches the running engine (ADR-W018, inv #11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyMode {
    /// The mutation was enqueued for the running engine's frame-boundary drain.
    Live,
    /// The stored document takes effect via config export + restart.
    Restart,
}

impl ApplyMode {
    /// The wire value carried by [`APPLY_HEADER`].
    const fn header_value(self) -> HeaderValue {
        match self {
            Self::Live => HeaderValue::from_static("live"),
            Self::Restart => HeaderValue::from_static("restart"),
        }
    }
}

/// Stamp a mutation response with its apply semantics (ADR-W018).
pub(crate) fn with_apply(mode: ApplyMode, mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(APPLY_HEADER, mode.header_value());
    response
}

/// Mark a mutation response as taking effect via config export + restart.
pub(crate) fn with_apply_restart(response: Response) -> Response {
    with_apply(ApplyMode::Restart, response)
}

/// The collections this module validates, naming the target config type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypedCollection {
    /// `multiview_config::Source`.
    Sources,
    /// `multiview_config::Output`.
    Outputs,
    /// `multiview_config::Overlay`.
    Overlays,
    /// `multiview_config::Probe`.
    Probes,
    /// `multiview_config::Device`.
    Devices,
    /// `multiview_config::SyncGroup`.
    SyncGroups,
}

impl TypedCollection {
    /// The collection name used in validation messages.
    fn name(self) -> &'static str {
        match self {
            Self::Sources => "source",
            Self::Outputs => "output",
            Self::Overlays => "overlay",
            Self::Probes => "probe",
            Self::Devices => "device",
            Self::SyncGroups => "sync-group",
        }
    }
}

/// Validate `body` against the collection's config type, returning the body to
/// store (with the path `id` injected when the body omitted it).
///
/// # Errors
///
/// [`ControlError::Validation`] when the body is not an object, carries an `id`
/// different from the path id, or fails typed deserialization — the detail
/// names the field path and the serde message.
pub(crate) fn validated_body(
    collection: TypedCollection,
    id: &str,
    body: &Value,
) -> Result<Value, ControlError> {
    let Some(map) = body.as_object() else {
        return Err(ControlError::Validation(format!(
            "{} body must be a JSON object",
            collection.name()
        )));
    };
    let mut map = map.clone();
    // Sources/overlays/probes: the body `id` IS the resource address — inject
    // the path id when omitted, reject a mismatch. Outputs are a DIFFERENT namespace:
    // their config-level `id` is optional, label-derived, and routable
    // (`OutputRef`), so it is preserved verbatim and never compared to the
    // store id.
    if collection != TypedCollection::Outputs {
        match map.get("id").and_then(Value::as_str) {
            Some(body_id) if body_id != id => {
                return Err(ControlError::Validation(format!(
                    "{} body id {body_id:?} does not match the resource id {id:?}",
                    collection.name()
                )));
            }
            Some(_) => {}
            None => {
                if map.contains_key("id") {
                    return Err(ControlError::Validation(format!(
                        "{} body `id` must be a string",
                        collection.name()
                    )));
                }
                map.insert("id".to_owned(), Value::String(id.to_owned()));
            }
        }
    }
    let candidate = Value::Object(map);
    // Typed deserialization first (field path on failure), then the per-item
    // semantic checks `MultiviewConfig::validate()` would apply — a well-typed
    // but semantically invalid document must not enter the store, where it
    // would poison `GET /config/export` for every client.
    match collection {
        TypedCollection::Sources => {
            let source = typecheck::<multiview_config::Source>(collection, &candidate)?;
            source
                .validate()
                .map_err(|err| ControlError::Validation(format!("source body invalid: {err}")))?;
        }
        TypedCollection::Outputs => {
            let output = typecheck::<multiview_config::Output>(collection, &candidate)?;
            output
                .validate()
                .map_err(|err| ControlError::Validation(format!("output body invalid: {err}")))?;
        }
        TypedCollection::Overlays => {
            typecheck::<multiview_config::Overlay>(collection, &candidate)?;
        }
        TypedCollection::Probes => {
            let probe = typecheck::<multiview_config::Probe>(collection, &candidate)?;
            // Per-item semantic checks (zone geometry, threshold ranges, finite
            // levels). Cell-reference resolution needs the cell set and stays a
            // document-level concern (`MultiviewConfig::validate`, run by
            // `GET /config/export` before render).
            probe
                .validate()
                .map_err(|err| ControlError::Validation(format!("probe body invalid: {err}")))?;
        }
        TypedCollection::Devices => {
            let device = typecheck::<multiview_config::Device>(collection, &candidate)?;
            // Per-item semantic checks (driver address requirement, non-empty
            // strings, definite offline severity, sane reconnect bounds, display
            // assignment shape). Document-level rules (id uniqueness, output /
            // wall-head reference resolution, sync-group membership) stay on the
            // document (`MultiviewConfig::validate`, run by `GET /config/export`).
            device
                .validate()
                .map_err(|err| ControlError::Validation(format!("device body invalid: {err}")))?;
        }
        TypedCollection::SyncGroups => {
            let group = typecheck::<multiview_config::SyncGroup>(collection, &candidate)?;
            // Per-item semantic checks (non-empty id, target-skew bounds, at
            // least one member, no duplicate member, member-offset bounds).
            // Document-level rules (group-id uniqueness, member device
            // resolution, one-group-per-device, Cast exclusion) stay on the
            // document (`MultiviewConfig::validate`, run by `GET /config/export`).
            group.validate().map_err(|err| {
                ControlError::Validation(format!("sync-group body invalid: {err}"))
            })?;
        }
    }
    Ok(candidate)
}

/// Deserialize `candidate` as `T`, mapping failure to a path-qualified
/// validation error.
fn typecheck<T: DeserializeOwned>(
    collection: TypedCollection,
    candidate: &Value,
) -> Result<T, ControlError> {
    let deserializer = candidate.clone();
    serde_path_to_error::deserialize::<_, T>(deserializer).map_err(|err| {
        let path = err.path().to_string();
        let message = err.into_inner().to_string();
        ControlError::Validation(if path == "." {
            format!("{} body invalid: {message}", collection.name())
        } else {
            format!("{} body invalid at `{path}`: {message}", collection.name())
        })
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::json;

    use super::{validated_body, TypedCollection};

    #[test]
    fn injects_the_path_id_when_absent() {
        let body = json!({ "kind": "bars" });
        let stored = validated_body(TypedCollection::Sources, "cam1", &body).unwrap();
        assert_eq!(stored["id"], "cam1");
    }

    #[test]
    fn rejects_a_mismatched_id() {
        let body = json!({ "id": "other", "kind": "bars" });
        let err = validated_body(TypedCollection::Sources, "cam1", &body).unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn rejects_an_unknown_source_kind_naming_the_variant() {
        let body = json!({ "id": "cam1", "kind": "flux-capacitor" });
        let err = validated_body(TypedCollection::Sources, "cam1", &body).unwrap_err();
        assert!(err.to_string().contains("flux-capacitor") || err.to_string().contains("kind"));
    }

    #[test]
    fn rejects_a_non_object_body() {
        let err = validated_body(TypedCollection::Overlays, "o1", &json!("nope")).unwrap_err();
        assert!(err.to_string().contains("JSON object"));
    }

    #[test]
    fn accepts_a_valid_output_with_optional_fields_preserved() {
        let body = json!({
            "id": "web1",
            "kind": "ll_hls",
            "path": "/var/lib/multiview/hls",
            "codec": "h264",
            "segment_ms": 2000
        });
        let stored = validated_body(TypedCollection::Outputs, "web1", &body).unwrap();
        assert_eq!(stored["segment_ms"], 2000);
    }
}
