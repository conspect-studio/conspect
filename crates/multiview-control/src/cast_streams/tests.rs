#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unchecked_time_subtraction
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::*;
use crate::output_routing::RoutedFeed;

fn clocked() -> (Arc<AtomicU64>, CastStreams) {
    let now = Arc::new(AtomicU64::new(1_000_000));
    let c = Arc::clone(&now);
    (
        now,
        CastStreams::with_clock(Arc::new(move || c.load(Ordering::SeqCst))),
    )
}

fn advance(now: &AtomicU64, d: Duration) {
    now.fetch_add(u64::try_from(d.as_millis()).unwrap(), Ordering::SeqCst);
}

#[test]
fn registry_holds_no_plaintext_token() {
    let (_, streams) = clocked();
    let claim = streams
        .claim("op", "output-2", "c1", &RoutedFeed::Program, false)
        .unwrap();
    let dump = format!("{:?}", streams.lock());
    assert!(!dump.contains(&claim.token), "{dump}");
    assert_eq!(streams.lookup(&claim.token).unwrap().1, "c1");
    // The owner's view re-derives the same token.
    assert_eq!(streams.get(&claim.id).unwrap().1, claim.token);
}

#[test]
fn pending_idle_and_absolute_expiry() {
    let (now, streams) = clocked();
    let a = streams
        .claim("op", "o", "c1", &RoutedFeed::Program, false)
        .unwrap();
    advance(&now, PENDING_TTL - Duration::from_millis(1));
    assert!(streams.lookup(&a.token).is_some());
    advance(&now, Duration::from_millis(1));
    assert!(streams.lookup(&a.token).is_none(), "pending expired");

    let b = streams
        .claim("op", "o", "c1", &RoutedFeed::Program, false)
        .unwrap();
    // Live: each playlist fetch pushes the idle expiry out.
    for _ in 0..20 {
        advance(&now, IDLE_TTL - Duration::from_secs(1));
        assert!(streams.lookup(&b.token).is_some());
        streams.record_fetch(&b.id, true, Some("Mozilla CrKey/1.56"));
    }
    advance(&now, IDLE_TTL);
    assert!(streams.lookup(&b.token).is_none(), "idle expired");

    let c = streams
        .claim("op", "o", "c1", &RoutedFeed::Program, false)
        .unwrap();
    let mut elapsed = Duration::ZERO;
    while elapsed < MAX_LIFETIME - Duration::from_secs(30) {
        advance(&now, Duration::from_secs(30));
        elapsed += Duration::from_secs(30);
        streams.record_fetch(&c.id, true, None);
    }
    advance(&now, Duration::from_secs(30));
    assert!(streams.lookup(&c.token).is_none(), "absolute max");
}

#[test]
fn caps_total_and_per_principal() {
    let (_, streams) = clocked();
    for i in 0..MAX_STREAMS_PER_PRINCIPAL {
        streams
            .claim("p", &format!("o{i}"), "c", &RoutedFeed::Program, false)
            .unwrap();
    }
    assert_eq!(
        streams
            .claim("p", "o", "c", &RoutedFeed::Program, false)
            .unwrap_err(),
        ClaimError::PrincipalCapacity
    );
    for i in MAX_STREAMS_PER_PRINCIPAL..MAX_STREAMS {
        streams
            .claim(&format!("q{i}"), "o", "c", &RoutedFeed::Program, false)
            .unwrap();
    }
    assert_eq!(
        streams
            .claim("z", "o", "c", &RoutedFeed::Program, false)
            .unwrap_err(),
        ClaimError::Capacity
    );
}

#[test]
fn first_shared_then_linger_and_end() {
    let (now, streams) = clocked();
    let a = streams
        .claim("p", "o", "c1", &RoutedFeed::Source("s".into()), false)
        .unwrap();
    assert!(a.first);
    let b = streams
        .claim("q", "o", "c1", &RoutedFeed::Source("s".into()), false)
        .unwrap();
    assert!(!b.first);
    streams.mark_moved("c1");
    // Another feed on a used output with expect_free loses.
    assert_eq!(
        streams
            .claim("q", "o", "c1", &RoutedFeed::Program, true)
            .unwrap_err(),
        ClaimError::Taken
    );
    streams.remove(&a.id).unwrap();
    assert_eq!(streams.sweep(), Vec::<Ended>::new());
    streams.remove(&b.id).unwrap();
    assert!(streams.lookup(&b.token).is_none(), "revoked at once");
    assert_eq!(streams.sweep(), Vec::<Ended>::new(), "linger starts");
    advance(&now, LINGER - Duration::from_millis(1));
    assert_eq!(streams.sweep(), Vec::<Ended>::new());
    advance(&now, Duration::from_millis(1));
    let ended = streams.sweep();
    assert_eq!(
        ended,
        vec![Ended {
            output: "o".into(),
            config_id: "c1".into(),
            moved: true
        }]
    );
    assert_eq!(streams.sizes().1, 0);
}

