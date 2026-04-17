pub mod dedup;
mod grounder;
mod lexical;
mod span;

#[cfg(feature = "vector")]
pub mod nli;
#[cfg(feature = "vector")]
mod semantic;

pub use grounder::{Grounder, GroundingConfig, GroundingProgressFn, GroundingVerdict};
pub use lexical::LexicalJudge;
pub use span::SpanJudge;

#[cfg(feature = "vector")]
pub use nli::{NliJudge, NliJudgePool};
#[cfg(feature = "vector")]
pub use semantic::SemanticJudge;
