//! The clip-job state model and the bounded, coalescing queue
//! (ADR-0100 decision 4).
//!
//! One runner executes every clip job with a bounded concurrency. A job is
//! `queued`, `running`, `succeeded`, `failed` or `cancelled`, carries a named
//! phase and a percent where known, and a **repeat trigger for a fill that
//! already has a job queued or running coalesces** — the operator pressing
//! "run" twice must not queue two browser captures for one slot.
//!
//! Everything here is pure logic with no clock of its own: the queue is a data
//! structure, and the later runner is what actually runs the browsers. There are
//! no threads, no timers and no allocation beyond the job ids themselves.

use std::collections::{HashMap, VecDeque};

use crate::error::{Error, Result};

/// The stable id of a fill definition (`[[media_fill]] id`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct FillId(String);

impl FillId {
    /// Name a fill.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The fill's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A clip job's id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct JobId(String);

impl JobId {
    /// Name a job.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The job's id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The named phase a job is in.
///
/// A newtype over the phase vocabulary (`queued`, `browser_start`, `page_load`,
/// `login`, `first_frame`, `capture`, `encode`, `promote`, ...) so the set can
/// grow with the job kinds without a model change, while a phase is still a
/// single owned string on the record and the wire.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct JobPhase(String);

impl JobPhase {
    /// The phase of a job that is waiting for a runner slot.
    pub const QUEUED: &'static str = "queued";
    /// The phase of a job that has finished.
    pub const DONE: &'static str = "done";

    /// Name a phase.
    #[must_use]
    pub fn new(phase: impl Into<String>) -> Self {
        Self(phase.into())
    }

    /// The queued phase.
    #[must_use]
    pub fn queued() -> Self {
        Self(Self::QUEUED.to_owned())
    }

    /// The finished phase.
    #[must_use]
    pub fn done() -> Self {
        Self(Self::DONE.to_owned())
    }

    /// The phase's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A job's lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Accepted, waiting for a runner slot.
    Queued,
    /// Executing.
    Running,
    /// Finished successfully; the clip is being (or has been) promoted.
    Succeeded,
    /// Finished with a failure that must be reported with a named reason.
    Failed,
    /// Cancelled by an operator or by admission.
    Cancelled,
}

impl JobState {
    /// The wire/state-machine name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether this state is terminal (no further transition is legal).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// Whether a job in this state has been dispatched to a runner.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    /// Whether `next` is a legal successor of this state.
    ///
    /// The legal moves are exactly ADR-0100 decision 4's:
    /// `queued -> running | cancelled`, `running -> succeeded | failed |
    /// cancelled`, and nothing out of a terminal state. In particular a job may
    /// not move straight from `queued` to `succeeded` (nothing ran) and a
    /// terminal state may not be revised.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Succeeded | Self::Failed | Self::Cancelled
                )
        )
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One job record.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Job {
    /// The job's id.
    pub id: JobId,
    /// The fill this job fills.
    pub fill: FillId,
    /// Where the job is in its lifecycle.
    pub state: JobState,
    /// The named phase within the job kind.
    pub phase: JobPhase,
    /// Completion percentage, when the phase can estimate one.
    pub percent: Option<u8>,
}

impl Job {
    /// A freshly queued job.
    #[must_use]
    pub fn queued(id: JobId, fill: FillId) -> Self {
        Self {
            id,
            fill,
            state: JobState::Queued,
            phase: JobPhase::queued(),
            percent: None,
        }
    }

    /// Move the job to `next`, rejecting an illegal move.
    ///
    /// This is the only way to change [`Job::state`]: a state transition cannot
    /// be written by a caller that has not gone through the guard.
    ///
    /// # Errors
    ///
    /// [`Error::IllegalTransition`] when the lifecycle does not allow the move.
    pub fn transition(&mut self, next: JobState) -> Result<JobState> {
        if !self.state.can_transition_to(next) {
            return Err(Error::IllegalTransition {
                job: self.id.as_str().to_owned(),
                from: self.state.as_str(),
                to: next.as_str(),
            });
        }
        self.state = next;
        if next.is_terminal() {
            self.phase = JobPhase::done();
            if next == JobState::Succeeded {
                self.percent = Some(100);
            }
        }
        Ok(next)
    }

