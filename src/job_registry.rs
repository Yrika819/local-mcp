//! Bounded lifecycle for background command jobs.
//!
//! # Why this registry is bounded
//!
//! Background jobs used to be retained until somebody polled or stopped them. A
//! finished job that nobody polled kept its `JoinHandle` *and* the whole rendered
//! output payload for the life of the process, so N unpolled jobs retained N full
//! outputs. Output capture is bounded now (see `sandbox`), but retention is a
//! separate problem: the payload still has to be retained *somewhere*, and a
//! client that never polls must not be able to grow that without limit.
//!
//! Three states are therefore distinguished explicitly:
//!
//! * **Running** — a live `JoinHandle`. Never expired by TTL, and never evicted
//!   to make room for another job.
//! * **Finished / retained** — completed, result still pollable.
//! * **Expired / evicted** — removed by GC; a later poll reports it as unknown or
//!   expired.
//!
//! # Why GC is opportunistic
//!
//! There is no maintenance thread. GC runs on the natural control-plane calls
//! that already hold the registry lock: admitting a job, polling one, stopping
//! one, and releasing a session. Because the registry is itself capped, a GC pass
//! is O(cap) and cannot grow with history.
//!
//! # Why the clock is injected
//!
//! Every elapsed-duration decision uses a monotonic [`Instant`] passed in by the
//! caller, never a wall clock. Tests drive expiry by supplying a synthetic instant
//! instead of sleeping.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use crate::resource_limits::{
    FINISHED_JOB_TTL, MAX_BACKGROUND_JOBS_GLOBAL, MAX_BACKGROUND_JOBS_PER_SESSION, ResourceLimit,
};

/// One retained background command.
///
/// The retained `String` is the already-bounded rendered result: output limits are
/// applied at capture time, so a finished job can never hold a payload larger
/// than the command output bounds.
pub(crate) struct Job {
    session_id: String,
    command: String,
    handle: JoinHandle<Result<String>>,
    /// When the host first observed this job finished.
    ///
    /// Recorded on observation rather than by wrapping the task, because that
    /// keeps the TTL honest without spawning a supervisor per job. The
    /// consequence is deliberate: expiry starts when the host *notices*, so a
    /// result is retained slightly longer than TTL and never shorter. Premature
    /// expiry would be the dangerous direction.
    finished_at: Option<Instant>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Job")
            .field("session_id", &self.session_id)
            .field("command", &self.command)
            .field("finished", &self.finished_at.is_some())
            .finish()
    }
}

