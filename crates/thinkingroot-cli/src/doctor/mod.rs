//! `root doctor` — single source of truth for setup + health
//! diagnosis.  Three skins (terminal pretty-print, --json,
//! --fix [--interactive]) on one check matrix.
//!
//! Spec: `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md` §2.
//!
//! Coexists with `crate::doctor_cmd` (the legacy implementation)
//! until Task 12 deletes it.

pub mod check;

pub use check::{CheckId, CheckResult, CheckStatus, DoctorEnv, FixAction};