#[test]
fn reports_are_rate_limited_and_stored() {
    let (now, streams) = clocked();
    let a = streams
        .claim("p", "o", "c1", &RoutedFeed::Program, false)
        .unwrap();
    streams
        .report(&a.token, serde_json::json!({"state": "playing"}))
        .unwrap();
    assert_eq!(
        streams.report(&a.token, serde_json::json!({})).unwrap_err(),
        ReportError::TooSoon
    );
    advance(&now, REPORT_INTERVAL);
    streams
        .report(&a.token, serde_json::json!({"state": "buffering"}))
        .unwrap();
    let view = streams.get(&a.id).unwrap().0;
    assert_eq!(view.last_report.unwrap().report["state"], "buffering");
    assert_eq!(
        streams.report("x".repeat(43).as_str(), serde_json::json!({})),
        Err(ReportError::Unknown)
    );
}

#[test]
fn create_limiter_burst_then_one_per_second() {
    let (now, streams) = clocked();
    for _ in 0..CREATE_BURST {
        streams.admit_create("p").unwrap();
    }
    assert!(streams.admit_create("p").is_err());
    assert!(streams.admit_create("other").is_ok());
    advance(&now, CREATE_REFILL);
    streams.admit_create("p").unwrap();
    assert!(streams.admit_create("p").is_err());
}

#[test]
fn memory_stays_bounded_under_a_create_expire_storm() {
    let (now, streams) = clocked();
    for round in 0..200u32 {
        for i in 0..MAX_STREAMS {
            let _ = streams.claim(
                &format!("p{round}-{i}"),
                "o",
                &format!("c{}", i % 4),
                &RoutedFeed::Program,
                false,
            );
            let _ = streams.admit_create(&format!("p{round}-{i}"));
        }
        advance(&now, PENDING_TTL + LINGER);
        let _ = streams.sweep();
        let (entries, slots, limited, rates) = streams.sizes();
        assert!(entries <= MAX_STREAMS && slots <= 4);
        assert!(limited <= MAX_LIMITED_PRINCIPALS && rates <= MAX_STREAMS);
    }
}

/// INV (round 3): gate creation is bounded by [`MAX_GATES`] and fails
/// closed: past it no gate is created, nothing is cleared and nothing can
/// be armed or served on that output.
#[test]
fn gates_are_bounded_and_fail_closed_past_the_cap() {
    let (_, streams) = clocked();
    let claim = streams
        .claim("op", "output-x", "over", &RoutedFeed::Program, false)
        .unwrap();
    for i in 0..MAX_GATES {
        let arm = streams.arm_snapshot(&format!("c{i}"), None);
        assert!(arm.generation > 0);
    }
    let arm = streams.arm_snapshot("over", None);
    assert_eq!(arm.generation, 0);
    assert!(arm.needs.is_none());
    assert_eq!(streams.lock().gates.len(), MAX_GATES);
    assert_eq!(
        streams.arm(&claim.id, arm.generation, None),
        Err(api::ArmError::Stale)
    );
    assert!(streams.serve(&claim.token).is_none());
    assert_eq!(streams.generation("over"), None);
}

/// #418 review ABUSE-HISTORY-R4: [`RETIRE_SEGMENTS`] is the packager's
/// retention plus the segment in progress: `multiview_output::hls::cast`
/// keeps the files of `2 * window` published segments (`retire_old_files`)
/// and the run's window is `multiview-cli` `CAST_WINDOW` = 10, pinned by
/// value (those live in crates this one cannot name).
#[test]
fn retire_segments_match_the_packager_retention() {
    assert_eq!(CAST_WINDOW, 10);
    assert_eq!(RETIRE_SEGMENTS, 21);
    assert_eq!(RETIRE_SEGMENTS, 2 * CAST_WINDOW + 1);
    // The time floor still covers 2 x window segments of the 4 s
    // EXT-X-TARGETDURATION at the nominal cadence.
    const { assert!(MEDIA_RETENTION_MS >= 2 * CAST_WINDOW * 4_000) };
}

