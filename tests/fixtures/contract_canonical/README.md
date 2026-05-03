# Canonical Contract Fixture

Workspace used by the Compile Completeness Contract CI gates
(`tests/contract_invariants.rs` Test 12.1–12.5).

## Architecture

The fixture exercises **every** Phase 6.7 emitter at minimum cardinality
so the byte-coverage audit (Phase 9) sees a representative cross-section
of source kinds.

## Components

See [auth.rs](./auth.rs) for the Rust function example. The user
table appears in [users.csv](./users.csv).

| File | Exercises |
| --- | --- |
| auth.rs | code_signatures, function_calls, doc_tags, code_markers, test_annotations, code_metrics |
| Cargo.toml | config_tree, manifest_dep |
| users.csv | data_rows |
| config.json | config_tree, data_rows |
| deprecated.py | code_signatures, doc_tags, quantities |

Reach out via support@example.com if a fixture file needs updating.
