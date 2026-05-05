// crates/thinkingroot-core/src/types/claim_migration.rs
//
// T3.6 — Schema versioning + claim-migration registry.
//
// A `ClaimMigration` is a typed transformation from one
// `claim_schema_version` to the next.  Consumers register migrations
// at startup; the branch merge gate walks the registered chain to
// migrate stale-version claims into the target's schema before they
// are applied.
//
// Design choices pinned in code:
//
// - Migrations are *contiguous-version* (`from = N`, `to = N + 1`).
//   The registry validates this on `register` so consumers cannot
//   accidentally skip a version and silently drop a transformation.
//   Multi-step migrations compose by registering one row per step.
//
// - The registry is process-global (`MIGRATION_REGISTRY` —
//   `OnceLock<RwLock<MigrationRegistry>>`).  CLI / serve / tests
//   each populate it once during process boot via
//   `register_migration`; the merge gate reads it under a short-lived
//   read lock.  We chose a global because migrations are pure
//   functions of `(Claim, from, to)` with no per-workspace state, so
//   the alternative — threading a `&MigrationRegistry` parameter
//   through `execute_merge_into_cancellable`, the engine's
//   `merge_into_branch_cancellable`, and every REST/MCP entry — is
//   pure ceremony for zero gain.  Tests get isolation via
//   `MigrationRegistry::clear_for_test`.
//
// - Migration application is fail-fast: the FIRST migration error
//   aborts the chain and propagates out.  Don't catch-and-log; a
//   broken migration that silently leaves stale-version claims in
//   place would violate the "no silent failure" honesty rule.
//
// - The workspace-meta key is `"claim_schema_version"` (distinct
//   from `compile_schema_version`, which gates the structural
//   substrate's own backfill chain).  Both keys live in the same
//   `workspace_meta` relation; readers fetch via
//   `GraphStore::get_workspace_meta` and writers via `set_workspace_meta`.

use std::sync::{OnceLock, RwLock};

use crate::error::{Error, Result};
use crate::types::Claim;

/// One step in the claim-migration chain.  Always migrates from
/// `from` to `from + 1` — multi-step migrations compose.
#[derive(Clone)]
pub struct ClaimMigration {
    /// Source schema version this migration consumes.
    pub from: u32,
    /// Destination schema version this migration produces.  Must
    /// equal `from + 1`; the registry rejects non-contiguous rows.
    pub to: u32,
    /// Human-readable identifier surfaced in audit logs and error
    /// messages.  Convention: `"v{from}-to-v{to}-{slug}"`, e.g.
    /// `"v1-to-v2-add-symbol-field"`.
    pub name: String,
    /// The transformation.  Mutates the claim in place and returns
    /// `Ok(())` on success.  Any error aborts the entire merge.
    pub apply: fn(&mut Claim) -> Result<()>,
}

/// The current schema version this build expects new claims to
/// carry.  Bumped manually by an engine release that introduces a
/// new claim shape; consumers register the corresponding
/// `ClaimMigration` so older workspaces upgrade on next merge.
pub const CURRENT_CLAIM_SCHEMA_VERSION: u32 = 1;

/// The workspace-meta key under which claim schema version is
/// persisted.  Reader / writer should treat absent as `1`
/// (the version pinned by `CURRENT_CLAIM_SCHEMA_VERSION` at the
/// time T3.6 shipped) so pre-T3.6 workspaces keep working without an
/// explicit migration step.
pub const CLAIM_SCHEMA_VERSION_META_KEY: &str = "claim_schema_version";

/// Ordered registry of contiguous-version migrations.  The vec is
/// kept sorted by `from` ascending; lookups are O(N) which is fine
/// because in practice a workspace registers a handful (under 10)
/// migrations across an engine's lifetime.
#[derive(Default)]
pub struct MigrationRegistry {
    migrations: Vec<ClaimMigration>,
}

