//! `CloudError` — single error type for every cloud-touching code path.

/// Single error type for every cloud-touching code path in the OSS
/// engine. Variants land in Slice 1 Task 2.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("placeholder — variants land in Slice 1 Task 2")]
    Placeholder,
}
