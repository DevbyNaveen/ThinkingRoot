pub mod dedup;
mod grounder;
mod lexical;
mod span;

#[cfg(feature = "vector")]
pub mod nli;
#[cfg(feature = "vector")]
mod semantic;

pub use grounder::{Grounder, GroundingConfig, GroundingVerdict};
pub use lexical::LexicalJudge;
pub use span::SpanJudge;

#[cfg(feature = "vector")]
pub use nli::NliJudge;
#[cfg(feature = "vector")]
pub use semantic::SemanticJudge;
