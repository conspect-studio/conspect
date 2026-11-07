//! The clip-job lifecycle and the bounded, coalescing queue (ADR-0100
//! decision 4).
//!
//! Two operator-visible guarantees live here: an illegal state move is
//! *refused* rather than applied, and pressing "run" twice for one fill does not
//! start two browser captures. Everything is pure logic with an injected job id,
//! so no runner, timer or browser is involved.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use multiview_webengine::{FillId, Job, JobId, JobPhase, JobQueue, JobState, Trigger};

fn id(name: &str) -> JobId {
    JobId::new(name)
}

fn fill(name: &str) -> FillId {
    FillId::new(name)
}

#[test]
fn a_job_walks_queued_running_succeeded() {
    let mut job = Job::queued(id("job-1"), fill("radar"));
    assert_eq!(job.state(), JobState::Queued);
    assert_eq!(job.state().as_str(), "queued");
    assert!(!job.state().is_terminal());
    assert!(job.state().is_active(), "a queued job counts as in flight");
    assert!(job.state().can_transition_to(JobState::Running));

    job.set_progress(JobPhase::new("browser_start"), Some(10))
        .expect("10%");
    assert_eq!(
        job.transition(JobState::Running).expect("start"),
        JobState::Running
    );
    job.set_progress(JobPhase::new("capture"), Some(60))
        .expect("60%");
    assert_eq!(job.percent, Some(60));
    assert_eq!(
        job.transition(JobState::Succeeded).expect("finish"),
        JobState::Succeeded
    );
    assert!(job.state().is_terminal());
    assert_eq!(job.percent, Some(100));
    assert_eq!(job.phase.as_str(), JobPhase::DONE);
}

#[test]
fn every_illegal_move_is_refused_and_changes_nothing() {
    let legal: [(JobState, JobState); 5] = [
        (JobState::Queued, JobState::Running),
        (JobState::Queued, JobState::Cancelled),
        (JobState::Running, JobState::Succeeded),
        (JobState::Running, JobState::Failed),
        (JobState::Running, JobState::Cancelled),
    ];
    let all = [
        JobState::Queued,
        JobState::Running,
        JobState::Succeeded,
        JobState::Failed,
        JobState::Cancelled,
    ];

    for from in all {
        for to in all {
            let expected = legal.contains(&(from, to));
            assert_eq!(
                from.can_transition_to(to),
                expected,
                "{from} -> {to} legality"
            );
        }
    }
}

#[test]
fn a_terminal_state_is_never_revised() {
    for terminal in [JobState::Succeeded, JobState::Failed, JobState::Cancelled] {
        assert!(terminal.is_terminal());
        for next in [
            JobState::Queued,
            JobState::Running,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Cancelled,
        ] {
            assert!(!terminal.can_transition_to(next), "{terminal} -> {next}");
        }
    }
}

#[test]
fn a_refused_transition_reports_the_job_the_states_and_leaves_it_alone() {
    let mut job = Job::queued(id("job-7"), fill("radar"));
    let error = job
        .transition(JobState::Succeeded)
        .expect_err("nothing ran");
    let text = error.to_string();
    assert!(text.contains("job-7"), "the job is named: {text}");
    assert!(
        text.contains("queued") && text.contains("succeeded"),
        "{text}"
    );
    assert_eq!(
        job.state(),
        JobState::Queued,
        "a refused move is not applied"
    );
}

#[test]
fn the_worker_set_is_exactly_the_wire_vocabulary() {
    let names: Vec<&str> = [
        JobState::Queued,
        JobState::Running,
        JobState::Succeeded,
        JobState::Failed,
        JobState::Cancelled,
    ]
    .iter()
    .map(|state| state.as_str())
    .collect();
    assert_eq!(
        names,
        vec!["queued", "running", "succeeded", "failed", "cancelled"]
    );
}

