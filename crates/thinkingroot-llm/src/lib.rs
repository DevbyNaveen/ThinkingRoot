//! Chat-time LLM substrate for ThinkingRoot.
//!
//! Separated from `thinkingroot-extract` (mechanical Witness Mesh
//! extraction) post-cutover so the extract crate's purpose is honest.
//! At compile time the engine consults no LLM — every Witness is
//! derived deterministically from primary bytes by a rule from
//! `thinkingroot_extract::rule_catalog`.
//!
//! This crate ships the surfaces that DO consult an LLM:
//!
//! - **`llm`** — provider-agnostic chat client (Anthropic / OpenAI /
//!   Azure / Bedrock / Ollama / CloudManaged + structural-only
//!   no-op), streaming + tool-call support.
//! - **`prompts`** — the system + few-shot prompt strings used at
//!   chat time (and retained for the deprecated batch-extract API).
//! - **`scheduler`** — `HeaderRateLimits` + `ThroughputScheduler`
//!   that read `x-ratelimit-*` response headers and back off the
//!   next request before the provider 429s us.
//! - **`citation`** — `CITATION_PROMPT` + `CitationParser` for
//!   `[[id]]`-style provenance markers the synthesizer asks the LLM
//!   to emit.
//! - **`readme`** — markdown README composition with begin/end
//!   markers so the chat assistant can rewrite a section without
//!   touching the rest of the file.
//! - **`graph_context`** — `GraphPrimedContext` / `KnownEntity` /
//!   `KnownRelation` priming structures the chat layer feeds into
//!   prompts to keep entity names canonical across turns.
//! - **`events`** — `EventExtractor::extract_from_claims_with_llm`,
//!   SVO miner consumed by the chat-time event view.
//! - **`checkpoint`** — `InFlightCheckpoint` JSONL persistence used
//!   by the (deprecated but still-callable) batch extract path so
//!   long-running chats can resume mid-stream.
//!
//! Consumers: `thinkingroot-serve::intelligence::*` (synthesizer,
//! agent, react, builtin_tools, tools, agent_streaming) and CLI
//! `eval_cmd`.

pub mod checkpoint;
pub mod citation;
pub mod events;
pub mod graph_context;
pub mod llm;
pub mod prompts;
pub mod readme;
pub mod scheduler;

pub use checkpoint::InFlightCheckpoint;
pub use events::EventExtractor;
pub use graph_context::{GraphPrimedContext, KnownEntity, KnownRelation};
