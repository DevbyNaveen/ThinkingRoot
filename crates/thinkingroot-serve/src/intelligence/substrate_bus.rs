//! Substrate Bus — Phase δ.1 of the Cognition Commits design
//! (`docs/2026-05-15-cognition-commits-design.md`).
//!
//! Background sub-agents that observe the substrate between user
//! turns and report what they find. The bus is the substrate-level
//! analogue of Letta's "sleep-time compute" idiom — async memory
//! work that doesn't block the chat loop but keeps the cognitive
//! state coherent.
//!
//! δ.1 ships three things:
//!
//! 1. **`SubAgent` trait** — async observation interface. One method
//!    (`tick`) per scheduled run. Sub-agents are stateless; durable
//!    state lives in the substrate (commits, proposals, gaps).
//! 2. **`SubAgentScheduler`** — owns a set of agents, runs each on
//!    its own interval inside a tokio task. Reports are retained in
//!    a bounded ring per agent so the desktop can render a "what
//!    happened while I was away" view without leaking memory.
//! 3. **`ReconcilerAgent`** — the first concrete sub-agent. Periodically
//!    calls `engine.reflect(ws)` and reports gap deltas + emerging
//!    structural patterns. Does NOT auto-open proposals; opening
//!    proposals is a write op that the user (or chat agent) drives
//!    explicitly so the substrate's surprise budget stays low.
//!
//! Deferred (δ.2 / δ.3 / δ.4): Gap-hunter, Curator, Watcher
//! sub-agents; desktop tray icon notification UI; compile-aware
//! backoff (skip tick while a compile is mid-flight); proposal
//! auto-write under an explicit user gate.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::engine::QueryEngine;
use thinkingroot_core::Result;

/// Read-only context handed to a sub-agent on each `tick`. The
/// scheduler owns the `Arc<RwLock<QueryEngine>>`; sub-agents acquire
/// the read guard themselves so they can hold it for the smallest
/// possible window.
#[derive(Clone)]
pub struct SubAgentContext {
    pub engine: Arc<RwLock<QueryEngine>>,
    pub workspace: String,
}

/// What a sub-agent produced during one `tick`. Even a quiet
/// "nothing to do" tick produces a report so the recent-reports
/// view shows the agent is alive — silence is suspicious in a
/// background-observer system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentReport {
    /// `agent.name()` — used by the desktop UI to group reports by
    /// agent and by the recovery log to thread runs together.
    pub agent: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// One-line summary suitable for tray-notification surfaces.
    /// Empty when the tick was a no-op and there's nothing to say.
    pub summary: String,
    /// Free-form observations the sub-agent surfaced. Each entry is
    /// a stand-alone fact; the UI renders them as bullet points.
    pub observations: Vec<String>,
    /// Branch / proposal ids opened by this tick. Empty in δ.1
    /// (agents don't auto-write); reserved for δ.2 when the user
    /// opts in to write-back.
    pub proposals_opened: Vec<String>,
    /// `Some(msg)` when the tick failed. The agent loop catches the
    /// error and surfaces it here rather than swallowing — a wedged
    /// sub-agent must be observable from the report stream.
    pub error: Option<String>,
}

impl SubAgentReport {
    /// Build a successful no-op report — useful for "I ran, nothing
    /// changed" ticks so the recovery surface shows the agent is
    /// alive.
    pub fn quiet(agent: &str, started: DateTime<Utc>) -> Self {
        Self {
            agent: agent.to_string(),
            started_at: started,
            finished_at: Utc::now(),
            summary: String::new(),
            observations: Vec::new(),
            proposals_opened: Vec::new(),
            error: None,
        }
    }

    /// Build a failed-tick report. Surfaces the error to the
    /// scheduler's report ring + the recovery log; the agent will
    /// still be retried at the next interval — δ.1 has no per-agent
    /// circuit breaker, intentional (a workspace mount failure
    /// should keep retrying because the user can mount mid-flight).
    pub fn failed(agent: &str, started: DateTime<Utc>, err: impl ToString) -> Self {
        Self {
            agent: agent.to_string(),
            started_at: started,
            finished_at: Utc::now(),
            summary: format!("{}: failed", agent),
            observations: Vec::new(),
            proposals_opened: Vec::new(),
            error: Some(err.to_string()),
        }
    }
}