fn mark(active: bool, generation: u64, video_segments: u64) -> SinkMark {
    SinkMark {
        active,
        generation,
        video_segments,
        disabled: false,
    }
}

/// #418 review r7: a disabled output (its purge failed: files of the old
/// generation may be left) never proves a superseded grant set replaced,
/// neither by a generation change nor by being inactive.
#[test]
fn gate_history_never_expires_through_a_disabled_output() {
    use super::reauth::Need;
    let need = |s: &str| Need::Object(s.to_owned());
    let after = 1_000 + MEDIA_RETENTION_MS;
    let disabled = |active: bool, generation: u64, segments: u64| SinkMark {
        disabled: true,
        ..mark(active, generation, segments)
    };
    for with_mark in [true, false] {
        let mut gate = registry::Gate::default();
        let at = mark(true, 7, 100);
        gate.retire(Some(vec![need("s2")]), 1_000, with_mark.then_some(&at));
        for sink in [
            disabled(false, 9, 0),
            disabled(true, 8, 0),
            disabled(true, 7, 1_000),
        ] {
            gate.prune(after * 10, Some(&sink));
            assert!(
                gate.cleared_needs().unwrap().contains(&need("s2")),
                "{sink:?}"
            );
        }
        // Enabled again (a later run): replacement proves it as before.
        gate.prune(after, Some(&mark(false, 9, 0)));
        assert!(!gate.remembers());
    }
}

/// ABUSE-HISTORY-R4: a superseded grant set expires only when its retention
/// elapsed AND the sink replaced its media (new generation, inactive, or
/// [`RETIRE_SEGMENTS`] more segments); without a sink status, never.
#[test]
fn gate_history_expires_only_after_retention_and_sink_replacement() {
    use super::reauth::Need;
    let need = |s: &str| Need::Object(s.to_owned());
    let at = mark(true, 7, 100);
    let mut gate = registry::Gate::default();
    gate.replace_settled(("src:s2".to_owned(), Some(vec![need("s2")])), 0, Some(&at));
    gate.replace_settled(
        ("src:s1".to_owned(), Some(vec![need("s1")])),
        1_000,
        Some(&at),
    );
    let after = 1_000 + MEDIA_RETENTION_MS;
    let holds = |gate: &mut registry::Gate, now: u64, sink: Option<&SinkMark>| {
        gate.prune(now, sink);
        gate.cleared_needs().unwrap().contains(&need("s2"))
    };
    // A stalled sink keeps it, however long.
    assert!(holds(&mut gate, after * 1_000, Some(&at)));
    assert!(holds(&mut gate, after, Some(&mark(true, 7, 120))));
    // Status unavailable: kept.
    assert!(holds(&mut gate, after, None));
    // Replaced, but the retention not yet elapsed: kept.
    assert!(holds(
        &mut gate,
        MEDIA_RETENTION_MS - 1,
        Some(&mark(true, 8, 0))
    ));
    // Every file retired (100 + 21 segments): gone.
    let mut g = gate.clone();
    assert!(!holds(&mut g, after, Some(&mark(true, 7, 121))));
    assert_eq!(g.cleared_needs().unwrap(), vec![need("s1")]);
    // A new generation: gone. Inactive: gone.
    let mut g = gate.clone();
    assert!(!holds(&mut g, after, Some(&mark(true, 8, 0))));
    let mut g = gate.clone();
    assert!(!holds(&mut g, after, Some(&mark(false, 0, 0))));
    // Superseded while the status was unavailable: only an inactive output
    // proves it replaced.
    let mut gate = registry::Gate::default();
    gate.retire(Some(vec![need("s2")]), 0, None);
    assert!(holds(&mut gate, after, Some(&mark(true, 9, 1_000))));
    assert!(!holds(&mut gate, after, Some(&mark(false, 0, 0))));
}

/// ABUSE-HISTORY-R4: the history is bounded; an overflow merges into the
/// oldest entry with the LATER retention and sink mark, so no need is
/// dropped early. Undeterminable history fails closed.
#[test]
fn gate_history_merges_bounded_and_never_drops_a_need_early() {
    use super::reauth::Need;
    let need = |s: &str| Need::Object(s.to_owned());
    let mut gate = registry::Gate::default();
    let max = u64::try_from(MAX_HISTORY).unwrap();
    for i in 0..max + 3 {
        gate.retire(
            Some(vec![need(&format!("n{i}"))]),
            i,
            Some(&mark(true, 7, i * 10)),
        );
    }
    assert_eq!(gate.history.len(), MAX_HISTORY);
    for i in 0..max + 3 {
        let mut g = gate.clone();
        g.prune(
            i + MEDIA_RETENTION_MS - 1,
            Some(&mark(true, 7, i * 10 + RETIRE_SEGMENTS)),
        );
        assert!(
            g.cleared_needs().unwrap().contains(&need(&format!("n{i}"))),
            "n{i}"
        );
        let mut g = gate.clone();
        g.prune(
            i + MEDIA_RETENTION_MS,
            Some(&mark(true, 7, i * 10 + RETIRE_SEGMENTS - 1)),
        );
        assert!(
            g.cleared_needs().unwrap().contains(&need(&format!("n{i}"))),
            "n{i}"
        );
    }
    gate.retire(None, 0, None);
    assert!(gate.cleared_needs().is_none());
}