#[test]
fn a_repeat_trigger_for_a_fill_queued_or_running_coalesces() {
    let mut queue = JobQueue::new(4, 1);
    assert_eq!(
        queue.trigger(fill("radar"), id("job-1")).expect("room"),
        Trigger::Queued(id("job-1"))
    );
    // A second press while the first job waits.
    assert_eq!(
        queue
            .trigger(fill("radar"), id("job-2"))
            .expect("coalesces"),
        Trigger::Coalesced(id("job-1"))
    );
    // And while it runs.
    let running = queue.start_next().expect("a slot").expect("a job");
    assert_eq!(running.id, id("job-1"));
    assert_eq!(running.state(), JobState::Running);
    assert_eq!(
        queue
            .trigger(fill("radar"), id("job-3"))
            .expect("coalesces"),
        Trigger::Coalesced(id("job-1"))
    );
    assert_eq!(queue.pending_len(), 0, "no extra job was queued");
    assert_eq!(queue.running_len(), 1);

    // A different fill is a separate job.
    assert_eq!(
        queue.trigger(fill("wind"), id("job-4")).expect("room"),
        Trigger::Queued(id("job-4"))
    );

    // The trigger that coalesced into the RUNNING j1 is not dropped: finishing
    // j1 queues exactly one rerun for that fill (ADR-0100 decision 4).
    assert_eq!(queue.pending_rerun_for(&fill("radar")), Some(&id("job-3")));
    queue
        .finish(&id("job-1"), JobState::Succeeded)
        .expect("finish");
    assert_eq!(queue.in_flight_for(&fill("radar")), Some(&id("job-3")));
    assert_eq!(
        queue.pending_len(),
        2,
        "the waiting wind job plus exactly one radar rerun"
    );
    // A further trigger now coalesces onto the rerun rather than adding a job.
    assert_eq!(
        queue
            .trigger(fill("radar"), id("job-5"))
            .expect("coalesces"),
        Trigger::Coalesced(id("job-3"))
    );
    assert_eq!(queue.pending_len(), 2, "still exactly one radar rerun");
}

#[test]
fn the_queue_bound_refuses_a_new_job_but_never_a_coalesced_one() {
    let mut queue = JobQueue::new(2, 1);
    queue.trigger(fill("a"), id("job-1")).expect("room");
    queue.trigger(fill("b"), id("job-2")).expect("room");
    assert!(!queue.could_accept());
    assert_eq!(queue.tracked_len(), 2);

    let error = queue.trigger(fill("c"), id("job-3")).expect_err("full");
    assert!(
        error.to_string().contains('2'),
        "the bound is named: {error}"
    );
    assert_eq!(queue.pending_len(), 2, "a refused trigger adds nothing");
    assert_eq!(
        queue
            .trigger(fill("a"), id("job-9"))
            .expect("coalesces even at the bound"),
        Trigger::Coalesced(id("job-1"))
    );
}

#[test]
fn the_concurrency_limit_bounds_the_running_window() {
    let mut queue = JobQueue::new(8, 2);
    for (index, name) in ["a", "b", "c"].iter().enumerate() {
        queue
            .trigger(fill(name), id(&format!("job-{index}")))
            .expect("room");
    }
    let first = queue.start_next().expect("a slot").expect("a job");
    let second = queue.start_next().expect("a slot").expect("a job");
    assert_ne!(first.id, second.id);
    assert_eq!(queue.running_len(), 2);
    assert!(
        queue.start_next().is_err(),
        "the third job waits for a free slot"
    );
    assert_eq!(queue.pending_len(), 1);

    queue.finish(&first.id, JobState::Failed).expect("finish");
    let third = queue
        .start_next()
        .expect("a slot")
        .expect("the waiting job");
    assert_eq!(third.id, id("job-2"));
    assert_eq!(queue.running_len(), 2);
}

#[test]
fn one_runner_never_starts_a_job_for_a_fill_already_running() {
    // Coalescing is what makes the single-runner default safe: two triggers for
    // one fill can never both reach a runner.
    let mut queue = JobQueue::new(4, 4);
    queue.trigger(fill("radar"), id("job-1")).expect("room");
    queue
        .trigger(fill("radar"), id("job-2"))
        .expect("coalesces");
    queue
        .trigger(fill("radar"), id("job-3"))
        .expect("coalesces");

    let started = queue.start_next().expect("a slot").expect("a job");
    assert_eq!(started.id, id("job-1"));
    assert!(
        queue.start_next().expect("a slot").is_none(),
        "nothing else is pending for that fill"
    );
    assert_eq!(queue.running_ids(), vec![id("job-1")]);
}

