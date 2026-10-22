//! Live authorization revocation on established WS/SSE sessions (ADR-RT010).
//!
//! WS/SSE capture the principal's role + object scope at CONNECT only, so a
//! mid-session scope narrow/widen, role downgrade, or key revoke used to keep
//! delivering now-unauthorized deltas (and keep the client displaying now-hidden
//! cached objects) until the client happened to reconnect. This closes the gap:
//! `ApiKeyStore` is the interior-mutable source of truth with a wait-free
//! generation counter, and each session (store-managed API-key principals only)
//! re-resolves on a generation change — narrowing/widening yields a `$resync`
//! rebuild directive, loss of read access yields a forced disconnect.
//!
//! These tests drive `SessionStream` + `ApiKeyStore` directly — the
//! transport-agnostic core both `run_ws_session` and `sse_handler` share —
//! exactly like `tests/realtime_watermark.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_control::{
    ApiKeyStore, Principal, RealtimeFrame, ReauthOutcome, Role, SessionStream,
};
use multiview_engine::EnginePublisher;
use multiview_events::{DeviceState, DeviceStatus, Event, FrameKind, ResyncReason, Topic};

type Publisher = EnginePublisher<serde_json::Value, Event>;

/// A `device.status` event scoped (by `object_authz_scope_id`) to `device_id`.
fn device_status(device_id: &str) -> Event {
    Event::DeviceStatus(DeviceStatus::new(device_id, DeviceState::Online))
}

/// A principal for `key_id` with an explicit role and object scope.
fn principal(key_id: &str, role: Role, scope: Option<Vec<String>>) -> Principal {
    Principal {
        key_id: key_id.to_owned(),
        role,
        scoped_object_ids: scope,
        scoped_output_ids: None,
        scoped_discovery_domains: None,
    }
}

/// A store carrying one registered key (built `mut` pre-`Arc`, then shared so the
/// runtime `&self` mutators — `revoke`/`set_principal` — model a live admin edit).
fn store_with(key_id: &str, secret: &str, p: Principal) -> Arc<ApiKeyStore> {
    let mut store = ApiKeyStore::new(b"pepper".to_vec());
    store.register(key_id, secret, p);
    Arc::new(store)
}

/// Drain up to `polls` deltas, collecting the ones actually delivered (an
/// `Ok(None)` is a suppressed/skipped event). Each event is pre-buffered, so
/// `next_delta` never awaits — the bounded poll count terminates deterministically.
async fn drain(session: &mut SessionStream, polls: usize) -> Vec<RealtimeFrame> {
    let mut delivered = Vec::new();
    for _ in 0..polls {
        if let Some(frame) = session.next_delta().await.unwrap() {
            delivered.push(frame);
        }
    }
    delivered
}

/// The core gap: a session scoped to object `A`, then `A` is revoked from the
/// principal mid-session. After re-resolution the session NO LONGER delivers an
/// `A` delta (an in-scope `B` still flows) — where before the fix the captured
/// connect-time scope kept delivering `A` forever.
#[tokio::test]
async fn scope_revocation_stops_delivering_the_revoked_object() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let store = store_with(
        "k1",
        "secret",
        principal(
            "k1",
            Role::Operator,
            Some(vec!["A".to_owned(), "B".to_owned()]),
        ),
    );

    let mut session = SessionStream::new(engine.subscribe(), "sess-revoke-a", None)
        .with_object_scope(Some(vec!["A".to_owned(), "B".to_owned()]))
        .with_live_reauth(Arc::clone(&store), "k1", Role::Operator);

    // Baseline: an `A` event IS delivered.
    let _ = engine.publish_event(device_status("A"));
    let delivered = drain(&mut session, 1).await;
    assert_eq!(delivered.len(), 1);
    assert!(
        matches!(&delivered[0].envelope.payload, Event::DeviceStatus(s) if s.device_id == "A"),
        "A is delivered before revocation"
    );

    // Mid-session: an admin narrows the principal's scope to `[B]` — `A` revoked.
    assert!(
        store.set_principal(
            "k1",
            principal("k1", Role::Operator, Some(vec!["B".to_owned()]))
        ),
        "set_principal updates an existing key"
    );

    // The session samples the bumped generation and adopts the new scope.
    assert_eq!(session.reauthorize(), ReauthOutcome::ScopeChanged);

    // Now an `A` event is NO LONGER delivered; an in-scope `B` still is.
    let _ = engine.publish_event(device_status("A"));
    let _ = engine.publish_event(device_status("B"));
    let delivered = drain(&mut session, 2).await;
    assert_eq!(
        delivered.len(),
        1,
        "the revoked object A must be filtered mid-session; B still flows"
    );
    assert!(
        matches!(&delivered[0].envelope.payload, Event::DeviceStatus(s) if s.device_id == "B"),
        "only the still-in-scope B is delivered after revocation"
    );
}

