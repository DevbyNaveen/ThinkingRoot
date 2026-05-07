//! Tunables that govern the extractor's failure-mode behaviour.
//!
//! Lifted out of `extractor.rs` so they're addressable from
//! integration tests and tracing without dragging in the full
//! extractor module.

/// Bail-fast threshold for the LLM extraction loop: once this many
/// batches in a row have failed with a `permanent` error (HTTP
/// 401/403/404 fingerprint, missing-config, security-violation,
/// unsupported-file-type), the pipeline stops issuing new requests
/// and surfaces an actionable [`thinkingroot_core::Error::LlmProvider`]
/// quoting the latest upstream message.
///
/// Why 8 — at the default `chunks_max=64 / max_concurrent_requests=5`
/// extraction batch caps, eight consecutive failures can fire in
/// parallel and must all return before the collector observes a
/// success. A lower bound (e.g. 3) trips on a single transient hiccup
/// that happens to land back-to-back; a higher bound (e.g. 32) wastes
/// minutes against a genuinely-dead key on workspaces with many
/// hundreds of batches.  Eight survived the empirical test of an
/// Azure 401 storm against a 477-batch compile and bailed cleanly
/// inside the first ~10 seconds.
pub const MAX_CONSECUTIVE_PERMANENT_FAILURES: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_is_sane() {
        // Sanity: the threshold must be > the typical concurrent
        // request budget so a single batch with a transient stall
        // doesn't trip the bail-out.  The default budget is
        // `max_concurrent_requests = 5`, so anything > 5 is fine.
        assert!(
            MAX_CONSECUTIVE_PERMANENT_FAILURES >= 6,
            "threshold ({MAX_CONSECUTIVE_PERMANENT_FAILURES}) must exceed the default \
             concurrent-request budget (5) so a single transient retry-storm doesn't \
             accidentally bail the whole pipeline"
        );
        // And it must be small enough to bail before a long compile
        // wastes user wall-time on a dead key.  At ~1s per permanent
        // failure (no retries since they're permanent), 32+ would
        // mean half a minute of false hope.
        assert!(
            MAX_CONSECUTIVE_PERMANENT_FAILURES <= 16,
            "threshold ({MAX_CONSECUTIVE_PERMANENT_FAILURES}) must stay small enough that \
             an obviously-dead key bails quickly (rule of thumb: under 16s of wall time)"
        );
    }
}
