//! Authenticated `MoQ` lease endpoints; only explicit unavailability permits fallback.
use crate::{
    auth::{Action, Principal},
    moq::MoqReject,
    preview::WhepScope,
    problem::Problem,
    state::AppState,
};
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    Json,
};
#[derive(serde::Deserialize)]
pub(crate) struct Generation {
    generation: u64,
}
fn reject(error: MoqReject) -> Response {
    let (status, code) = match error {
        MoqReject::Unavailable => (503, "moq-unavailable"),
        MoqReject::Capacity => (503, "moq-capacity"),
        MoqReject::Forbidden => (403, "forbidden"),
        MoqReject::Codec => (415, "moq-codec"),
        MoqReject::NotFound => (404, "not-found"),
        MoqReject::Generation => (409, "moq-generation"),
    };
    Problem::new(status, code, "MoQ lease request rejected").into_response()
}
fn authorize(principal: &Principal, scope: &WhepScope) -> Result<(), crate::error::ControlError> {
    principal.role.require(Action::Write)?;
    match scope {
        WhepScope::Program => crate::routes::require_unscoped_for_whole_system(principal),
        WhepScope::Input(id) => crate::auth::authorize_object(principal, id),
        WhepScope::Output(id) => crate::auth::authorize_output(principal, id),
    }
}
#[allow(clippy::needless_pass_by_value)] // Owned route-extracted values, off the data plane.
fn open(state: &AppState, principal: &Principal, scope: WhepScope, base: String) -> Response {
    if let Err(err) = authorize(principal, &scope) {
        return err.into_response();
    }
    match state.moq.open(principal, &scope) {
        Ok(lease) => (
            axum::http::StatusCode::CREATED,
            [
                ("location", format!("{base}/{}", lease.lease_id)),
                ("cache-control", "no-store".into()),
            ],
            Json(lease),
        )
            .into_response(),
        Err(err) => reject(err),
    }
}
#[allow(clippy::needless_pass_by_value)] // Owned route-extracted values.
fn close(
    state: &AppState,
    principal: &Principal,
    scope: WhepScope,
    id: String,
    generation: u64,
) -> Response {
    if let Err(err) = authorize(principal, &scope) {
        return err.into_response();
    }
    match state.moq.revoke(principal, &scope, &id, generation) {
        Ok(ack) => ([("cache-control", "no-store")], Json(ack)).into_response(),
        Err(err) => reject(err),
    }
}
#[cfg_attr(feature="openapi", utoipa::path(post,path="/api/v1/preview/program/moq",tag="preview",responses((status=201,description="MoQ lease result",body=crate::moq::MoqLease),(status=403,description="Forbidden",body=crate::problem::Problem),(status=415,description="Unsupported codec",body=crate::problem::Problem),(status=503,description="Unavailable or capacity",body=crate::problem::Problem))))]
pub(crate) async fn program_open(State(state): State<AppState>, principal: Principal) -> Response {
    open(
        &state,
        &principal,
        WhepScope::Program,
        "/api/v1/preview/program/moq".to_owned(),
    )
}
#[cfg_attr(feature="openapi", utoipa::path(delete,path="/api/v1/preview/program/moq/{lease_id}",tag="preview",params(("generation" = u64, Query, description="Exact admitted generation")),responses((status=200,description="MoQ lease result",body=crate::moq::MoqRevoked),(status=403,description="Forbidden",body=crate::problem::Problem),(status=404,description="Unknown lease",body=crate::problem::Problem),(status=409,description="Generation mismatch",body=crate::problem::Problem))))]
pub(crate) async fn program_close(
    State(state): State<AppState>,
    principal: Principal,
    Path(lease_id): Path<String>,
    Query(query): Query<Generation>,
) -> Response {
    close(
        &state,
        &principal,
        WhepScope::Program,
        lease_id,
        query.generation,
    )
}
#[cfg_attr(feature="openapi", utoipa::path(post,path="/api/v1/preview/inputs/{id}/moq",tag="preview",responses((status=201,description="MoQ lease result",body=crate::moq::MoqLease),(status=403,description="Forbidden",body=crate::problem::Problem),(status=415,description="Unsupported codec",body=crate::problem::Problem),(status=503,description="Unavailable or capacity",body=crate::problem::Problem))))]
pub(crate) async fn input_open(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    open(
        &state,
        &principal,
        WhepScope::Input(id.clone()),
        format!("/api/v1/preview/inputs/{id}/moq"),
    )
}
#[cfg_attr(feature="openapi", utoipa::path(delete,path="/api/v1/preview/inputs/{id}/moq/{lease_id}",tag="preview",params(("generation" = u64, Query, description="Exact admitted generation")),responses((status=200,description="MoQ lease result",body=crate::moq::MoqRevoked),(status=403,description="Forbidden",body=crate::problem::Problem),(status=404,description="Unknown lease",body=crate::problem::Problem),(status=409,description="Generation mismatch",body=crate::problem::Problem))))]
pub(crate) async fn input_close(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, lease_id)): Path<(String, String)>,
    Query(query): Query<Generation>,
) -> Response {
    close(
        &state,
        &principal,
        WhepScope::Input(id.clone()),
        lease_id,
        query.generation,
    )
}
#[cfg_attr(feature="openapi", utoipa::path(post,path="/api/v1/preview/outputs/{id}/moq",tag="preview",responses((status=201,description="MoQ lease result",body=crate::moq::MoqLease),(status=403,description="Forbidden",body=crate::problem::Problem),(status=415,description="Unsupported codec",body=crate::problem::Problem),(status=503,description="Unavailable or capacity",body=crate::problem::Problem))))]
pub(crate) async fn output_open(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    open(
        &state,
        &principal,
        WhepScope::Output(id.clone()),
        format!("/api/v1/preview/outputs/{id}/moq"),
    )
}
#[cfg_attr(feature="openapi", utoipa::path(delete,path="/api/v1/preview/outputs/{id}/moq/{lease_id}",tag="preview",params(("generation" = u64, Query, description="Exact admitted generation")),responses((status=200,description="MoQ lease result",body=crate::moq::MoqRevoked),(status=403,description="Forbidden",body=crate::problem::Problem),(status=404,description="Unknown lease",body=crate::problem::Problem),(status=409,description="Generation mismatch",body=crate::problem::Problem))))]
pub(crate) async fn output_close(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, lease_id)): Path<(String, String)>,
    Query(query): Query<Generation>,
) -> Response {
    close(
        &state,
        &principal,
        WhepScope::Output(id.clone()),
        lease_id,
        query.generation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;
    fn principal(role: Role) -> Principal {
        Principal {
            key_id: "owner".into(),
            role,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        }
    }
    #[test]
    fn authorization_rejects_read_roles_and_scoped_program() {
        for role in [Role::ReadOnly, Role::Viewer] {
            let p = principal(role);
            for scope in [
                WhepScope::Program,
                WhepScope::Input("a".into()),
                WhepScope::Output("a".into()),
            ] {
                assert!(authorize(&p, &scope).is_err());
            }
        }
        let mut p = principal(Role::Operator);
        p.scoped_object_ids = Some(vec!["a".into()]);
        assert!(authorize(&p, &WhepScope::Program).is_err());
        assert!(authorize(&p, &WhepScope::Input("a".into())).is_ok());
        assert!(authorize(&p, &WhepScope::Input("b".into())).is_err());
        p.scoped_output_ids = Some(vec!["out".into()]);
        assert!(authorize(&p, &WhepScope::Output("out".into())).is_ok());
        assert!(authorize(&p, &WhepScope::Output("a".into())).is_err());
    }
    #[test]
    fn generation_query_is_required_and_integer() {
        assert!(Query::<Generation>::try_from_uri(&"/lease?generation=7".parse().unwrap()).is_ok());
        for uri in ["/lease", "/lease?generation=oops", "/lease?generation=-1"] {
            assert!(Query::<Generation>::try_from_uri(&uri.parse().unwrap()).is_err());
        }
    }
}