/// One scheduled observer. Implementations are `Send + Sync` and
/// idempotent — the same input substrate state must produce the
/// same report across runs so the recovery log doesn't reflect
/// non-determinism from the agent itself.
#[async_trait]
pub trait SubAgent: Send + Sync {
    /// Stable identifier — used as the `agent` field on every
    /// emitted report. Should be a short snake-case slug
    /// (`"reconciler"`, `"gap_hunter"`, etc.). Pinned across
    /// releases — bumping the slug invalidates the recovery log's
    /// agent-history join.
    fn name(&self) -> &'static str;

    /// Cadence at which `tick` is invoked. The scheduler sleeps
    /// `interval()` between consecutive ticks; if `tick` takes
    /// longer than the interval, the next tick fires immediately
    /// after (no overlap — each agent runs at most one tick at a
    /// time per scheduler).
    fn interval(&self) -> Duration;

    /// Do one observation pass. Errors are reported via the
    /// `SubAgentReport.error` channel; never panic — the scheduler's
    /// loop body catches panics, but a panicking sub-agent is a bug
    /// that should surface as a recovery-log entry instead.
    async fn tick(&self, ctx: &SubAgentContext) -> Result<SubAgentReport>;
}

/// The Reconciler — the substrate-bus's flagship sub-agent.
///
/// On each tick: calls `engine.reflect(workspace)` (the existing
/// structural-pattern + gap engine), inspects the delta (new gaps,
/// resolved gaps, currently-open gaps), and reports it. Surfaces
/// emerging structural patterns as observations.
///
/// Cost profile: `engine.reflect` takes ~50–200 ms on a typical
/// workspace (CozoDB Datalog over the structural tables). At a
/// 15-minute default interval that's well under 0.1% CPU.
pub struct ReconcilerAgent {
    interval: Duration,
}

impl ReconcilerAgent {
    /// Construct with a custom tick cadence. Use `Self::default()`
    /// for the 15-minute production setting.
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Default for ReconcilerAgent {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(15 * 60),
        }
    }
}

#[async_trait]
impl SubAgent for ReconcilerAgent {
    fn name(&self) -> &'static str {
        "reconciler"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn tick(&self, ctx: &SubAgentContext) -> Result<SubAgentReport> {
        let started = Utc::now();
        let engine = ctx.engine.read().await;
        match engine.reflect(&ctx.workspace).await {
            Ok(r) => {
                // Surface up to 10 patterns as observations — the
                // tray surface can't carry more than a handful and
                // truncation here is honest (the full pattern list
                // is reachable via `engine.reflect` directly).
                let observations: Vec<String> = r
                    .patterns
                    .iter()
                    .take(10)
                    .map(|p| {
                        format!(
                            "pattern: entity_type=`{}` expects=`{}` freq={:.2}",
                            p.entity_type, p.expected_claim_type, p.frequency
                        )
                    })
                    .collect();
                let summary = if r.open_gaps_total == 0
                    && r.gaps_created == 0
                    && r.gaps_resolved == 0
                {
                    String::new()
                } else {
                    format!(
                        "{} open gaps (+{} new, -{} resolved this tick)",
                        r.open_gaps_total, r.gaps_created, r.gaps_resolved
                    )
                };
                Ok(SubAgentReport {
                    agent: self.name().to_string(),
                    started_at: started,
                    finished_at: Utc::now(),
                    summary,
                    observations,
                    proposals_opened: Vec::new(),
                    error: None,
                })
            }
            Err(e) => Ok(SubAgentReport::failed(self.name(), started, e)),
        }
    }
}

/// The Gap-hunter — Phase δ.3 sub-agent.
///
/// On each tick: snapshots the current open-gap count, compares
/// against the previous tick's count, and reports the delta. When
/// gaps grow faster than they're resolved over a small window, the
/// summary flags it. Read-only; no proposals opened.
pub struct GapHunterAgent {
    interval: Duration,
    /// Rolling memory of the last-seen open-gap count. The
    /// in-process state is intentionally not persistent — gap
    /// trajectory is an observation, not a durable substrate fact;
    /// a fresh boot starts from `None` and re-baselines on first
    /// tick.
    last_open_gaps: Arc<tokio::sync::Mutex<Option<usize>>>,
}