/// ADR-W026 over RT010: the OUTPUT axis is carried into the live session and
/// re-scoped mid-session. An output-only widen (object + discovery unchanged) is
/// a `ScopeChanged` — the pre-W026 object-only `reauthorize` missed it, so an
/// output re-scope was DEAD on the live stream.
#[tokio::test]
async fn reauthorize_adopts_an_output_only_scope_change() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let base = Principal {
        key_id: "k1".to_owned(),
        role: Role::Operator,
        scoped_object_ids: None,
        scoped_output_ids: Some(vec!["out-a".to_owned()]),
        scoped_discovery_domains: None,
    };
    let store = store_with("k1", "secret", base.clone());
    let mut session = SessionStream::new(engine.subscribe(), "sess-out-axis", None)
        .with_scopes(None, Some(vec!["out-a".to_owned()]), None)
        .with_live_reauth(Arc::clone(&store), "k1", Role::Operator);

    // Grant `program:main` on the output axis only.
    assert!(store.set_principal(
        "k1",
        Principal {
            scoped_output_ids: Some(vec!["out-a".to_owned(), "program:main".to_owned()]),
            ..base.clone()
        }
    ));
    assert_eq!(session.reauthorize(), ReauthOutcome::ScopeChanged);
}

/// ADR-W026 over RT010: the DISCOVERY axis is carried into the live session and
/// re-scoped mid-session. A discovery-only widen is a `ScopeChanged`.
#[tokio::test]
async fn reauthorize_adopts_a_discovery_only_scope_change() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let base = Principal {
        key_id: "k1".to_owned(),
        role: Role::Operator,
        scoped_object_ids: None,
        scoped_output_ids: None,
        scoped_discovery_domains: Some(vec!["site-a".to_owned()]),
    };
    let store = store_with("k1", "secret", base.clone());
    let mut session = SessionStream::new(engine.subscribe(), "sess-disc-axis", None)
        .with_scopes(None, None, Some(vec!["site-a".to_owned()]))
        .with_live_reauth(Arc::clone(&store), "k1", Role::Operator);

    assert!(store.set_principal(
        "k1",
        Principal {
            scoped_discovery_domains: Some(vec!["site-a".to_owned(), "site-b".to_owned()]),
            ..base.clone()
        }
    ));
    assert_eq!(session.reauthorize(), ReauthOutcome::ScopeChanged);
}

/// A full deauthorization (the API key revoked — "role downgraded to none"): the
/// principal no longer resolves to any reading role, so the established session is
/// torn down (`ReauthOutcome::Disconnect`) rather than streaming to a revoked
/// identity.
#[tokio::test]
async fn revoked_key_disconnects_the_established_session() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let store = store_with(
        "k1",
        "secret",
        principal("k1", Role::Viewer, Some(vec!["A".to_owned()])),
    );

    let mut session = SessionStream::new(engine.subscribe(), "sess-disc", None)
        .with_object_scope(Some(vec!["A".to_owned()]))
        .with_live_reauth(Arc::clone(&store), "k1", Role::Viewer);

    // No change yet: the session is stable.
    assert_eq!(session.reauthorize(), ReauthOutcome::Unchanged);

    // The key is revoked mid-session.
    assert!(store.revoke("k1"), "revoke removes the key");

    // The session re-resolves to "no principal" and must disconnect.
    assert_eq!(session.reauthorize(), ReauthOutcome::Disconnect);
}