/// INV418-FINAL-1: the distinct grants retained across the whole history
/// are bounded, overflow merges included. Past [`MAX_HISTORY_NEEDS`] the
/// history collapses to one undeterminable entry (fails closed) that keeps
/// the latest retention and sink mark, and still expires normally.
#[test]
fn gate_history_grant_storage_is_bounded_and_overflow_fails_closed() {
    use super::reauth::Need;
    let need = |s: String| Need::Object(s);
    let stalled = mark(true, 7, 100);
    let mut gate = registry::Gate::default();
    let stored = |gate: &registry::Gate| -> usize {
        gate.history
            .iter()
            .map(|h| h.needs.as_ref().map_or(0, Vec::len))
            .sum()
    };
    // Below the bound: every grant is still required.
    let per = 3;
    let fits = MAX_HISTORY_NEEDS / per;
    for i in 0..fits {
        let needs = (0..per).map(|j| need(format!("n{i}-{j}"))).collect();
        gate.retire(Some(needs), 0, Some(&stalled));
    }
    assert!(stored(&gate) <= MAX_HISTORY_NEEDS);
    assert!(gate
        .cleared_needs()
        .unwrap()
        .contains(&need("n0-0".to_owned())));
    // Many more distinct grants with the sink stalled: bounded, fail closed.
    let end = fits + 2_000;
    // The latest retirement time (`retire` at `i` ms).
    let last = u64::try_from(end - 1).unwrap();
    for i in fits..end {
        let needs = (0..per).map(|j| need(format!("n{i}-{j}"))).collect();
        gate.retire(Some(needs), u64::try_from(i).unwrap(), Some(&stalled));
        assert!(stored(&gate) <= MAX_HISTORY_NEEDS, "{i}");
        assert!(gate.history.len() <= MAX_HISTORY);
    }
    assert!(gate.cleared_needs().is_none());
    // A stalled sink keeps it, however long.
    gate.prune(last * 1_000, Some(&stalled));
    assert!(gate.remembers() && gate.cleared_needs().is_none());
    // The retention not yet elapsed: kept even when the sink replaced it.
    gate.prune(last + MEDIA_RETENTION_MS - 1, Some(&mark(true, 8, 0)));
    assert!(gate.cleared_needs().is_none());
    // Retention elapsed and every file retired: it clears.
    gate.prune(
        last + MEDIA_RETENTION_MS,
        Some(&mark(true, 7, 100 + RETIRE_SEGMENTS)),
    );
    assert!(!gate.remembers());
    assert_eq!(gate.cleared_needs(), Some(Vec::new()));
}

// ---- ADR-0114 decision 5.2 / 5.4: push attachments in the registry ------

mod attachments {
    use super::*;
    use crate::cast_streams::identity::{CastAttachment, Clearance, Component, OutputLabels};
    use crate::cast_streams::reauth::Need;

    fn armed(streams: &CastStreams, owner: &str) -> (Claim, u64) {
        let claim = streams
            .claim(owner, "o", "c1", &RoutedFeed::Program, false)
            .unwrap();
        let arm = streams.arm_snapshot("c1", None);
        streams
            .arm(&claim.id, arm.generation, Some(("program".into(), Some(vec![Need::Bus]))))
            .unwrap();
        (claim, arm.generation)
    }

    fn attachment(id: &str, rung: &'static str) -> Arc<CastAttachment> {
        Arc::new(CastAttachment::new(
            id,
            "c1",
            rung,
            Arc::new(OutputLabels::new()),
            Clearance::of(&[(Component::Video, 1)]),
        ))
    }