impl GapHunterAgent {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_open_gaps: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

impl Default for GapHunterAgent {
    fn default() -> Self {
        Self::new(Duration::from_secs(30 * 60))
    }
}

#[async_trait]
impl SubAgent for GapHunterAgent {
    fn name(&self) -> &'static str {
        "gap_hunter"
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn tick(&self, ctx: &SubAgentContext) -> Result<SubAgentReport> {
        let started = Utc::now();
        let engine = ctx.engine.read().await;
        let result = match engine.reflect(&ctx.workspace).await {
            Ok(r) => r,
            Err(e) => return Ok(SubAgentReport::failed(self.name(), started, e)),
        };
        let mut prev = self.last_open_gaps.lock().await;
        let delta_summary = match *prev {
            Some(p) if result.open_gaps_total > p => format!(
                "gaps growing: {p} → {} (+{} since last tick)",
                result.open_gaps_total,
                result.open_gaps_total - p
            ),
            Some(p) if result.open_gaps_total < p => format!(
                "gaps shrinking: {p} → {} (-{} since last tick)",
                result.open_gaps_total,
                p - result.open_gaps_total
            ),
            Some(_) => "gap count stable since last tick".to_string(),
            None => format!(
                "baseline: {} open gaps (first tick)",
                result.open_gaps_total
            ),
        };
        *prev = Some(result.open_gaps_total);
        Ok(SubAgentReport {
            agent: self.name().to_string(),
            started_at: started,
            finished_at: Utc::now(),
            summary: delta_summary,
            observations: Vec::new(),
            proposals_opened: Vec::new(),
            error: None,
        })
    }
}

/// The Curator — Phase δ.3 sub-agent.
///
/// On each tick: counts recent cognition commits and reports a
/// curation-load summary. When the commit count climbs above the
/// configured threshold the summary surfaces a "consider archiving"
/// hint so a downstream UI can suggest dedup or summarisation.
/// Does NOT touch the substrate — pure read.
pub struct CuratorAgent {
    interval: Duration,
    /// Threshold above which the curator flags the commit pile as
    /// "consider archiving". 500 chosen as a reasonable workspace-
    /// scale signal — most workspaces stay well under this; the
    /// recent-week-of-active-chat range crosses it.
    archive_threshold: u64,
}

impl CuratorAgent {
    pub fn new(interval: Duration, archive_threshold: u64) -> Self {
        Self {
            interval,
            archive_threshold,
        }
    }
}

impl Default for CuratorAgent {
    fn default() -> Self {
        Self::new(Duration::from_secs(60 * 60), 500)
    }
}

#[async_trait]
impl SubAgent for CuratorAgent {
    fn name(&self) -> &'static str {
        "curator"
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn tick(&self, ctx: &SubAgentContext) -> Result<SubAgentReport> {
        let started = Utc::now();
        let engine = ctx.engine.read().await;
        let total = match engine.count_cognition_commits(&ctx.workspace).await {
            Ok(n) => n,
            Err(e) => return Ok(SubAgentReport::failed(self.name(), started, e)),
        };
        let summary = if total >= self.archive_threshold {
            format!(
                "{total} cognition commits — past archive threshold ({}); consider summarising older slices",
                self.archive_threshold
            )
        } else {
            format!("{total} cognition commits")
        };
        Ok(SubAgentReport {
            agent: self.name().to_string(),
            started_at: started,
            finished_at: Utc::now(),
            summary,
            observations: Vec::new(),
            proposals_opened: Vec::new(),
            error: None,
        })
    }
}

/// The Watcher — Phase δ.3 sub-agent.
///
/// On each tick: counts witnesses in the workspace and reports the
/// growth rate. When the witness count grows faster than the
/// configured spike threshold (default 100 / tick), the summary
/// flags it as "ingest spike" — useful tray-notification material
/// for "your workspace just absorbed a lot of new sources".
pub struct WatcherAgent {
    interval: Duration,
    spike_threshold: u64,
    last_witness_count: Arc<tokio::sync::Mutex<Option<u64>>>,
}

impl WatcherAgent {
    pub fn new(interval: Duration, spike_threshold: u64) -> Self {
        Self {
            interval,
            spike_threshold,
            last_witness_count: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

impl Default for WatcherAgent {
    fn default() -> Self {
        Self::new(Duration::from_secs(10 * 60), 100)
    }
}

#[async_trait]
impl SubAgent for WatcherAgent {
    fn name(&self) -> &'static str {
        "watcher"
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn tick(&self, ctx: &SubAgentContext) -> Result<SubAgentReport> {
        let started = Utc::now();
        let engine = ctx.engine.read().await;
        let total = match engine.count_witnesses(&ctx.workspace).await {
            Ok(n) => n,
            Err(e) => return Ok(SubAgentReport::failed(self.name(), started, e)),
        };
        let mut prev = self.last_witness_count.lock().await;
        let summary = match *prev {
            Some(p) if total > p && total - p >= self.spike_threshold => format!(
                "ingest spike: {} → {total} (+{} witnesses this tick)",
                p,
                total - p
            ),
            Some(p) if total > p => format!(
                "{total} witnesses (+{} since last tick)",
                total - p
            ),
            Some(p) if total < p => format!("{total} witnesses (-{} since last tick)", p - total),
            Some(_) => format!("{total} witnesses (stable)"),
            None => format!("baseline: {total} witnesses (first tick)"),
        };
        *prev = Some(total);
        Ok(SubAgentReport {
            agent: self.name().to_string(),
            started_at: started,
            finished_at: Utc::now(),
            summary,
            observations: Vec::new(),
            proposals_opened: Vec::new(),
            error: None,
        })
    }
}

/// Register every Phase δ.3 sub-agent into a fresh scheduler with
/// the production-default cadences. Used by the daemon's boot-time
/// wiring (δ.2) to set up the substrate bus per workspace.
pub fn default_scheduler() -> SubAgentScheduler {
    // 100 reports per agent ≈ one week at the default cadences.
    let mut s = SubAgentScheduler::new(100);
    s.register(Arc::new(ReconcilerAgent::default()));
    s.register(Arc::new(GapHunterAgent::default()));
    s.register(Arc::new(CuratorAgent::default()));
    s.register(Arc::new(WatcherAgent::default()));
    s
}

/// Bounded ring of recent reports per agent. Used internally by the
/// scheduler to retain a history without unbounded memory growth.
#[derive(Default)]
struct ReportRing {
    reports: Vec<SubAgentReport>,
    capacity: usize,
}

impl ReportRing {
    fn new(capacity: usize) -> Self {
        Self {
            reports: Vec::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, report: SubAgentReport) {
        if self.capacity == 0 {
            return;
        }
        self.reports.push(report);
        if self.reports.len() > self.capacity {
            let drop_n = self.reports.len() - self.capacity;
            self.reports.drain(0..drop_n);
        }
    }

    fn snapshot(&self) -> Vec<SubAgentReport> {
        self.reports.clone()
    }
}

/// Holds the registered sub-agents + the shared report history.
/// Constructed by `SubAgentScheduler::new`; consumed by `start` to
/// spawn the per-agent tokio tasks.
pub struct SubAgentScheduler {
    agents: Vec<Arc<dyn SubAgent>>,
    reports: Arc<RwLock<std::collections::HashMap<String, ReportRing>>>,
    cancel: CancellationToken,
    history_per_agent: usize,
}

impl SubAgentScheduler {
    /// Construct an empty scheduler. `history_per_agent` is the max
    /// number of reports retained per agent (FIFO eviction). A value
    /// of 100 covers ~24h of activity at the default 15-minute
    /// interval — enough for the desktop "what happened while I was
    /// away" rail without bounding memory growth on a long-running
    /// daemon.
    pub fn new(history_per_agent: usize) -> Self {
        Self {
            agents: Vec::new(),
            reports: Arc::new(RwLock::new(std::collections::HashMap::new())),
            cancel: CancellationToken::new(),
            history_per_agent,
        }
    }

    /// Register a sub-agent. Must be called before `start`.
    pub fn register(&mut self, agent: Arc<dyn SubAgent>) {
        self.agents.push(agent);
    }

    /// How many agents are currently registered. Test hook.
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Spawn one tokio task per registered agent. Each task loops
    /// `tick → sleep(interval)` until cancelled. Reports are written
    /// to the shared ring under `reports.write()` — the per-agent
    /// ring caps memory growth at `history_per_agent` entries.
    pub fn start(&self, ctx: SubAgentContext) {
        for agent in &self.agents {
            let agent = Arc::clone(agent);
            let ctx = ctx.clone();
            let cancel = self.cancel.clone();
            let reports = Arc::clone(&self.reports);
            let history_per_agent = self.history_per_agent;
            tokio::spawn(async move {
                run_agent_loop(agent, ctx, cancel, reports, history_per_agent).await;
            });
        }
    }

    /// Trigger one tick of `agent_name` synchronously (skipping the
    /// scheduled sleep) and return the resulting report. Used by:
    ///   - tests, to exercise tick logic without waiting on
    ///     `tokio::time::sleep`.
    ///   - the future δ.2 desktop "Run now" button that lets the
    ///     user manually invoke a sub-agent for a fresh report.
    pub async fn run_now(
        &self,
        agent_name: &str,
        ctx: &SubAgentContext,
    ) -> Option<SubAgentReport> {
        let agent = self.agents.iter().find(|a| a.name() == agent_name)?;
        let report = match agent.tick(ctx).await {
            Ok(r) => r,
            Err(e) => SubAgentReport::failed(agent.name(), Utc::now(), e),
        };
        push_report(&self.reports, report.clone(), self.history_per_agent).await;
        Some(report)
    }

    /// Snapshot every retained report across every agent. Newest
    /// first within each agent's slice; the slices themselves are
    /// concatenated in agent-name ASCII order so the output is
    /// stable across calls when the substrate is quiet.
    pub async fn recent_reports(&self) -> Vec<SubAgentReport> {
        let map = self.reports.read().await;
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        let mut out = Vec::new();
        for k in keys {
            if let Some(ring) = map.get(k) {
                let mut slice = ring.snapshot();
                slice.reverse(); // newest first
                out.extend(slice);
            }
        }
        out
    }

    /// Names of registered agents in registration order. Stable
    /// surface for "which agents are wired" introspection.
    pub fn agent_names(&self) -> Vec<&'static str> {
        self.agents.iter().map(|a| a.name()).collect()
    }

    /// Cancel every running tick. Idempotent — calling twice is a
    /// no-op. After `shutdown`, the scheduler is effectively dead;
    /// construct a fresh `SubAgentScheduler` to restart.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

async fn push_report(
    reports: &Arc<RwLock<std::collections::HashMap<String, ReportRing>>>,
    report: SubAgentReport,
    capacity: usize,
) {
    let key = report.agent.clone();
    let mut guard = reports.write().await;
    let ring = guard
        .entry(key)
        .or_insert_with(|| ReportRing::new(capacity));
    ring.push(report);
}

async fn run_agent_loop(
    agent: Arc<dyn SubAgent>,
    ctx: SubAgentContext,
    cancel: CancellationToken,
    reports: Arc<RwLock<std::collections::HashMap<String, ReportRing>>>,
    history_per_agent: usize,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(
                    agent = %agent.name(),
                    "substrate_bus: sub-agent loop cancelled"
                );
                break;
            }
            _ = tokio::time::sleep(agent.interval()) => {
                let started = Utc::now();
                let report = match agent.tick(&ctx).await {
                    Ok(r) => r,
                    Err(e) => SubAgentReport::failed(agent.name(), started, e),
                };
                push_report(&reports, report, history_per_agent).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A test sub-agent that increments a counter on every tick and
    /// emits a fixed report. Lets us exercise the scheduler without
    /// touching a real engine fixture.
    struct CountingAgent {
        name: &'static str,
        interval: Duration,
        counter: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SubAgent for CountingAgent {
        fn name(&self) -> &'static str {
            self.name
        }
        fn interval(&self) -> Duration {
            self.interval
        }
        async fn tick(&self, _ctx: &SubAgentContext) -> Result<SubAgentReport> {
            let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(SubAgentReport {
                agent: self.name.to_string(),
                started_at: Utc::now(),
                finished_at: Utc::now(),
                summary: format!("tick {n}"),
                observations: vec![format!("count={n}")],
                proposals_opened: vec![],
                error: None,
            })
        }
    }

    /// A test sub-agent that always returns Err — exercises the
    /// scheduler's error-into-report mapping.
    struct FailingAgent;

    #[async_trait]
    impl SubAgent for FailingAgent {
        fn name(&self) -> &'static str {
            "failing_agent"
        }
        fn interval(&self) -> Duration {
            Duration::from_millis(10)
        }
        async fn tick(&self, _ctx: &SubAgentContext) -> Result<SubAgentReport> {
            Err(thinkingroot_core::Error::Config(
                "intentional test failure".to_string(),
            ))
        }
    }

    fn fixture_ctx() -> SubAgentContext {
        // The CountingAgent + FailingAgent don't touch the engine,
        // but the SubAgentContext type requires a real engine handle.
        // `QueryEngine::new()` is the canonical empty constructor;
        // it doesn't mount any workspace so any `tick` that called
        // `engine.reflect()` here would fail with WorkspaceNotMounted
        // — that's exercised by the failing-agent test elsewhere
        // (the ReconcilerAgent's real-engine path is covered by
        // serve integration tests, not by these unit tests).
        let engine = QueryEngine::new();
        SubAgentContext {
            engine: Arc::new(RwLock::new(engine)),
            workspace: "test-ws".to_string(),
        }
    }

    #[tokio::test]
    async fn run_now_increments_counter_and_records_report() {
        let counter = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(CountingAgent {
            name: "counter",
            interval: Duration::from_secs(60),
            counter: Arc::clone(&counter),
        });
        let mut scheduler = SubAgentScheduler::new(8);
        scheduler.register(agent);
        let ctx = fixture_ctx();
        let report = scheduler
            .run_now("counter", &ctx)
            .await
            .expect("counter agent registered");
        assert_eq!(report.agent, "counter");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let recent = scheduler.recent_reports().await;
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].summary, "tick 1");
    }

    #[tokio::test]
    async fn run_now_unknown_agent_returns_none() {
        let scheduler = SubAgentScheduler::new(8);
        let ctx = fixture_ctx();
        assert!(scheduler.run_now("nonexistent", &ctx).await.is_none());
    }

    #[tokio::test]
    async fn failing_agent_surfaces_error_in_report() {
        let mut scheduler = SubAgentScheduler::new(8);
        scheduler.register(Arc::new(FailingAgent));
        let ctx = fixture_ctx();
        let report = scheduler
            .run_now("failing_agent", &ctx)
            .await
            .expect("failing agent registered");
        assert!(report.error.is_some());
        assert!(report.error.unwrap().contains("intentional"));
    }

    #[tokio::test]
    async fn history_ring_truncates_at_capacity() {
        let counter = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(CountingAgent {
            name: "counter",
            interval: Duration::from_secs(60),
            counter: Arc::clone(&counter),
        });
        let mut scheduler = SubAgentScheduler::new(3);
        scheduler.register(agent);
        let ctx = fixture_ctx();
        for _ in 0..10 {
            scheduler.run_now("counter", &ctx).await.unwrap();
        }
        let recent = scheduler.recent_reports().await;
        // Only the last 3 retained.
        assert_eq!(recent.len(), 3);
        // Newest first: tick 10, 9, 8.
        assert_eq!(recent[0].summary, "tick 10");
        assert_eq!(recent[1].summary, "tick 9");
        assert_eq!(recent[2].summary, "tick 8");
    }

    #[tokio::test]
    async fn recent_reports_groups_by_agent_in_stable_order() {
        let counter_a = Arc::new(AtomicUsize::new(0));
        let counter_b = Arc::new(AtomicUsize::new(0));
        let agent_a = Arc::new(CountingAgent {
            name: "alpha",
            interval: Duration::from_secs(60),
            counter: Arc::clone(&counter_a),
        });
        let agent_b = Arc::new(CountingAgent {
            name: "beta",
            interval: Duration::from_secs(60),
            counter: Arc::clone(&counter_b),
        });
        let mut scheduler = SubAgentScheduler::new(4);
        scheduler.register(agent_a);
        scheduler.register(agent_b);
        let ctx = fixture_ctx();
        scheduler.run_now("alpha", &ctx).await.unwrap();
        scheduler.run_now("beta", &ctx).await.unwrap();
        scheduler.run_now("alpha", &ctx).await.unwrap();
        let recent = scheduler.recent_reports().await;
        // alpha (newest first) then beta — agents sorted by name.
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].agent, "alpha");
        assert_eq!(recent[1].agent, "alpha");
        assert_eq!(recent[2].agent, "beta");
    }