    /// Set the phase and percentage, with the percentage confined to `0..=100`.
    ///
    /// # Errors
    ///
    /// [`Error::NotAPercent`] when `percent` does not fit `u8`, or is above 100.
    pub fn set_progress(&mut self, phase: JobPhase, percent: Option<u32>) -> Result<()> {
        self.phase = phase;
        self.percent = match percent {
            None => None,
            Some(value) => Some(percent_u8(value)?),
        };
        Ok(())
    }
}

/// Narrow a percentage to `u8`, refusing anything above 100.
///
/// # Errors
///
/// [`Error::NotAPercent`] when `value > 100` or does not fit `u8`.
pub fn percent_u8(value: u32) -> Result<u8> {
    if value > 100 {
        return Err(Error::NotAPercent { value });
    }
    u8::try_from(value).map_err(|_| Error::NotAPercent { value })
}

/// What a trigger for a fill actually did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A new job was queued.
    Queued(JobId),
    /// The fill already had a job queued or running, so the trigger coalesced
    /// onto it (ADR-0100 decision 4).
    Coalesced(JobId),
}

impl Trigger {
    /// The job the trigger applies to, whether new or coalesced.
    #[must_use]
    pub fn job(&self) -> &JobId {
        match self {
            Self::Queued(id) | Self::Coalesced(id) => id,
        }
    }

    /// Whether a new job was created.
    #[must_use]
    pub const fn queued_new_job(&self) -> bool {
        matches!(self, Self::Queued(_))
    }
}

/// A bounded job queue with a concurrency window and per-fill coalescing.
///
/// `capacity` bounds how many jobs are tracked at once (queued **or** running),
/// so the structure cannot grow without limit; `max_concurrent` bounds how many
/// may be running at once, which is the same statement as
/// `media_jobs.concurrency` (default 1).
#[derive(Clone, Debug)]
pub struct JobQueue {
    capacity: usize,
    max_concurrent: usize,
    pending: VecDeque<Job>,
    /// The job in flight per fill, oldest first within a fill: the coalescing
    /// key. One entry per fill with a job queued **or** running, so a repeat
    /// trigger can be answered with the job it coalesced onto.
    in_flight: HashMap<FillId, JobId>,
    /// The currently running job ids, oldest first.
    running: Vec<JobId>,
}

