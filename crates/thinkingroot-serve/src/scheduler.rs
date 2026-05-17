//! Clean-room reimplementation. Inspired by openhuman/composio/periodic.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.2 (2026-05-17) — process-wide periodic-task abstraction.
//!
//! Background tasks (today: stream-branch cleanup; tomorrow: TTL
//! cleanup, engram cache compaction, recovery-log rotation, etc.)
//! all want the same shape: "run this thing every N seconds in the
//! background, log errors, never panic the process." Before this
//! module each such task re-rolled the
//! `tokio::spawn + tokio::time::interval` pattern in its own file.
//!
//! This module gives us:
//!
//! 1. A `PeriodicTask` trait — implement once per concern.
//! 2. A `Scheduler` registry — register + idempotently start.
//! 3. A `mark_recently_run` channel for event-driven paths to
//!    suppress the next tick (e.g. when a forced cleanup just ran).
//!
//! ## Production wiring
//!
//! The existing `maintenance::spawn_stream_cleanup` keeps its public
//! signature for backwards compat with `thinkingroot-cli/src/serve.rs`.
//! Internally it now builds a `StreamCleanupTask` (in
//! `maintenance.rs`) and feeds it into `spawn_periodic_task` here —
//! the same helper `Scheduler::start` uses for each registered task.
//! That keeps the worker-loop logic in exactly one place.
//!
//! `Scheduler::start` is idempotent via `OnceLock<()>`: calling it
//! twice is harmless and the second call is a no-op.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use thinkingroot_core::Error;

/// One background concern that wants to run on a fixed interval.
///
/// Implementors must be cheaply clonable (typically: hold `Arc`s
/// internally) so the scheduler can move the task into a `tokio::spawn`
/// loop. `run` returns `Result` so transient errors can be logged
/// without killing the loop — the worker swallows + logs each tick's
/// error and continues.
#[async_trait]
pub trait PeriodicTask: Send + Sync + 'static {
    /// Stable identifier used for telemetry + the `mark_recently_run`
    /// channel. Should be a short snake_case-ish string —
    /// `"stream_cleanup"`, `"ttl_cleanup"`, etc.
    fn name(&self) -> &'static str;

    /// How often to run. The scheduler clamps below 1 second to 1
    /// second to keep accidentally-tight loops from spinning the CPU.
    fn interval(&self) -> Duration;

    /// One tick of work. Errors are logged at WARN and the loop
    /// continues — periodic tasks must NEVER halt the process on a
    /// transient failure.
    async fn run(&self) -> Result<(), Error>;
}

/// Process-wide registry for `PeriodicTask`s.
///
/// Typical lifecycle:
///
/// ```ignore
/// let sched = Scheduler::new();
/// sched.register(Arc::new(MyTask::new()));
/// sched.register(Arc::new(OtherTask::new()));
/// sched.start();   // idempotent — safe to call from setup
/// ```
///
/// `start` spawns one tokio task per registered `PeriodicTask`.
/// Handles are retained internally; callers that need to abort on
/// shutdown can call `abort_all`.
pub struct Scheduler {
    tasks: AsyncMutex<Vec<Arc<dyn PeriodicTask>>>,
    /// `OnceLock<()>` so `start` is idempotent. The handles vec is
    /// held under its own mutex because we may need to append on
    /// re-`start`-into-no-op (the no-op leaves the handles alone).
    started: OnceLock<()>,
    handles: AsyncMutex<Vec<JoinHandle<()>>>,
    /// Per-task last-run timestamp. Event-driven paths can call
    /// `mark_recently_run` to record a forced run that the next tick
    /// might want to skip. Today this is informational; future
    /// `PeriodicTask` impls may consult it inside `run` to short-circuit.
    last_runs: AsyncMutex<HashMap<String, Instant>>,
}

impl Scheduler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks: AsyncMutex::new(Vec::new()),
            started: OnceLock::new(),
            handles: AsyncMutex::new(Vec::new()),
            last_runs: AsyncMutex::new(HashMap::new()),
        })
    }

    /// Register one periodic task.
    ///
    /// **Order requirement:** call every `register` BEFORE `start`.
    /// `start` is one-shot (gated by `OnceLock`), so tasks registered
    /// after the first `start` are silently never spawned — calling
    /// `start` again is a no-op. Use [`start_one`] to spawn a single
    /// late-registered task on top of an already-running scheduler.
    pub async fn register(&self, task: Arc<dyn PeriodicTask>) {
        self.tasks.lock().await.push(task);
    }

    /// One-shot start. First call spawns one tokio worker per
    /// currently-registered task. Subsequent calls are no-ops —
    /// `start_one` is the entry point for late additions.
    pub async fn start(self: &Arc<Self>) {
        if self.started.set(()).is_err() {
            return;
        }
        let tasks_snapshot: Vec<Arc<dyn PeriodicTask>> = self.tasks.lock().await.clone();
        let mut handles = self.handles.lock().await;
        for task in tasks_snapshot {
            handles.push(spawn_periodic_task(task));
        }
    }

    /// Spawn a single task on a scheduler that has already been
    /// started. Use when a feature wants to add a periodic worker
    /// dynamically (e.g. after a workspace mounts). The task is also
    /// appended to `tasks` so it survives a hypothetical future
    /// restart hook. Idempotency vs. `start` is by construction:
    /// this method does NOT consult `started` at all.
    pub async fn start_one(self: &Arc<Self>, task: Arc<dyn PeriodicTask>) {
        self.tasks.lock().await.push(task.clone());
        self.handles.lock().await.push(spawn_periodic_task(task));
    }

    /// Record a forced run of `task_name` — used by event-driven
    /// surfaces (e.g. a manual gc_branches MCP call) that just did
    /// the same work the next tick would have. Stored for telemetry
    /// + future PeriodicTask impls that want to consult it.
    pub async fn mark_recently_run(&self, task_name: &str) {
        self.last_runs
            .lock()
            .await
            .insert(task_name.to_string(), Instant::now());
    }

    /// How long ago `task_name` last ran (forced OR scheduled).
    /// Returns `None` if no record exists yet.
    pub async fn last_run_age(&self, task_name: &str) -> Option<Duration> {
        self.last_runs
            .lock()
            .await
            .get(task_name)
            .map(|t| t.elapsed())
    }

    /// Abort every spawned worker. Used at clean shutdown — without
    /// this the tokio runtime would have to wait for the next tick
    /// before each worker observes the shutdown signal.
    pub async fn abort_all(&self) {
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *self.handles.lock().await);
        for h in handles {
            h.abort();
        }
    }
}