#[test]
fn cancelling_a_waiting_job_frees_its_fill_and_a_running_one_frees_its_slot() {
    let mut queue = JobQueue::new(4, 1);
    queue.trigger(fill("a"), id("job-1")).expect("room");
    queue.trigger(fill("b"), id("job-2")).expect("room");

    assert_eq!(
        queue.cancel(&id("job-2")).expect("cancel"),
        JobState::Cancelled
    );
    assert!(!queue.is_fill_active(&fill("b")));
    assert!(queue
        .trigger(fill("b"), id("job-3"))
        .expect("room")
        .queued_new_job());

    queue.start_next().expect("a slot").expect("a job");
    let before = queue.running_len();
    assert_eq!(
        queue.cancel(&id("job-1")).expect("cancel"),
        JobState::Cancelled
    );
    assert!(queue.running_len() < before, "the runner slot is freed");
    assert!(queue.cancel(&id("job-1")).is_err(), "already cancelled");
    assert!(queue
        .trigger(fill("a"), id("job-4"))
        .expect("room")
        .queued_new_job());
}

#[test]
fn finishing_an_unknown_job_is_an_error_not_a_silent_success() {
    let mut queue = JobQueue::new(2, 1);
    assert!(queue.finish(&id("nope"), JobState::Succeeded).is_err());
    queue.trigger(fill("a"), id("job-1")).expect("room");
    queue.start_next().expect("a slot").expect("a job");
    // A non-terminal "finish" is an illegal move, not a completion.
    assert!(queue.finish(&id("job-1"), JobState::Running).is_err());
    assert_eq!(queue.running_len(), 1, "the job is still running");
    assert!(queue.finish(&id("job-1"), JobState::Cancelled).is_ok());
    assert!(queue.finish(&id("job-1"), JobState::Succeeded).is_err());
}

#[test]
fn a_zero_bound_is_raised_so_the_runner_cannot_wedge() {
    let mut queue = JobQueue::new(0, 0);
    assert_eq!(queue.capacity(), 1);
    assert_eq!(queue.max_concurrent(), 1);
    assert!(queue.trigger(fill("a"), id("job-1")).is_ok());
    assert!(queue.start_next().expect("a slot").is_some());
}

#[test]
fn a_cancelled_job_does_not_occupy_capacity() {
    let mut queue = JobQueue::new(1, 1);
    queue.trigger(fill("a"), id("job-1")).expect("room");
    assert!(!queue.could_accept(), "the single slot is taken");

    assert_eq!(
        queue.cancel(&id("job-1")).expect("cancel"),
        JobState::Cancelled
    );
    assert_eq!(queue.tracked_len(), 0, "the cancelled entry is gone");
    assert!(queue.could_accept());
    assert_eq!(
        queue.trigger(fill("b"), id("job-2")).expect("room"),
        Trigger::Queued(id("job-2"))
    );
}

#[test]
fn a_duplicate_job_id_is_refused() {
    let mut queue = JobQueue::new(4, 1);
    queue.trigger(fill("a"), id("job-1")).expect("room");
    let error = queue
        .trigger(fill("b"), id("job-1"))
        .expect_err("the id is already tracked");
    assert!(error.to_string().contains("job-1"), "{error}");
    assert_eq!(queue.pending_len(), 1);
}

#[test]
fn the_queue_owns_the_running_record_and_transitions_it() {
    // `finish` moves the queue's own record through `Job::transition`, so a
    // non-terminal "finish" is refused and the job keeps running.
    let mut queue = JobQueue::new(2, 1);
    queue.trigger(fill("a"), id("job-1")).expect("room");
    let started = queue.start_next().expect("a slot").expect("a job");
    assert_eq!(started.state(), JobState::Running);
    assert!(queue.finish(&id("job-1"), JobState::Running).is_err());
    assert_eq!(queue.running_len(), 1, "the refused finish changed nothing");
    assert_eq!(queue.running_ids(), vec![id("job-1")]);
    assert_eq!(
        queue
            .finish(&id("job-1"), JobState::Failed)
            .expect("finish"),
        JobState::Failed
    );
    assert_eq!(queue.running_len(), 0);
}