impl Job {
    pub(crate) fn new(
        session_id: String,
        command: String,
        handle: JoinHandle<Result<String>>,
    ) -> Self {
        Self {
            session_id,
            command,
            handle,
            finished_at: None,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    /// Request task cancellation without awaiting it.
    ///
    /// Shutdown uses this for every retained job before its first await, so
    /// cancellation of the shutdown future cannot detach jobs later in the batch.
    pub(crate) fn request_termination(&self) {
        self.handle.abort();
    }

    /// Stop the job's task and wait for it to unwind.
    ///
    /// Dropping the spawned task is what tears the child down: the process-group
    /// guard inside it signals the whole group on drop. This is the same
    /// mechanism `stop_job` always used; no broader process authority is added.
    pub(crate) async fn terminate(self) {
        self.request_termination();
        let _ = self.handle.await;
    }

    /// Wait for the job to finish and take its rendered result.
    pub(crate) async fn join(self) -> Result<String> {
        self.handle
            .await
            .context("background command task failed")?
    }
}

#[derive(Default)]
pub(crate) struct JobRegistry {
    jobs: HashMap<Uuid, Job>,
}

impl JobRegistry {
    /// Drop finished results whose retention period has elapsed.
    ///
    /// Only finished jobs are considered: a running job is never expired, however
    /// old its entry is, and GC never signals a process.
    fn collect_expired(&mut self, now: Instant) {
        // First, stamp jobs that have just finished, so their retention period is
        // measured from the observation rather than from an arbitrary earlier one.
        for job in self.jobs.values_mut() {
            if job.finished_at.is_none() && job.handle.is_finished() {
                job.finished_at = Some(now);
            }
        }
        let ttl = FINISHED_JOB_TTL;
        self.jobs.retain(|_, job| match job.finished_at {
            // Still running: retained regardless of age.
            None => true,
            Some(finished_at) => now.saturating_duration_since(finished_at) < ttl,
        });
    }

    fn count_for_session(&self, session_id: &str) -> usize {
        self.jobs
            .values()
            .filter(|job| job.session_id == session_id)
            .count()
    }

    /// Decide whether a new job may be created for `session_id`, after GC.
    ///
    /// This is a *check*, not a reservation. It lets a request fail before any
    /// process is spawned; [`JobRegistry::insert`] then performs the retention
    /// while the same lock is still held, so the two cannot disagree.
    ///
    /// Both ceilings are enforced here rather than by evicting: a running job is
    /// never displaced to admit another job, and one Session can never displace
    /// another Session's job.
    pub(crate) fn can_admit(&mut self, now: Instant, session_id: &str) -> Result<()> {
        self.collect_expired(now);
        // Typed rather than formatted, so the transport reports a resource
        // failure instead of a generic server error.
        crate::resource_limits::ensure_resource(
            self.count_for_session(session_id) < MAX_BACKGROUND_JOBS_PER_SESSION,
            ResourceLimit::BackgroundJobsPerSession,
            "stop or poll a retained job before starting another",
        )?;
        crate::resource_limits::ensure_resource(
            self.jobs.len() < MAX_BACKGROUND_JOBS_GLOBAL,
            ResourceLimit::BackgroundJobsGlobal,
            "stop or poll a retained job before starting another",
        )?;
        Ok(())
    }

    /// Retain a new job.
    ///
    /// Infallible by construction: the caller must already have established
    /// capacity with [`JobRegistry::can_admit`] while still holding the registry
    /// lock. Keeping the check outside this method means a rejected job is still
    /// owned by its caller, which can terminate it — dropping a `JoinHandle`
    /// detaches the task rather than stopping the child.
    pub(crate) fn insert(&mut self, now: Instant, job_id: Uuid, job: Job) {
        self.collect_expired(now);
        self.jobs.insert(job_id, job);
    }

    /// Look up a job that belongs to `session_id`.
    ///
    /// Cross-Session access is refused: the job is not revealed, stopped, or
    /// otherwise disturbed.
    pub(crate) fn take(&mut self, now: Instant, job_id: Uuid, session_id: &str) -> Result<Job> {
        self.collect_expired(now);
        let job = self
            .jobs
            .get(&job_id)
            .with_context(|| "unknown or expired job_id")?;
        anyhow::ensure!(
            job.session_id() == session_id,
            "job does not belong to this session"
        );
        self.jobs
            .remove(&job_id)
            .context("job disappeared under the registry lock")
    }

    /// Remove a job only if it has already finished, returning it for polling.
    ///
    /// A still-running job stays registered and is reported as running, so a poll
    /// can never destroy a job the caller expects to keep polling.
    pub(crate) fn take_if_finished(
        &mut self,
        now: Instant,
        job_id: Uuid,
        session_id: &str,
    ) -> Result<Option<Job>> {
        self.collect_expired(now);
        let job = self
            .jobs
            .get(&job_id)
            .with_context(|| "unknown or expired job_id")?;
        anyhow::ensure!(
            job.session_id() == session_id,
            "job does not belong to this session"
        );
        if job.handle.is_finished() {
            Ok(Some(self.jobs.remove(&job_id).context(
                "finished job disappeared under the registry lock",
            )?))
        } else {
            Ok(None)
        }
    }

    /// Remove and return every job belonging to `session_id`.
    ///
    /// Used when a Session is torn down. Jobs belonging to other Sessions are
    /// untouched: cleanup is scoped, never a system-wide sweep.
    #[expect(
        dead_code,
        reason = "Audit finding: Resource Bounds V1 has no Session-teardown signal to call this. `local-mcp start` and `local-mcp mcp` are separate processes, exiting the UI does not notify the MCP server, and the durable session record is not removed on exit. Retention is therefore bounded by the per-session ceiling, the global ceiling and FINISHED_JOB_TTL instead. This is the scoped teardown operation, exercised directly by tests."
    )]
    pub(crate) fn release_session(&mut self, session_id: &str) -> Vec<Job> {
        let owned: Vec<Uuid> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.session_id() == session_id)
            .map(|(job_id, _)| *job_id)
            .collect();
        owned
            .into_iter()
            .filter_map(|job_id| self.jobs.remove(&job_id))
            .collect()
    }

    /// Remove and return every retained job.
    ///
    /// Used when the process that owns the registry is shutting down. Abnormal
    /// parent death is a separate, later concern; this is explicit shutdown only.
    pub(crate) fn release_all(&mut self) -> Vec<Job> {
        self.jobs.drain().map(|(_, job)| job).collect()
    }

    #[cfg(test)]
    pub(crate) fn snapshot_len(&self) -> usize {
        self.jobs.len()
    }

    /// Any retained job identifier, for stress-audit iteration.
    #[cfg(test)]
    pub(crate) fn first_job_id_for_test(&self) -> Option<Uuid> {
        self.jobs.keys().next().copied()
    }

    /// Whether a job id is currently registered, for assertions in existing tests.
    #[cfg(test)]
    pub(crate) fn contains_key_for_test(&self, job_id: &Uuid) -> bool {
        self.jobs.contains_key(job_id)
    }

    /// Yield until a retained job reports itself finished.
    ///
    /// Only used by tests, which construct their own registry rather than the
    /// process-global one so that parallel test threads cannot interfere.
    #[cfg(test)]
    pub(crate) async fn wait_until_finished(&mut self, job_id: Uuid) {
        loop {
            let finished = self
                .jobs
                .get(&job_id)
                .map(|job| job.handle.is_finished())
                .unwrap_or(true);
            if finished {
                return;
            }
            tokio::task::yield_now().await;
        }
    }
}