/// A role downgrade that STAYS within the reading roles (here `Admin` → `Viewer`,
/// both permit `Action::Read`) with unchanged scope is adopted without a spurious
/// disconnect — the realtime read stream is role-gated on `Read`, which both
/// still satisfy, so the session continues. Proves role re-resolution takes effect
/// mid-session without over-disconnecting a still-authorized reader.
#[tokio::test]
async fn reading_role_downgrade_is_adopted_without_disconnect() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let store = store_with("k1", "secret", principal("k1", Role::Admin, None));

    let mut session = SessionStream::new(engine.subscribe(), "sess-role", None)
        .with_object_scope(None)
        .with_live_reauth(Arc::clone(&store), "k1", Role::Admin);

    // Downgrade Admin -> Viewer (still can Read; scope unchanged: unscoped).
    assert!(store.set_principal("k1", principal("k1", Role::Viewer, None)));
    assert_eq!(
        session.reauthorize(),
        ReauthOutcome::Unchanged,
        "a still-reading downgrade with unchanged scope keeps the session alive"
    );
    // ADR-RT010: the downgraded role is OBSERVABLY adopted (not merely re-resolved
    // then discarded) — the session's live role reflects Viewer after the downgrade.
    // Guards against the adoption `live.role = principal.role` being silently dropped.
    assert_eq!(
        session.live_role(),
        Some(Role::Viewer),
        "the re-resolved (downgraded) role is adopted into the session's live-authz handle"
    );

    // The stream still flows for the (now Viewer) reader.
    let _ = engine.publish_event(device_status("anything"));
    assert_eq!(drain(&mut session, 1).await.len(), 1);
}

/// Scope NARROWING must tell the client to drop the now-hidden cached object: the
/// session emits a server-initiated `$resync` (a full REBUILD directive, not a
/// merge) naming the object-bearing topics, so the client rebuilds and a
/// narrowed-away object is absent — closing the DISPLAY hole a silent filter swap
/// would leave open.
#[tokio::test]
async fn scope_narrowing_emits_a_resync_rebuild_directive() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let store = store_with(
        "k1",
        "secret",
        principal("k1", Role::Operator, Some(vec!["A".to_owned()])),
    );

    let mut session = SessionStream::new(engine.subscribe(), "sess-resync", None)
        .with_object_scope(Some(vec!["A".to_owned()]))
        .with_live_reauth(Arc::clone(&store), "k1", Role::Operator);

    // Narrow to the empty (closed) scope: A is now hidden.
    assert!(store.set_principal("k1", principal("k1", Role::Operator, Some(vec![]))));
    assert_eq!(session.reauthorize(), ReauthOutcome::ScopeChanged);

    // The server emits the `$resync` full-rebuild directive.
    let resync = session.resync_frame(0);
    assert_eq!(
        resync.kind,
        FrameKind::Snapshot,
        "$resync resets the client baseline (a rebuild boundary)"
    );
    assert_eq!(resync.envelope.topic, Topic::Control);
    match &resync.envelope.payload {
        Event::Resync(r) => {
            assert_eq!(
                r.reason,
                ResyncReason::AuthzChanged,
                "the resync reason distinguishes an authz change from a replay eviction"
            );
            // The rebuild set is exactly the re-snapshotted topics (tiles +
            // devices), so the client drops now-hidden cached objects on them.
            // Switcher is NOT listed: it is never re-snapshotted, so advertising it
            // would strand the client's switcher state on a rebuild-not-merge (the
            // protocol gap the #231 panel flagged).
            assert!(r.resubscribe.contains(&Topic::Tiles));
            assert!(r.resubscribe.contains(&Topic::Devices));
            assert!(
                !r.resubscribe.contains(&Topic::Switcher),
                "Switcher is never re-snapshotted; it must not be in the rebuild set"
            );
        }
        other => panic!("expected an $resync directive, got {other:?}"),
    }
}