    #[test]
    fn an_attachment_is_registered_only_at_the_current_generation() {
        let (_, streams) = clocked();
        let (claim, generation) = armed(&streams, "op");
        let stale = attachment(&claim.id, "moq");
        assert!(!streams.attach(Arc::clone(&stale), generation + 1));
        let ok = attachment(&claim.id, "moq");
        assert!(streams.attach(Arc::clone(&ok), generation));
        assert_eq!(streams.attachments(&claim.id).len(), 1);
        // Another output's attachment never registers on this stream.
        let foreign = Arc::new(CastAttachment::new(
            &claim.id,
            "c2",
            "moq",
            Arc::new(OutputLabels::new()),
            Clearance::default(),
        ));
        assert!(!streams.attach(foreign, generation));
    }

    #[test]
    fn revoke_remove_and_expiry_fence_every_attachment() {
        let (now, streams) = clocked();
        let (a, ga) = armed(&streams, "op");
        let (b, gb) = armed(&streams, "op2");
        let (c, gc) = armed(&streams, "op3");
        let (xa, xb, xc) = (attachment(&a.id, "moq"), attachment(&b.id, "moq"), attachment(&c.id, "moq"));
        assert!(streams.attach(Arc::clone(&xa), streams.generation("c1").unwrap()));
        let _ = (ga, gb, gc);
        assert!(streams.attach(Arc::clone(&xb), streams.generation("c1").unwrap()));
        assert!(streams.attach(Arc::clone(&xc), streams.generation("c1").unwrap()));
        streams.revoke(&a.id, "test").unwrap();
        assert!(xa.is_fenced());
        streams.remove(&b.id).unwrap();
        assert!(xb.is_fenced());
        // A push attachment keeps the token live only through ladder calls
        // or reports; with none it expires and is fenced by the sweep.
        assert!(!xc.is_fenced());
        advance(&now, IDLE_TTL + Duration::from_secs(1));
        streams.sweep();
        assert!(xc.is_fenced());
    }

    #[test]
    fn a_requested_change_clears_only_the_tokens_that_pass_it() {
        let (_, streams) = clocked();
        let (keep, _) = armed(&streams, "op");
        let (lose, _) = armed(&streams, "op2");
        let generation = streams.generation("c1").unwrap();
        let xk = attachment(&keep.id, "moq");
        let xl = attachment(&lose.id, "moq");
        assert!(streams.attach(Arc::clone(&xk), generation));
        assert!(streams.attach(Arc::clone(&xl), generation));
        let lose_id = lose.id.clone();
        let check = move |s: &OwnedStream, _: Option<&[Need]>| {
            if s.id == lose_id {
                Err("revoked: no")
            } else {
                Ok(())
            }
        };
        let revoked = streams.commit(
            "c1",
            Change {
                op: Some("op-1".into()),
                bus: Some("src:cam2".into()),
                needs: Some(vec![Need::Object("cam2".into())]),
                ids: vec![(Component::Video, 42)],
            },
            None,
            &check,
        );
        assert_eq!(revoked.len(), 1);
        assert!(xk.clearance().clears(&Component::Video, 42));
        assert!(xk.clearance().clears(&Component::Video, 1), "the current one stays");
        assert!(xl.is_fenced(), "revoked before the request goes to the engine");
        assert!(!xl.clearance().clears(&Component::Video, 42));
    }

    #[test]
    fn at_most_two_per_rung_make_before_break() {
        let (_, streams) = clocked();
        let (claim, generation) = armed(&streams, "op");
        let first = attachment(&claim.id, "moq");
        let second = attachment(&claim.id, "moq");
        let third = attachment(&claim.id, "moq");
        assert!(streams.attach(Arc::clone(&first), generation));
        assert!(streams.attach(Arc::clone(&second), generation));
        assert!(!first.is_fenced() && !second.is_fenced());
        assert!(streams.attach(Arc::clone(&third), generation));
        assert!(first.is_fenced(), "the oldest of the rung goes");
        assert!(!second.is_fenced() && !third.is_fenced());
        assert_eq!(streams.attachments(&claim.id).len(), MAX_PER_RUNG);
    }

    #[test]
    fn a_live_attachment_keeps_its_token_live_through_ladder_calls() {
        let (now, streams) = clocked();
        let (claim, generation) = armed(&streams, "op");
        assert!(streams.attach(attachment(&claim.id, "moq"), generation));
        for _ in 0..5 {
            advance(&now, IDLE_TTL - Duration::from_secs(1));
            let request = crate::cast_streams::ladder::parse_request(br#"{"v":1}"#, streams.now_ms()).unwrap();
            streams.record_ladder(&claim.id, request, Some("moq")).unwrap();
        }
        assert!(streams.lookup(&claim.token).is_some());
    }
}