/// The process-global registry.
pub(crate) fn registry() -> MutexGuard<'static, JobRegistry> {
    static JOBS: OnceLock<Mutex<JobRegistry>> = OnceLock::new();
    let jobs = JOBS.get_or_init(|| Mutex::new(JobRegistry::default()));
    // A poisoned lock would mean a panic while the lock was held. Recovering the
    // guard keeps one panicking handler from making the whole control plane
    // permanently unusable, and the registry holds no unwinding invariants: it is
    // rebuilt from scratch on every failure path anyway.
    match jobs.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Monotonic instant used for every registry decision.
pub(crate) fn now() -> Instant {
    Instant::now()
}

/// Allocate an identifier for a job that is about to be created.
pub(crate) fn new_job_id() -> Uuid {
    Uuid::new_v4()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_limits::{MAX_BACKGROUND_JOBS_GLOBAL, MAX_BACKGROUND_JOBS_PER_SESSION};

    /// A synthetic monotonic instant. TTL tests never sleep.
    fn at(seconds: u64) -> Instant {
        Instant::now() + std::time::Duration::from_secs(seconds)
    }

    /// A job whose task has already run to completion.
    fn completed_job(session: &str) -> (Uuid, Job) {
        let job_id = Uuid::new_v4();
        let handle = tokio::spawn(async { Ok("{\"exit_code\":0}".to_owned()) });
        (
            job_id,
            Job::new(session.to_owned(), "run".to_owned(), handle),
        )
    }

    /// A job whose task never finishes, standing in for a running command.
    fn running_job(session: &str) -> (Uuid, Job) {
        let job_id = Uuid::new_v4();
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok("unreachable".to_owned())
        });
        (
            job_id,
            Job::new(session.to_owned(), "run".to_owned(), handle),
        )
    }

    /// Admit and retain `count` completed jobs for one session.
    async fn fill(registry: &mut JobRegistry, session: &str, count: usize) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for _ in 0..count {
            let (job_id, job) = completed_job(session);
            registry
                .can_admit(at(0), session)
                .expect("capacity available");
            registry.insert(at(0), job_id, job);
            registry.wait_until_finished(job_id).await;
            ids.push(job_id);
        }
        ids
    }

    #[tokio::test]
    async fn a_per_session_ceiling_rejects_a_new_job() {
        let mut registry = JobRegistry::default();
        fill(&mut registry, "session-a", MAX_BACKGROUND_JOBS_PER_SESSION).await;
        assert!(
            registry.can_admit(at(0), "session-a").is_err(),
            "a session at its ceiling must not admit another job"
        );
    }

    #[tokio::test]
    async fn the_per_session_error_names_the_session_ceiling() {
        let mut registry = JobRegistry::default();
        fill(&mut registry, "session-a", MAX_BACKGROUND_JOBS_PER_SESSION).await;
        let error = registry
            .can_admit(at(0), "session-a")
            .expect_err("at the ceiling");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("background jobs for this session"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("overall"),
            "a per-session breach must not be reported as the global ceiling"
        );
    }

    #[tokio::test]
    async fn one_session_cannot_consume_the_whole_registry() {
        let mut registry = JobRegistry::default();
        // Each Session stays inside its own ceiling; together they reach the
        // global ceiling.
        let mut total = 0;
        let mut session = 0;
        while total < MAX_BACKGROUND_JOBS_GLOBAL {
            let id = format!("session-{session}");
            session += 1;
            for _ in 0..MAX_BACKGROUND_JOBS_PER_SESSION {
                if total >= MAX_BACKGROUND_JOBS_GLOBAL || registry.can_admit(at(0), &id).is_err() {
                    break;
                }
                let (job_id, job) = completed_job(&id);
                registry.insert(at(0), job_id, job);
                registry.wait_until_finished(job_id).await;
                total += 1;
            }
            assert!(session <= MAX_BACKGROUND_JOBS_GLOBAL, "fill must terminate");
        }
        assert_eq!(registry.snapshot_len(), MAX_BACKGROUND_JOBS_GLOBAL);
        let error = registry
            .can_admit(at(0), "session-brand-new")
            .expect_err("the global ceiling must hold across sessions");
        assert!(format!("{error:#}").contains("background jobs overall"));
    }

    #[tokio::test]
    async fn the_global_ceiling_is_never_exceeded_across_many_sessions() {
        let mut registry = JobRegistry::default();
        let mut admitted = 0;
        for index in 0..(MAX_BACKGROUND_JOBS_GLOBAL + 16) {
            let session = format!("session-{index}");
            if registry.can_admit(at(0), &session).is_ok() {
                let (job_id, job) = running_job(&session);
                registry.insert(at(0), job_id, job);
                admitted += 1;
            }
        }
        assert_eq!(admitted, MAX_BACKGROUND_JOBS_GLOBAL);
        assert_eq!(registry.snapshot_len(), MAX_BACKGROUND_JOBS_GLOBAL);
    }

    #[tokio::test]
    async fn one_session_cannot_evict_another_sessions_running_job() {
        let mut registry = JobRegistry::default();
        let (running_id, running) = running_job("session-a");
        registry.insert(at(0), running_id, running);

        // Session B fills its own ceiling and hits the global ceiling.
        for index in 0..MAX_BACKGROUND_JOBS_GLOBAL {
            let session = format!("session-b-{index}");
            if registry.can_admit(at(0), &session).is_ok() {
                let (job_id, job) = running_job(&session);
                registry.insert(at(0), job_id, job);
            }
        }
        assert_eq!(registry.snapshot_len(), MAX_BACKGROUND_JOBS_GLOBAL);

        // Session A's job was never displaced to make room.
        assert!(
            registry
                .take_if_finished(at(0), running_id, "session-a")
                .is_ok(),
            "an unrelated session's running job must not be evicted"
        );
    }

    #[tokio::test]
    async fn a_running_job_is_never_expired_by_the_finished_result_ttl() {
        let mut registry = JobRegistry::default();
        let (job_id, job) = running_job("session-a");
        registry.insert(at(0), job_id, job);
        // Far beyond the TTL.
        let much_later = at(FINISHED_JOB_TTL.as_secs() * 100);
        registry
            .can_admit(much_later, "session-a")
            .expect("still admissible");
        assert_eq!(
            registry.snapshot_len(),
            1,
            "GC must never kill a still-running job because its entry is old"
        );
        assert!(
            registry
                .take_if_finished(much_later, job_id, "session-a")
                .unwrap()
                .is_none(),
            "a running job is still reported as running after the TTL"
        );
    }

    #[tokio::test]
    async fn a_recently_finished_result_is_still_pollable() {
        let mut registry = JobRegistry::default();
        let ids = fill(&mut registry, "session-a", 1).await;
        let job_id = ids[0];
        // Just inside the retention period.
        let just_inside = at(FINISHED_JOB_TTL.as_secs() - 1);
        registry
            .can_admit(just_inside, "session-a")
            .expect("capacity available");
        let taken = registry
            .take_if_finished(just_inside, job_id, "session-a")
            .expect("the job is still registered");
        assert!(
            taken.is_some(),
            "a result inside its TTL must remain pollable"
        );
    }

    #[tokio::test]
    async fn a_finished_result_is_evicted_once_its_ttl_elapses() {
        let mut registry = JobRegistry::default();
        fill(&mut registry, "session-a", 1).await;

        // First GC after completion stamps `finished_at`.
        registry
            .can_admit(at(0), "session-a")
            .expect("capacity available");
        assert_eq!(
            registry.snapshot_len(),
            1,
            "the result is retained at first"
        );

        let expired = at(FINISHED_JOB_TTL.as_secs());
        registry
            .can_admit(expired, "session-b")
            .expect("capacity available");
        assert_eq!(
            registry.snapshot_len(),
            0,
            "an unpolled finished result must not be retained forever"
        );
    }

    #[tokio::test]
    async fn polling_an_expired_result_reports_it_as_unknown_or_expired() {
        let mut registry = JobRegistry::default();
        let ids = fill(&mut registry, "session-a", 1).await;
        let job_id = ids[0];
        // The first GC after completion observes the finish and starts the clock.
        registry
            .can_admit(at(0), "session-b")
            .expect("capacity available");
        let expired = at(FINISHED_JOB_TTL.as_secs());
        registry
            .can_admit(expired, "session-b")
            .expect("capacity available");

        let error = registry
            .take_if_finished(expired, job_id, "session-a")
            .expect_err("an expired result is no longer registered");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("expired"),
            "unexpected message: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_new_job_is_admitted_once_gc_frees_capacity() {
        let mut registry = JobRegistry::default();
        let session = "session-a";
        let ids = fill(&mut registry, session, MAX_BACKGROUND_JOBS_PER_SESSION).await;
        assert!(registry.can_admit(at(0), session).is_err());

        let expired = at(FINISHED_JOB_TTL.as_secs());
        registry
            .can_admit(expired, session)
            .expect("GC freed the capacity");
        assert!(
            registry.can_admit(expired, session).is_ok(),
            "a new job must be admitted after GC frees capacity"
        );
        assert_eq!(registry.snapshot_len(), 0);
        assert_eq!(ids.len(), MAX_BACKGROUND_JOBS_PER_SESSION);
    }

    #[tokio::test]
    async fn an_explicit_stop_removes_exactly_the_named_job() {
        let mut registry = JobRegistry::default();
        let ids = fill(&mut registry, "session-a", 3).await;
        let target = ids[1];
        let removed = registry
            .take(at(0), target, "session-a")
            .expect("the named job is removable");
        assert_eq!(removed.session_id(), "session-a");
        assert_eq!(registry.snapshot_len(), 2);
        for other in [ids[0], ids[2]] {
            assert!(
                registry
                    .take_if_finished(at(0), other, "session-a")
                    .unwrap()
                    .is_some(),
                "stopping one job must not remove its siblings"
            );
        }
    }

    #[tokio::test]
    async fn a_session_cannot_reach_another_sessions_job() {
        let mut registry = JobRegistry::default();
        let (job_id, job) = running_job("session-a");
        registry.insert(at(0), job_id, job);

        for op in [
            registry.take(at(0), job_id, "session-b").map(|_| ()),
            registry
                .take_if_finished(at(0), job_id, "session-b")
                .map(|_| ()),
        ] {
            let error = op.expect_err("cross-session access must be refused");
            assert!(format!("{error:#}").contains("does not belong"));
        }
        assert_eq!(
            registry.snapshot_len(),
            1,
            "a refused lookup must not disturb the job"
        );
    }

    #[tokio::test]
    async fn releasing_a_session_removes_only_that_sessions_jobs() {
        let mut registry = JobRegistry::default();
        let (a_id, a_job) = running_job("session-a");
        registry.insert(at(0), a_id, a_job);
        let (b_id, b_job) = running_job("session-b");
        registry.insert(at(0), b_id, b_job);

        let released = registry.release_session("session-a");
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].session_id(), "session-a");
        assert_eq!(
            registry.snapshot_len(),
            1,
            "session release must not touch another session's jobs"
        );
        assert!(
            registry.take_if_finished(at(0), b_id, "session-b").is_ok(),
            "the other session's job must survive"
        );
    }

    #[tokio::test]
    async fn releasing_a_session_stops_its_running_jobs() {
        let mut registry = JobRegistry::default();
        let (job_id, job) = running_job("session-a");
        registry.insert(at(0), job_id, job);
        for job in registry.release_session("session-a") {
            job.terminate().await;
        }
        assert_eq!(registry.snapshot_len(), 0);
    }

    #[tokio::test]
    async fn releasing_everything_empties_the_registry() {
        let mut registry = JobRegistry::default();
        fill(&mut registry, "session-a", 2).await;
        let (job_id, job) = running_job("session-b");
        registry.insert(at(0), job_id, job);
        assert_eq!(registry.snapshot_len(), 3);
        let released = registry.release_all();
        assert_eq!(released.len(), 3);
        assert_eq!(registry.snapshot_len(), 0);
    }

    #[tokio::test]
    async fn every_completion_path_eventually_releases_its_entry() {
        // Table over the completion paths: success, non-zero exit, spawn
        // failure, explicit stop, and session release all end with the registry
        // holding nothing for that job.
        let mut registry = JobRegistry::default();

        let (success_id, success) = completed_job("session-a");
        registry.insert(at(0), success_id, success);

        let (failure_id, failure) = (
            Uuid::new_v4(),
            Job::new(
                "session-a".to_owned(),
                "run".to_owned(),
                tokio::spawn(async { Err(anyhow::anyhow!("spawn failure")) }),
            ),
        );
        registry.insert(at(0), failure_id, failure);

        let (stopped_id, stopped) = running_job("session-a");
        registry.insert(at(0), stopped_id, stopped);
        assert_eq!(registry.snapshot_len(), 3);

        registry.wait_until_finished(success_id).await;
        registry.wait_until_finished(failure_id).await;

        // A finished job is released by polling.
        assert!(
            registry
                .take_if_finished(at(0), success_id, "session-a")
                .unwrap()
                .is_some()
        );
        // A failed job is released the same way, and carries its failure.
        let failed = registry
            .take_if_finished(at(0), failure_id, "session-a")
            .unwrap()
            .expect("failed job is pollable");
        assert!(failed.join().await.is_err());

        // A running job is released by an explicit stop.
        registry
            .take(at(0), stopped_id, "session-a")
            .expect("stoppable")
            .terminate()
            .await;
        assert_eq!(registry.snapshot_len(), 0);
    }

    #[tokio::test]
    async fn a_poll_never_destroys_a_running_job() {
        let mut registry = JobRegistry::default();
        let (job_id, _job) = running_job("session-a");
        registry.insert(at(0), job_id, _job);
        for _ in 0..5 {
            assert!(
                registry
                    .take_if_finished(at(0), job_id, "session-a")
                    .unwrap()
                    .is_none(),
                "polling a running job must leave it registered"
            );
        }
        assert_eq!(registry.snapshot_len(), 1);
        registry
            .take(at(0), job_id, "session-a")
            .expect("stoppable")
            .terminate()
            .await;
    }

    #[tokio::test]
    async fn many_jobs_never_grow_the_registry_past_its_cap() {
        // GC scans the registry, which is itself capped, so neither a GC pass nor
        // the registry can grow with history however many jobs pass through.
        let mut registry = JobRegistry::default();
        let mut round = 0_u64;
        for _ in 0..(MAX_BACKGROUND_JOBS_GLOBAL * 4) {
            round += 1;
            let now = at(round);
            if registry.can_admit(now, "session-a").is_ok() {
                let (job_id, job) = completed_job("session-a");
                registry.insert(now, job_id, job);
                registry.wait_until_finished(job_id).await;
            }
            assert!(
                registry.snapshot_len() <= MAX_BACKGROUND_JOBS_GLOBAL,
                "the registry exceeded its cap at round {round}"
            );
        }
        // Once every retained result is past its TTL, nothing is left behind.
        let after = at(round + FINISHED_JOB_TTL.as_secs() + 1);
        registry.can_admit(after, "session-a").expect("capacity");
        assert_eq!(registry.snapshot_len(), 0);
    }
}