/// Scope WIDENING makes newly-in-scope objects visible mid-session: after the
/// admin adds `B` to the allowlist and the session re-resolves, a `B` delta that
/// was previously filtered is now delivered.
#[tokio::test]
async fn scope_widening_makes_new_objects_visible() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let store = store_with(
        "k1",
        "secret",
        principal("k1", Role::Operator, Some(vec!["A".to_owned()])),
    );

    let mut session = SessionStream::new(engine.subscribe(), "sess-widen", None)
        .with_object_scope(Some(vec!["A".to_owned()]))
        .with_live_reauth(Arc::clone(&store), "k1", Role::Operator);

    // Before: B is out of scope.
    let _ = engine.publish_event(device_status("B"));
    assert_eq!(
        drain(&mut session, 1).await.len(),
        0,
        "B is out of scope before widening"
    );

    // Widen to [A, B].
    assert!(store.set_principal(
        "k1",
        principal(
            "k1",
            Role::Operator,
            Some(vec!["A".to_owned(), "B".to_owned()]),
        ),
    ));
    assert_eq!(session.reauthorize(), ReauthOutcome::ScopeChanged);

    // After: B is delivered.
    let _ = engine.publish_event(device_status("B"));
    let delivered = drain(&mut session, 1).await;
    assert_eq!(delivered.len(), 1);
    assert!(
        matches!(&delivered[0].envelope.payload, Event::DeviceStatus(s) if s.device_id == "B"),
        "B becomes visible after the scope widens"
    );
}

/// A session with NO live re-auth wired (the local-admin / JWT principals that are
/// not store-managed, and every existing transport-only test) is untouched:
/// `reauthorize` is always `Unchanged` and the stream is unaffected. Guards that
/// the fix is opt-in and does not regress non-store principals.
#[tokio::test]
async fn session_without_live_reauth_never_disconnects_or_resyncs() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let mut session = SessionStream::new(engine.subscribe(), "sess-none", None)
        .with_object_scope(Some(vec!["A".to_owned()]));

    assert_eq!(session.reauthorize(), ReauthOutcome::Unchanged);
    let _ = engine.publish_event(device_status("A"));
    assert_eq!(drain(&mut session, 1).await.len(), 1);
    // Still unchanged after traffic — no live_authz means no re-resolution.
    assert_eq!(session.reauthorize(), ReauthOutcome::Unchanged);
}

/// The revocation hook itself: `set_principal` and `revoke` bump the wait-free
/// generation and re-resolve the current principal; `set_principal` keeps the
/// secret digest (the same token authenticates with the new authz) while `revoke`
/// removes the key entirely (the token no longer authenticates). A missing-key
/// mutation is a no-op that does not bump the generation.
#[test]
fn store_mutators_bump_the_generation_and_reresolve() {
    let mut store = ApiKeyStore::new(b"pepper".to_vec());
    store.register(
        "k1",
        "secret",
        principal("k1", Role::Operator, Some(vec!["A".to_owned()])),
    );

    let g0 = store.generation();
    assert!(store.verify("k1.secret").is_ok(), "the token authenticates");
    assert_eq!(
        store.principal_for_key("k1").map(|p| p.role),
        Some(Role::Operator)
    );

    // set_principal replaces the authz (role + scope), keeps the digest, bumps gen.
    assert!(store.set_principal("k1", principal("k1", Role::Viewer, Some(vec![]))));
    let g1 = store.generation();
    assert!(g1 > g0, "set_principal bumps the auth generation");
    assert_eq!(
        store.principal_for_key("k1").map(|p| p.role),
        Some(Role::Viewer)
    );
    assert_eq!(
        store
            .principal_for_key("k1")
            .and_then(|p| p.scoped_object_ids),
        Some(vec![])
    );
    assert!(
        store.verify("k1.secret").is_ok(),
        "the same secret still authenticates after set_principal (digest kept)"
    );

    // revoke removes the key, bumps gen, and the token stops authenticating.
    assert!(store.revoke("k1"));
    let g2 = store.generation();
    assert!(g2 > g1, "revoke bumps the auth generation");
    assert_eq!(store.principal_for_key("k1"), None);
    assert!(
        store.verify("k1.secret").is_err(),
        "a revoked key no longer authenticates"
    );

    // A mutation of a missing key is a no-op — no generation bump.
    assert!(!store.revoke("nope"));
    assert!(!store.set_principal("nope", principal("nope", Role::Admin, None)));
    assert_eq!(
        store.generation(),
        g2,
        "a no-op mutation must not bump the generation"
    );
}

