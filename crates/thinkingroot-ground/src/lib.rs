mod lexical;
mod span;
mod grounder;
pub mod dedup;

#[cfg(feature = "vector")]
mod semantic;
#[cfg(feature = "vector")]
pub mod nli;

pub use grounder::{Grounder, GroundingConfig, GroundingVerdict};
pub use lexical::LexicalJudge;
pub use span::SpanJudge;

#[cfg(feature = "vector")]
pub use semantic::SemanticJudge;
#[cfg(feature = "vector")]
pub use nli::NliJudge;