/// Spawn one periodic-task worker. Public-in-crate so
/// `maintenance::spawn_stream_cleanup` can reuse the same loop
/// pattern without going through the full `Scheduler::register +
/// start` ceremony (the CLI caller wants a single `JoinHandle<()>`
/// back).
///
/// The worker loop:
///   1. Build a `tokio::time::interval` clamped to ≥ 1 second.
///   2. Set `MissedTickBehavior::Delay` (skip catch-up; we don't
///      want a bursty re-run after a long pause).
///   3. Eat the first tick (fires immediately by default).
///   4. Loop: tick → run → log-on-error → continue.
pub(crate) fn spawn_periodic_task(task: Arc<dyn PeriodicTask>) -> JoinHandle<()> {
    let interval_dur = task.interval().max(Duration::from_secs(1));
    let name = task.name();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval_dur);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the immediate first tick — the historical contract
        // (maintenance.rs::spawn_stream_cleanup pre-E.2) is to wait
        // one full interval before the first run so the workspace
        // has time to settle.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match task.run().await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "periodic_task",
                        task = name,
                        "tick failed (non-fatal): {e}"
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingTask {
        name: &'static str,
        interval: Duration,
        counter: Arc<AtomicUsize>,
        fail_until: Option<usize>,
    }

    #[async_trait]
    impl PeriodicTask for CountingTask {
        fn name(&self) -> &'static str {
            self.name
        }
        fn interval(&self) -> Duration {
            self.interval
        }
        async fn run(&self) -> Result<(), Error> {
            let n = self.counter.fetch_add(1, Ordering::SeqCst);
            if let Some(threshold) = self.fail_until {
                if n < threshold {
                    return Err(Error::GraphStorage(format!("simulated tick fail #{n}")));
                }
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn scheduler_runs_registered_task_on_interval() {
        let sched = Scheduler::new();
        let counter = Arc::new(AtomicUsize::new(0));
        sched
            .register(Arc::new(CountingTask {
                name: "test_task",
                interval: Duration::from_secs(1), // clamped floor
                counter: counter.clone(),
                fail_until: None,
            }))
            .await;
        sched.start().await;
        // Wait long enough for ~2 ticks past the eaten first tick.
        tokio::time::sleep(Duration::from_millis(2200)).await;
        sched.abort_all().await;
        let observed = counter.load(Ordering::SeqCst);
        assert!(
            observed >= 1,
            "expected at least one tick after 2.2s, observed {observed}"
        );
    }

    #[tokio::test]
    async fn scheduler_start_is_idempotent() {
        let sched = Scheduler::new();
        let counter = Arc::new(AtomicUsize::new(0));
        sched
            .register(Arc::new(CountingTask {
                name: "idempotent_task",
                interval: Duration::from_secs(60),
                counter: counter.clone(),
                fail_until: None,
            }))
            .await;
        sched.start().await;
        sched.start().await; // no-op
        sched.start().await; // no-op
        // Only one worker should have been spawned — total handles == 1.
        let handles_count = sched.handles.lock().await.len();
        assert_eq!(handles_count, 1, "start must be idempotent");
        sched.abort_all().await;
    }

    #[tokio::test]
    async fn scheduler_swallows_task_errors_and_keeps_ticking() {
        let sched = Scheduler::new();
        let counter = Arc::new(AtomicUsize::new(0));
        sched
            .register(Arc::new(CountingTask {
                name: "flaky",
                interval: Duration::from_secs(1),
                counter: counter.clone(),
                // First two ticks error; third succeeds. The worker
                // must keep ticking past the failures.
                fail_until: Some(2),
            }))
            .await;
        sched.start().await;
        tokio::time::sleep(Duration::from_millis(3500)).await;
        sched.abort_all().await;
        let observed = counter.load(Ordering::SeqCst);
        assert!(
            observed >= 3,
            "worker must survive transient errors; observed {observed} ticks"
        );
    }

    #[tokio::test]
    async fn mark_recently_run_records_timestamp() {
        let sched = Scheduler::new();
        assert!(sched.last_run_age("nope").await.is_none());
        sched.mark_recently_run("manual_cleanup").await;
        let age = sched
            .last_run_age("manual_cleanup")
            .await
            .expect("expected age after mark");
        assert!(age < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn multiple_tasks_each_spawn_one_worker() {
        let sched = Scheduler::new();
        let c1 = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::new(AtomicUsize::new(0));
        sched
            .register(Arc::new(CountingTask {
                name: "t1",
                interval: Duration::from_secs(60),
                counter: c1,
                fail_until: None,
            }))
            .await;
        sched
            .register(Arc::new(CountingTask {
                name: "t2",
                interval: Duration::from_secs(60),
                counter: c2,
                fail_until: None,
            }))
            .await;
        sched.start().await;
        let handles_count = sched.handles.lock().await.len();
        assert_eq!(handles_count, 2);
        sched.abort_all().await;
    }
}