/// CONNECT-RACE (ADR-RT010, panel critical finding): a key revoked in the window
/// BETWEEN connect-authentication (when the baseline generation is captured) and
/// the live-authz handle being installed must still be caught. Modelling the
/// transport wiring: `resolve_principal` captures `baseline` at auth, then a revoke
/// lands, then `with_live_reauth_at` installs the handle with that baseline. The
/// first `reauthorize` MUST observe the window revoke and disconnect — never
/// silently retain the pre-revocation authorization forever.
#[tokio::test]
async fn revoke_racing_connect_is_caught_not_retained() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let store = store_with("k1", "secret", principal("k1", Role::Operator, None));

    // Connect-auth captures the baseline generation here…
    let baseline = store.generation();
    // …then the key is revoked in the connect window, before the handle installs.
    assert!(store.revoke("k1"), "revoke removes the key");

    let mut session = SessionStream::new(engine.subscribe(), "sess-race-revoke", None)
        .with_live_reauth_at(Arc::clone(&store), "k1", Role::Operator, baseline);

    assert_eq!(
        session.reauthorize(),
        ReauthOutcome::Disconnect,
        "a key revoked between auth and install must disconnect, not retain access"
    );
}

/// CONNECT-RACE, re-scope variant (ADR-RT010): a scope NARROWED in the window
/// between connect-auth and install must be adopted on the first re-resolution, not
/// stranded at the connect-time (wider) scope. The baseline captured at auth makes
/// the window mutation observable as a generation advance.
#[tokio::test]
async fn rescope_racing_connect_is_adopted_not_stale() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let store = store_with(
        "k1",
        "secret",
        principal(
            "k1",
            Role::Operator,
            Some(vec!["A".to_owned(), "B".to_owned()]),
        ),
    );

    let baseline = store.generation();
    // Narrow to [A] in the connect window, after the baseline is captured.
    assert!(store.set_principal(
        "k1",
        principal("k1", Role::Operator, Some(vec!["A".to_owned()]))
    ));

    let mut session = SessionStream::new(engine.subscribe(), "sess-race-scope", None)
        // The connect-time scope the client authenticated with (wider).
        .with_object_scope(Some(vec!["A".to_owned(), "B".to_owned()]))
        .with_live_reauth_at(Arc::clone(&store), "k1", Role::Operator, baseline);

    assert_eq!(
        session.reauthorize(),
        ReauthOutcome::ScopeChanged,
        "a scope narrowed in the connect window must be adopted on first re-resolution"
    );
    // And the adopted scope is the narrowed [A]: an out-of-scope B is now filtered.
    let _ = engine.publish_event(device_status("B"));
    assert!(
        drain(&mut session, 1).await.is_empty(),
        "out-of-scope B is filtered once the window re-scope is adopted"
    );
}

/// The `$resync` rebuild directive must name EXACTLY the topics
/// `build_resync_frames` actually re-snapshots (tiles + devices). Switcher is
/// neither object-authz-scoped nor re-snapshotted at connect or on resync, so
/// advertising it would tell a rebuild-not-merge client to CLEAR switcher state it
/// then never receives back — stranding it. Regression for the panel's protocol gap.
#[tokio::test]
async fn resync_rebuild_topics_match_the_re_snapshotted_set() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let mut session = SessionStream::new(engine.subscribe(), "sess-resync", None);

    let frame = session.resync_frame(0);
    let Event::Resync(resync) = &frame.envelope.payload else {
        panic!("resync_frame builds a Resync event");
    };
    assert_eq!(resync.reason, ResyncReason::AuthzChanged);
    assert_eq!(
        resync.resubscribe,
        vec![Topic::Tiles, Topic::Devices],
        "resubscribe names exactly the re-snapshotted topics"
    );
    assert!(
        !resync.resubscribe.contains(&Topic::Switcher),
        "Switcher is never re-snapshotted; advertising it strands the client's switcher state"
    );
}