impl JobQueue {
    /// A queue bounded at `capacity` tracked jobs and `max_concurrent` running.
    ///
    /// Both bounds are raised to at least 1, so a misconfigured `0` cannot wedge
    /// the runner into never accepting work.
    #[must_use]
    pub fn new(capacity: usize, max_concurrent: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            max_concurrent: max_concurrent.max(1),
            pending: VecDeque::new(),
            in_flight: HashMap::new(),
            running: Vec::new(),
        }
    }

    /// The tracked-job bound.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The concurrency bound.
    #[must_use]
    pub const fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// How many jobs are waiting for a runner slot.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// How many jobs are currently running.
    #[must_use]
    pub fn running_len(&self) -> usize {
        self.running.len()
    }

    /// How many jobs are tracked (waiting or running).
    #[must_use]
    pub fn tracked_len(&self) -> usize {
        self.pending.len().saturating_add(self.running.len())
    }

    /// Whether the queue could take one more job right now.
    #[must_use]
    pub fn could_accept(&self) -> bool {
        self.tracked_len() < self.capacity
    }

    /// Trigger a run for `fill`.
    ///
    /// A trigger for a fill that already has a job queued or running coalesces
    /// onto that job and creates nothing. Otherwise a new job with `id` is
    /// queued, or the trigger is refused when the queue is at its bound.
    ///
    /// # Errors
    ///
    /// [`Error::QueueFull`] when `fill` has no job in flight and the queue is at
    /// capacity.
    pub fn trigger(&mut self, fill: FillId, id: JobId) -> Result<Trigger> {
        if let Some(existing) = self.in_flight.get(&fill) {
            return Ok(Trigger::Coalesced(existing.clone()));
        }
        if !self.could_accept() {
            return Err(Error::QueueFull {
                limit: self.capacity,
            });
        }
        self.in_flight.insert(fill.clone(), id.clone());
        self.pending.push_back(Job::queued(id.clone(), fill));
        Ok(Trigger::Queued(id))
    }

    /// Start the next pending job, if a runner slot is free.
    ///
    /// A job that was cancelled while waiting is dropped rather than started.
    ///
    /// # Errors
    ///
    /// [`Error::QueueFull`] when the concurrency window is already full.
    pub fn start_next(&mut self) -> Result<Option<Job>> {
        if self.running.len() >= self.max_concurrent {
            return Err(Error::QueueFull {
                limit: self.max_concurrent,
            });
        }
        while let Some(mut job) = self.pending.pop_front() {
            if job.state != JobState::Queued {
                // Cancelled while it waited (see `cancel`): nothing to run.
                continue;
            }
            job.transition(JobState::Running)?;
            self.running.push(job.id.clone());
            return Ok(Some(job));
        }
        Ok(None)
    }

    /// Finish `id` in `terminal` (`succeeded`, `failed` or `cancelled`).
    ///
    /// # Errors
    ///
    /// [`Error::UnknownJob`] when `id` is not currently running;
    /// [`Error::IllegalTransition`] when `terminal` is not a legal successor of
    /// the running state (only reachable by passing a non-terminal state).
    pub fn finish(&mut self, id: &JobId, terminal: JobState) -> Result<JobState> {
        if terminal != JobState::Succeeded
            && terminal != JobState::Failed
            && terminal != JobState::Cancelled
        {
            return Err(Error::IllegalTransition {
                job: id.as_str().to_owned(),
                from: JobState::Running.as_str(),
                to: terminal.as_str(),
            });
        }
        if !self.running.iter().any(|running| running == id) {
            return Err(Error::UnknownJob {
                job: id.as_str().to_owned(),
            });
        }
        self.running.retain(|running| running != id);
        self.in_flight.retain(|_, in_flight| in_flight != id);
        Ok(terminal)
    }

    /// Cancel a job, whether it is waiting or running.
    ///
    /// A job that has not been discovered in either set was already cancelled or
    /// finished.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownJob`] when `id` is not tracked.
    pub fn cancel(&mut self, id: &JobId) -> Result<JobState> {
        if let Some(job) = self.pending.iter_mut().find(|job| &job.id == id) {
            job.transition(JobState::Cancelled)?;
            self.in_flight.retain(|_, in_flight| in_flight != id);
            return Ok(JobState::Cancelled);
        }
        if self.running.iter().any(|running| running == id) {
            self.running.retain(|running| running != id);
            self.in_flight.retain(|_, in_flight| in_flight != id);
            return Ok(JobState::Cancelled);
        }
        Err(Error::UnknownJob {
            job: id.as_str().to_owned(),
        })
    }

    /// The pending jobs, oldest first, for diagnostics and tests.
    #[must_use]
    pub fn pending(&self) -> Vec<&Job> {
        self.pending.iter().collect()
    }

    /// The ids of the jobs currently running, oldest first.
    #[must_use]
    pub fn running_ids(&self) -> &[JobId] {
        &self.running
    }

    /// Whether `fill` has a job queued or running.
    #[must_use]
    pub fn is_fill_active(&self, fill: &FillId) -> bool {
        self.in_flight.contains_key(fill)
    }

    /// The job currently in flight for `fill`, queued or running.
    #[must_use]
    pub fn in_flight_for(&self, fill: &FillId) -> Option<&JobId> {
        self.in_flight.get(fill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_id(name: &str) -> JobId {
        JobId::new(name)
    }

    fn fill(name: &str) -> FillId {
        FillId::new(name)
    }

    #[test]
    fn legal_transitions_are_accepted() {
        let mut job = Job::queued(job_id("j1"), fill("f1"));
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(
            job.transition(JobState::Running)
                .expect("queued -> running"),
            JobState::Running
        );
        job.set_progress(JobPhase::new("browser_start"), Some(25))
            .expect("25%");
        assert_eq!(job.percent, Some(25));
        assert_eq!(job.phase.as_str(), "browser_start");
        assert_eq!(
            job.transition(JobState::Succeeded)
                .expect("running -> succeeded"),
            JobState::Succeeded
        );
        assert_eq!(job.percent, Some(100));
        assert_eq!(job.phase.as_str(), JobPhase::DONE);
    }

    #[test]
    fn queued_may_be_cancelled_and_running_may_fail() {
        let mut queued = Job::queued(job_id("j1"), fill("f1"));
        assert_eq!(
            queued.transition(JobState::Cancelled).expect("cancel"),
            JobState::Cancelled
        );

        let mut running = Job::queued(job_id("j2"), fill("f2"));
        running.transition(JobState::Running).expect("start");
        assert_eq!(
            running.transition(JobState::Failed).expect("fail"),
            JobState::Failed
        );
        assert_eq!(
            running.percent, None,
            "a failed job has no completion percent"
        );
    }

    #[test]
    fn illegal_transitions_are_rejected_and_leave_the_state_alone() {
        // queued -> succeeded (nothing ran)
        let mut job = Job::queued(job_id("j1"), fill("f1"));
        assert!(job.transition(JobState::Succeeded).is_err());
        assert_eq!(
            job.state,
            JobState::Queued,
            "a refused move must not change state"
        );
        assert!(
            job.transition(JobState::Queued).is_err(),
            "no self transition"
        );

        // terminal states are final
        job.transition(JobState::Running).expect("start");
        job.transition(JobState::Succeeded).expect("finish");
        for next in [
            JobState::Running,
            JobState::Failed,
            JobState::Cancelled,
            JobState::Succeeded,
        ] {
            assert!(
                job.transition(next).is_err(),
                "a terminal state must reject {next}"
            );
        }
    }

    #[test]
    fn percent_above_one_hundred_is_refused() {
        let mut job = Job::queued(job_id("j1"), fill("f1"));
        assert!(job
            .set_progress(JobPhase::new("capture"), Some(101))
            .is_err());
        assert_eq!(job.percent, None);
        assert!(percent_u8(100).is_ok());
        assert!(percent_u8(1_000).is_err());
    }

    #[test]
    fn a_second_trigger_for_an_active_fill_coalesces() {
        let mut queue = JobQueue::new(8, 1);
        let first = queue.trigger(fill("radar"), job_id("j1")).expect("room");
        assert!(first.queued_new_job());

        // Still queued: the repeat trigger coalesces onto j1.
        let repeat = queue
            .trigger(fill("radar"), job_id("j2"))
            .expect("coalesces");
        assert_eq!(repeat, Trigger::Coalesced(job_id("j1")));
        assert_eq!(queue.pending_len(), 1, "no second job");

        // Running: a repeat trigger still coalesces.
        let running = queue.start_next().expect("slot").expect("a job");
        assert_eq!(running.id, job_id("j1"));
        let repeat = queue
            .trigger(fill("radar"), job_id("j3"))
            .expect("coalesces");
        assert_eq!(repeat, Trigger::Coalesced(job_id("j1")));
        assert_eq!(queue.pending_len(), 0);
        assert_eq!(queue.running_len(), 1);

        // A different fill is a separate job.
        let other = queue.trigger(fill("wind"), job_id("j4")).expect("room");
        assert_eq!(other, Trigger::Queued(job_id("j4")));

        // Once the first job finishes, the fill can be triggered again.
        queue
            .finish(&job_id("j1"), JobState::Succeeded)
            .expect("finish");
        let again = queue.trigger(fill("radar"), job_id("j5")).expect("room");
        assert_eq!(again, Trigger::Queued(job_id("j5")));
    }

    #[test]
    fn the_queue_bound_refuses_a_new_job() {
        let mut queue = JobQueue::new(2, 1);
        assert!(queue.could_accept());
        queue.trigger(fill("a"), job_id("j1")).expect("room");
        queue.trigger(fill("b"), job_id("j2")).expect("room");
        assert!(!queue.could_accept());
        assert_eq!(queue.tracked_len(), 2);
        assert!(queue.trigger(fill("c"), job_id("j3")).is_err());
        assert_eq!(queue.pending_len(), 2, "a refused trigger adds nothing");

        // A coalesced trigger is still allowed at the bound.
        assert_eq!(
            queue.trigger(fill("a"), job_id("j9")).expect("coalesces"),
            Trigger::Coalesced(job_id("j1"))
        );
    }

    #[test]
    fn the_concurrency_bound_limits_the_running_window() {
        let mut queue = JobQueue::new(4, 1);
        queue.trigger(fill("a"), job_id("j1")).expect("room");
        queue.trigger(fill("b"), job_id("j2")).expect("room");

        let first = queue.start_next().expect("slot").expect("a job");
        assert_eq!(first.id, job_id("j1"));
        assert!(queue.start_next().is_err(), "one runner, one job");
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.running_len(), 1);

        queue
            .finish(&job_id("j1"), JobState::Succeeded)
            .expect("finish");
        let second = queue.start_next().expect("slot").expect("a job");
        assert_eq!(second.id, job_id("j2"));
    }

    #[test]
    fn cancelling_a_waiting_job_drops_it_instead_of_starting_it() {
        let mut queue = JobQueue::new(4, 1);
        queue.trigger(fill("a"), job_id("j1")).expect("room");
        assert_eq!(
            queue.cancel(&job_id("j1")).expect("cancel"),
            JobState::Cancelled
        );
        assert!(!queue.is_fill_active(&fill("a")));
        assert!(queue.start_next().expect("no slot issue").is_none());
        // The fill can be triggered again after cancellation.
        assert!(queue
            .trigger(fill("a"), job_id("j2"))
            .expect("room")
            .queued_new_job());
    }

    #[test]
    fn cancelling_a_running_job_frees_the_slot() {
        let mut queue = JobQueue::new(2, 1);
        queue.trigger(fill("a"), job_id("j1")).expect("room");
        queue.start_next().expect("slot").expect("a job");
        assert_eq!(
            queue.cancel(&job_id("j1")).expect("cancel"),
            JobState::Cancelled
        );
        assert_eq!(queue.running_len(), 0);
        assert!(queue.start_next().expect("slot free").is_none());
        assert!(
            queue.cancel(&job_id("nope")).is_err(),
            "unknown id is an error"
        );
    }

    #[test]
    fn a_zero_bound_is_raised_to_one() {
        let mut queue = JobQueue::new(0, 0);
        assert_eq!(queue.capacity(), 1);
        assert_eq!(queue.max_concurrent(), 1);
        assert!(queue.trigger(fill("a"), job_id("j1")).is_ok());
    }
}