impl MigrationRegistry {
    /// Register a migration.  Returns `Err(Error::Config)` if:
    /// - `to != from + 1` (non-contiguous), or
    /// - a migration with the same `from → to` is already
    ///   registered (duplicate).
    ///
    /// The registry stays sorted by `from` ascending after every
    /// successful insert so `migrate_claim` can walk it in order.
    pub fn register(&mut self, migration: ClaimMigration) -> Result<()> {
        if migration.to != migration.from.saturating_add(1) {
            return Err(Error::Config(format!(
                "claim migration '{}' is non-contiguous: from={}, to={} \
                 (must be from=N, to=N+1)",
                migration.name, migration.from, migration.to
            )));
        }
        if self
            .migrations
            .iter()
            .any(|m| m.from == migration.from && m.to == migration.to)
        {
            return Err(Error::Config(format!(
                "claim migration {} → {} already registered (existing: '{}', \
                 attempted: '{}')",
                migration.from,
                migration.to,
                self.migrations
                    .iter()
                    .find(|m| m.from == migration.from)
                    .map(|m| m.name.as_str())
                    .unwrap_or("?"),
                migration.name
            )));
        }
        self.migrations.push(migration);
        self.migrations.sort_by_key(|m| m.from);
        Ok(())
    }

    /// Apply every migration step in sequence to bring `claim` from
    /// `from` to `to`.  No-op when `from >= to`.  Errors out when
    /// there is a gap in the chain (e.g. requested 1 → 3 but only
    /// 1 → 2 is registered) — the alternative would be silently
    /// shipping a partially-migrated claim.
    pub fn migrate_claim(&self, claim: &mut Claim, from: u32, to: u32) -> Result<()> {
        if from >= to {
            return Ok(());
        }
        let mut current = from;
        while current < to {
            let Some(step) = self.migrations.iter().find(|m| m.from == current) else {
                return Err(Error::Config(format!(
                    "no claim migration registered for v{} → v{} (chain target v{})",
                    current,
                    current + 1,
                    to
                )));
            };
            (step.apply)(claim)?;
            current = step.to;
        }
        Ok(())
    }

    /// Number of registered migrations — surfaced for audit /
    /// diagnostic logging.  Not used by the merge gate.
    pub fn len(&self) -> usize {
        self.migrations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.migrations.is_empty()
    }

    /// Test-only: drop every registered migration.  Production
    /// callers must NOT use this — the registry is process-global
    /// and clearing it mid-run would let a stale-version branch
    /// merge into main without migration.
    #[doc(hidden)]
    pub fn clear_for_test(&mut self) {
        self.migrations.clear();
    }
}

/// Process-global migration registry.  Initialised lazily on first
/// access; protected by an `RwLock` so the merge gate can read it
/// concurrently with consumer-driven registrations at startup.
fn global_registry() -> &'static RwLock<MigrationRegistry> {
    static REGISTRY: OnceLock<RwLock<MigrationRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(MigrationRegistry::default()))
}

/// Register a migration in the process-global registry.  Consumers
/// (CLI bootstrap, serve daemon startup, in-process tests) call this
/// once per migration during boot.
///
/// Returns the same error variants as `MigrationRegistry::register`.
pub fn register_migration(migration: ClaimMigration) -> Result<()> {
    global_registry()
        .write()
        .map_err(|e| Error::Config(format!("migration registry poisoned: {e}")))?
        .register(migration)
}

/// Apply the registered migration chain to `claim`.  Used by the
/// merge gate to migrate stale-version branch claims into the
/// target's schema before they are merged.
pub fn migrate_claim(claim: &mut Claim, from: u32, to: u32) -> Result<()> {
    global_registry()
        .read()
        .map_err(|e| Error::Config(format!("migration registry poisoned: {e}")))?
        .migrate_claim(claim, from, to)
}

