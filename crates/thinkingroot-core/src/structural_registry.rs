//! Single source of truth for the structural tables that participate in
//! per-source cascade, Phase 9 audit union, and migration sweeps.
//!
//! Adding a new structural table to the schema MUST add its entry here,
//! or the cascade-coverage compile-time check fails (see
//! `STRUCTURAL_TABLES_COVER_CASCADE` in
//! `crates/thinkingroot-graph/src/structural_inserts.rs`).

#[derive(Debug, Clone, Copy)]
pub struct StructuralTableSpec {
    pub name: &'static str,
    /// Column name used to filter rows by source.  Almost always
    /// `"source_id"`; `source_references` uses `"from_source_id"` because
    /// it has both `from_` and `to_` source-id columns.
    pub source_id_column: &'static str,
}

pub const STRUCTURAL_TABLES: &[StructuralTableSpec] = &[
    StructuralTableSpec { name: "function_calls",     source_id_column: "source_id" },
    StructuralTableSpec { name: "code_imports",        source_id_column: "from_source" },
    StructuralTableSpec { name: "headings",           source_id_column: "source_id" },
    StructuralTableSpec { name: "doc_tags",           source_id_column: "source_id" },
    StructuralTableSpec { name: "code_links",         source_id_column: "source_id" },
    StructuralTableSpec { name: "code_signatures",    source_id_column: "source_id" },
    StructuralTableSpec { name: "config_tree",        source_id_column: "source_id" },
    StructuralTableSpec { name: "data_rows",          source_id_column: "source_id" },
    StructuralTableSpec { name: "chunks_residual",    source_id_column: "source_id" },
    StructuralTableSpec { name: "quantities",         source_id_column: "source_id" },
    StructuralTableSpec { name: "source_annotations", source_id_column: "source_id" },
    StructuralTableSpec { name: "source_references",  source_id_column: "from_source_id" },
    StructuralTableSpec { name: "code_markers",       source_id_column: "source_id" },
    StructuralTableSpec { name: "test_annotations",   source_id_column: "source_id" },
    StructuralTableSpec { name: "git_blame",          source_id_column: "source_id" },
    StructuralTableSpec { name: "git_commits",        source_id_column: "source_id" },
    StructuralTableSpec { name: "code_metrics",       source_id_column: "source_id" },
];

/// Generate the CozoDB Datalog script that projects primary-key columns for
/// rows in `name` matching `sid_col = $sid`, then issues a `:rm` on those
/// keys.  Used by both the cascade delete in `graph.rs` and the migration
/// GC sweep in `backfill.rs` — a single canonical copy avoids drift between
/// the two callers.
///
/// Composite-key tables (those where `source_id` is part of the PK) require
/// unification syntax (`source_id = $sid`) so Cozo resolves the PK tuple
/// before the `:rm`.  Tables whose PK is a standalone `id` column use the
/// filter binding `{sid_col}: $sid` directly in the pattern.
pub fn pk_rm_script_for_table(name: &str, sid_col: &str) -> String {
    match name {
        "code_signatures" => format!(
            r#"?[claim_id] := *{name}{{claim_id, {sid_col}: $sid}}
            :rm {name} {{claim_id}}"#
        ),
        "config_tree" => format!(
            r#"?[source_id, dotted_path] := *{name}{{source_id, dotted_path}}, source_id = $sid
            :rm {name} {{source_id, dotted_path}}"#
        ),
        "git_commits" => format!(
            r#"?[source_id, commit_sha] := *{name}{{source_id, commit_sha}}, source_id = $sid
            :rm {name} {{source_id, commit_sha}}"#
        ),
        "git_blame" => format!(
            r#"?[source_id, line_start, line_end] := *{name}{{source_id, line_start, line_end}}, source_id = $sid
            :rm {name} {{source_id, line_start, line_end}}"#
        ),
        _ => format!(
            r#"?[id] := *{name}{{id, {sid_col}: $sid}}
            :rm {name} {{id}}"#
        ),
    }
}

/// Batched IN-set variant of [`pk_rm_script_for_table`]. Generates a
/// Datalog script that removes rows for every source id in `$sids` —
/// one query that fans out across N sources instead of N queries each
/// scoped to one source.
///
/// `$sids` is a parameter expected to be a `DataValue::List` of
/// 1-element `DataValue::List`s, e.g. `[[s1], [s2], [s3]]`. The
/// `candidate[sid] <- $sids` introduction unpacks the rows into a
/// candidate relation, which is then joined inside the rule body
/// against the target table.
///
/// Used by `GraphStore::transactional_remove_sources` (the Tier 2
/// batched Phase 4 path). Composite-key tables follow the same
/// projection rules as the single-source variant.
pub fn pk_rm_script_for_table_batched(name: &str, sid_col: &str) -> String {
    match name {
        "code_signatures" => format!(
            r#"candidate[sid] <- $sids
               ?[claim_id] := candidate[sid], *{name}{{claim_id, {sid_col}: sid}}
               :rm {name} {{claim_id}}"#
        ),
        "config_tree" => format!(
            r#"candidate[sid] <- $sids
               ?[source_id, dotted_path] := candidate[sid], *{name}{{source_id, dotted_path}}, source_id = sid
               :rm {name} {{source_id, dotted_path}}"#
        ),
        "git_commits" => format!(
            r#"candidate[sid] <- $sids
               ?[source_id, commit_sha] := candidate[sid], *{name}{{source_id, commit_sha}}, source_id = sid
               :rm {name} {{source_id, commit_sha}}"#
        ),
        "git_blame" => format!(
            r#"candidate[sid] <- $sids
               ?[source_id, line_start, line_end] := candidate[sid], *{name}{{source_id, line_start, line_end}}, source_id = sid
               :rm {name} {{source_id, line_start, line_end}}"#
        ),
        _ => format!(
            r#"candidate[sid] <- $sids
               ?[id] := candidate[sid], *{name}{{id, {sid_col}: sid}}
               :rm {name} {{id}}"#
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_tables_count_is_17() {
        assert_eq!(STRUCTURAL_TABLES.len(), 17);
    }

    #[test]
    fn structural_table_names_unique() {
        let mut names: Vec<&'static str> = STRUCTURAL_TABLES.iter().map(|s| s.name).collect();
        names.sort();
        let original_len = names.len();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate table names in STRUCTURAL_TABLES");
    }

    #[test]
    fn source_references_uses_from_source_id_column() {
        let spec = STRUCTURAL_TABLES.iter().find(|s| s.name == "source_references").unwrap();
        assert_eq!(spec.source_id_column, "from_source_id");
    }
}