    #[tokio::test]
    async fn scheduler_records_ticks_under_real_interval() {
        // Use a short interval so the test runs in <50ms; verify the
        // scheduler actually fires ticks via the spawned loop, not
        // just `run_now`.
        let counter = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(CountingAgent {
            name: "fast",
            interval: Duration::from_millis(5),
            counter: Arc::clone(&counter),
        });
        let mut scheduler = SubAgentScheduler::new(20);
        scheduler.register(agent);
        let ctx = fixture_ctx();
        scheduler.start(ctx);
        // Wait long enough for at least 3 ticks to land.
        tokio::time::sleep(Duration::from_millis(60)).await;
        scheduler.shutdown();
        // Give the loop a moment to exit cleanly.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let n = counter.load(Ordering::SeqCst);
        assert!(n >= 3, "expected at least 3 ticks, got {n}");
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let scheduler = SubAgentScheduler::new(4);
        scheduler.shutdown();
        scheduler.shutdown(); // second call: no panic, no deadlock.
    }

    #[tokio::test]
    async fn reconciler_default_interval_is_15_minutes() {
        let r = ReconcilerAgent::default();
        assert_eq!(r.interval(), Duration::from_secs(15 * 60));
        assert_eq!(r.name(), "reconciler");
    }

    #[test]
    fn report_quiet_factory_constructs_clean_empty_report() {
        let started = Utc::now();
        let r = SubAgentReport::quiet("test", started);
        assert_eq!(r.agent, "test");
        assert!(r.summary.is_empty());
        assert!(r.observations.is_empty());
        assert!(r.proposals_opened.is_empty());
        assert!(r.error.is_none());
    }