/// Test-only: reset the global registry.  Tests that exercise the
/// registry MUST call this before registering their own rows so
/// state from a sibling test cannot leak in.
#[doc(hidden)]
pub fn clear_global_registry_for_test() {
    if let Ok(mut reg) = global_registry().write() {
        reg.clear_for_test();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClaimType, Confidence, PipelineVersion, Sensitivity, SourceId, WorkspaceId};
    use chrono::Utc;

    fn fresh_claim(stmt: &str) -> Claim {
        let now = Utc::now();
        Claim {
            id: crate::types::ClaimId::new(),
            statement: stmt.to_string(),
            claim_type: ClaimType::Fact,
            source: SourceId::new(),
            source_span: None,
            confidence: Confidence::new(0.8),
            valid_from: now,
            valid_until: None,
            sensitivity: Sensitivity::Public,
            workspace: WorkspaceId::new(),
            extracted_by: PipelineVersion::current(),
            superseded_by: None,
            created_at: now,
            grounding_score: None,
            grounding_method: None,
            extraction_tier: crate::types::ExtractionTier::default(),
            event_date: None,
            admission_tier: crate::types::AdmissionTier::default(),
            derivation: None,
            predicate: None,
            last_rooted_at: None,
            row_blake3: None,
            symbol: None,
        }
    }

    fn append_v1_to_v2(claim: &mut Claim) -> Result<()> {
        claim.statement = format!("[v2] {}", claim.statement);
        Ok(())
    }

    fn append_v2_to_v3(claim: &mut Claim) -> Result<()> {
        claim.statement = format!("{} (v3)", claim.statement);
        Ok(())
    }

    fn always_fails(_claim: &mut Claim) -> Result<()> {
        Err(Error::Config("intentional test failure".into()))
    }

    #[test]
    fn registry_rejects_non_contiguous_migration() {
        let mut reg = MigrationRegistry::default();
        let res = reg.register(ClaimMigration {
            from: 1,
            to: 3,
            name: "skip-v2".into(),
            apply: append_v1_to_v2,
        });
        assert!(res.is_err());
    }

    #[test]
    fn registry_rejects_duplicate_step() {
        let mut reg = MigrationRegistry::default();
        reg.register(ClaimMigration {
            from: 1,
            to: 2,
            name: "first".into(),
            apply: append_v1_to_v2,
        })
        .unwrap();
        let dup = reg.register(ClaimMigration {
            from: 1,
            to: 2,
            name: "second".into(),
            apply: append_v2_to_v3,
        });
        assert!(dup.is_err());
    }

    #[test]
    fn migrate_claim_walks_chain_in_order() {
        let mut reg = MigrationRegistry::default();
        // Register OUT OF ORDER on purpose to pin that the registry
        // sorts on insert.
        reg.register(ClaimMigration {
            from: 2,
            to: 3,
            name: "v2-to-v3".into(),
            apply: append_v2_to_v3,
        })
        .unwrap();
        reg.register(ClaimMigration {
            from: 1,
            to: 2,
            name: "v1-to-v2".into(),
            apply: append_v1_to_v2,
        })
        .unwrap();

        let mut claim = fresh_claim("hello");
        reg.migrate_claim(&mut claim, 1, 3).unwrap();
        assert_eq!(claim.statement, "[v2] hello (v3)");
    }

    #[test]
    fn migrate_claim_is_noop_when_from_ge_to() {
        let reg = MigrationRegistry::default();
        let mut claim = fresh_claim("hello");
        // No-op even with no registered migrations.
        reg.migrate_claim(&mut claim, 5, 3).unwrap();
        reg.migrate_claim(&mut claim, 3, 3).unwrap();
        assert_eq!(claim.statement, "hello");
    }

    #[test]
    fn migrate_claim_errors_on_chain_gap() {
        let mut reg = MigrationRegistry::default();
        reg.register(ClaimMigration {
            from: 1,
            to: 2,
            name: "v1-to-v2".into(),
            apply: append_v1_to_v2,
        })
        .unwrap();
        // Request 1 → 3; v2 → v3 is missing.
        let mut claim = fresh_claim("hello");
        let res = reg.migrate_claim(&mut claim, 1, 3);
        assert!(res.is_err());
    }

    #[test]
    fn failing_migration_aborts_chain_and_propagates() {
        let mut reg = MigrationRegistry::default();
        reg.register(ClaimMigration {
            from: 1,
            to: 2,
            name: "boom".into(),
            apply: always_fails,
        })
        .unwrap();
        let mut claim = fresh_claim("hello");
        let res = reg.migrate_claim(&mut claim, 1, 2);
        assert!(res.is_err());
        // The original statement must be preserved (the migration
        // mutated nothing before erroring).
        assert_eq!(claim.statement, "hello");
    }
}
