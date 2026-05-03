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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_tables_count_is_16() {
        assert_eq!(STRUCTURAL_TABLES.len(), 16);
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