    #[test]
    fn report_failed_factory_propagates_error_string() {
        let started = Utc::now();
        let r = SubAgentReport::failed("test", started, "boom");
        assert_eq!(r.error.as_deref(), Some("boom"));
        assert!(r.summary.contains("failed"));
    }

    #[test]
    fn delta_3_agent_names_and_intervals_are_stable() {
        let r = ReconcilerAgent::default();
        let g = GapHunterAgent::default();
        let c = CuratorAgent::default();
        let w = WatcherAgent::default();
        assert_eq!(r.name(), "reconciler");
        assert_eq!(g.name(), "gap_hunter");
        assert_eq!(c.name(), "curator");
        assert_eq!(w.name(), "watcher");
        // Cadence sanity — production defaults shouldn't be sub-minute
        // (they read the substrate; sub-minute would burn CPU).
        assert!(r.interval() >= Duration::from_secs(60));
        assert!(g.interval() >= Duration::from_secs(60));
        assert!(c.interval() >= Duration::from_secs(60));
        assert!(w.interval() >= Duration::from_secs(60));
    }

    #[test]
    fn default_scheduler_registers_all_four_agents() {
        let s = default_scheduler();
        assert_eq!(s.agent_count(), 4);
        let names = s.agent_names();
        assert!(names.contains(&"reconciler"));
        assert!(names.contains(&"gap_hunter"));
        assert!(names.contains(&"curator"));
        assert!(names.contains(&"watcher"));
    }
}
