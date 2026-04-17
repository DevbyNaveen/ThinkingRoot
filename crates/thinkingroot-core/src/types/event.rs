use serde::{Deserialize, Serialize};

/// An SVO (Subject-Verb-Object) event extracted from a claim during compilation.
///
/// The Event Calendar pre-compiles these at pipeline time into the CozoDB `events`
/// table, enabling temporal queries at 50µs Datalog speed instead of 100-200ms
/// LLM runtime extraction (Chronos-style).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEvent {
    pub id: String,
    /// Entity ID of the subject (who/what performed the action).
    pub subject_entity_id: String,
    /// Action verb normalised to lower-case (e.g. "visited", "decided", "ate").
    pub verb: String,
    /// Entity ID of the object (what the subject acted on). May be empty.
    pub object_entity_id: String,
    /// Unix epoch seconds (f64 for sub-second precision).
    pub timestamp: f64,
    /// ISO 8601 date string, e.g. "2025-03-15" or "2025-03" (month precision).
    /// Empty string when no date could be resolved.
    pub normalized_date: String,
    pub source_id: String,
    pub confidence: f64,
}
