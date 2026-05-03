pub mod config;
pub mod error;
pub mod global_config;
pub mod id;
pub mod ir;
pub mod safe_path;
pub mod structural_registry;
pub mod types;

pub use config::Config;
pub use error::{Error, Result};
pub use global_config::{
    Credentials, GlobalConfig, ServeConfig, WorkspaceEntry, WorkspaceRegistry,
};
pub use id::Id;
pub use safe_path::{atomic_write, is_loopback_host, safe_join_under, validate_id};
pub use structural_registry::{STRUCTURAL_TABLES, StructuralTableSpec};
pub use types::*;
