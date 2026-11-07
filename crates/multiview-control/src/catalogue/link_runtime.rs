//! Link-tracker runtime (slice D): after every catalogue sync, re-evaluate the
//! linked sources, publish `catalogue.link_status` for each transition, raise or
//! clear the aggregate `catalogue-link-attention` health warning, and apply
//! `follow = "auto"` updates through the **normal If-Match write path**, audited as
//! the system principal. Control plane only; nothing here awaits the engine.

use std::sync::Arc;

use multiview_events::{CatalogueLinkStatus, Event, HealthWarning, WarningCode, WarningSeverity};

use super::links::{LinkStatus, Transition};
use super::Catalogue;
use crate::state::AppState;

/// The audit actor for unattended, `follow = auto` changes.
pub const SYSTEM_ACTOR: &str = "system:catalogue";

fn event_of(t: &Transition) -> Event {
    Event::CatalogueLinkStatus(CatalogueLinkStatus {
        source_id: t.source_id.clone(),
        status: t.status.as_str().to_owned(),
        previous: t.previous.map(|p| p.as_str().to_owned()),
        channel: t.channel.clone(),
        feed: t.feed.clone(),
        replaced_by: t.replaced_by.clone(),
    })
}

fn warning(count: usize, since: i64, active: bool) -> HealthWarning {
    HealthWarning {
        code: WarningCode::CatalogueLinkAttention,
        severity: WarningSeverity::Warning,
        subsystem: "catalogue".to_owned(),
        message: format!(
            "{count} source(s) linked to the IPTV-ORG catalogue need attention (stream removed, \
             channel closed, blocklisted, updated upstream, or edited)."
        ),
        remediation: "Open Sources → Link: review the change, switch to an alternative, or unlink."
            .to_owned(),
        since,
        active,
    }
}

/// All accepted sources (system view: not caller-scoped; used only server-side).
fn all_sources(state: &AppState) -> Vec<multiview_config::Source> {
    super::accepted_source_docs(state)
}

/// Re-evaluate every linked source against the current snapshot.
pub async fn recompute_and_publish(state: &AppState, cat: &Arc<Catalogue>) {
    let Some(snapshot) = cat.snapshot() else {
        return;
    };
    let sources = all_sources(state);
    let transitions = cat.links.recompute(&sources, &snapshot);
    for t in &transitions {
        state.engine.publish_event(event_of(t));
    }
    let attention = cat.links.attention_count();
    let since = state.ack_now().as_nanos();
    if attention > 0 {
        state
            .engine
            .publish_event(Event::HealthWarningRaised(warning(attention, since, true)));
    } else if !transitions.is_empty() || cat.links.take_warning_latch() {
        state
            .engine
            .publish_event(Event::HealthWarningCleared(warning(0, since, false)));
    }
    if attention > 0 {
        cat.links.set_warning_latch();
    }
    apply_auto(state, cat, &snapshot).await;
}

/// Apply `follow = auto` updates (only `updated`, never adding `geo_blocked` or
/// `unsupported`) through the normal source write path.
async fn apply_auto(state: &AppState, cat: &Arc<Catalogue>, _snapshot: &super::index::Snapshot) {
    let infos = cat.links.all();
    for decision in super::plan_auto(state, &infos) {
        match super::apply_auto_decision(state, cat, &decision, SYSTEM_ACTOR).await {
            Ok(true) => {
                tracing::info!(source = %decision.source_id, "catalogue auto-follow applied an upstream update");
            }
            Ok(false) => {}
            Err(_) => {
                tracing::warn!(source = %decision.source_id, "catalogue auto-follow could not apply the change");
            }
        }
    }
    // Re-evaluate so applied updates settle to `in_sync` immediately.
    if let Some(snapshot) = cat.snapshot() {
        let sources = all_sources(state);
        let transitions = cat.links.recompute(&sources, &snapshot);
        for t in &transitions {
            state.engine.publish_event(event_of(t));
        }
        if cat.links.attention_count() == 0 && cat.links.take_warning_latch() {
            state
                .engine
                .publish_event(Event::HealthWarningCleared(warning(
                    0,
                    state.ack_now().as_nanos(),
                    false,
                )));
        }
    }
}

/// Whether a status needs attention (re-exported for callers).
#[must_use]
pub fn needs_attention(s: LinkStatus) -> bool {
    s.needs_attention()
}
