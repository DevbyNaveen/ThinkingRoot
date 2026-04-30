use std::sync::Arc;

use thinkingroot_core::config::{AzureConfig, LlmConfig, ProviderConfig};
use thinkingroot_core::{Error, Result};

use crate::prompts;
use crate::scheduler::{HeaderRateLimits, ThroughputScheduler};
use crate::schema::ExtractionResult;

/// Output of a single provider chat call.
struct ChatOutput {
    text: String,
    truncated: bool,
    /// Rate limit headers from the response (empty for Bedrock/Ollama).
    limits: HeaderRateLimits,
}

// ── Streaming chat ──────────────────────────────────────────────
//
// Public surface for token-by-token streaming. The synthesizer pipes
// these chunks straight into the engine's SSE endpoint, which the
// desktop's chat command consumes — see `crates/thinkingroot-serve`
// `rest::ask_stream_handler` and `apps/thinkingroot-desktop/src-tauri`
// `commands::chat::chat_send_stream` for the endpoints.

/// One token-or-segment chunk emitted by a streaming provider call.
///
/// Most chunks carry a non-empty `text` and `finish == None`. The final
/// chunk in a successful stream carries `finish == Some(_)` so callers
/// can attach truncation flags + rate-limit headers without sniffing
/// the underlying transport.
#[derive(Debug, Clone, Default)]
pub struct ChatChunk {
    pub text: String,
    pub finish: Option<ChatFinish>,
}

/// Terminal metadata for a streamed chat. Only present on the last
/// chunk of a successful stream.
#[derive(Debug, Clone, Default)]
pub struct ChatFinish {
    /// True when the upstream stopped because it hit `max_tokens` —
    /// the same signal we surface for non-streaming chats via
    /// `Error::TruncatedOutput`. Streaming callers can choose to
    /// re-prompt rather than treat the partial body as final.
    pub truncated: bool,
    /// Rate-limit headers carried back to the scheduler so adaptive
    /// concurrency can adjust mid-flight.
    pub limits: HeaderRateLimits,
}

/// Pinned, boxed stream of `ChatChunk` results — the public surface
/// every `LlmClient::chat_stream` consumer holds onto.
pub type ChatStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<ChatChunk>> + Send>>;

// ── Tool calling — public types ──────────────────────────────────
//
// Surface used by `LlmClient::chat_with_tools`. The five providers
// each have a different native tool-use wire format; these types are
// the provider-agnostic shape every caller talks in. Per-provider
// translation lives in the helpers + `chat_with_tools` impls below.

/// A tool the LLM may call.
///
/// `input_schema` is JSON Schema. It is passed verbatim to providers
/// that accept JSON Schema (Anthropic, OpenAI, Azure, OpenAI-compatible
/// hosts including Ollama) and converted to the AWS smithy `Document`
/// type for Bedrock. The expected shape is
/// `{"type": "object", "properties": {...}, "required": [...]}`.
#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// A model-emitted tool call. The `id` is provider-supplied — every
/// provider's wire format carries a stable identifier so the caller can
/// echo it back in the matching `ToolResult`. Anthropic and Bedrock use
/// `tool_use_id`; OpenAI / Azure / Ollama call it `tool_call_id`. We
/// normalise to `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// The result of executing a `ToolCall`. Pass back as
/// [`ChatMessage::ToolResults`] on the next turn.
///
/// `content` is plain text — typically the JSON-stringified tool output
/// for structured results, or human-readable text for free-form ones.
/// Anthropic and Bedrock support a structured-content variant we don't
/// expose here on purpose, since flattening to a string keeps every
/// provider symmetrical and tools are ultimately consumed by the LLM as
/// text anyway.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: String,
    /// True when the tool reported a runtime error rather than returning
    /// data. Anthropic/Bedrock surface this as a native `is_error` /
    /// `status: "error"` flag; OpenAI/Azure/Ollama have no such field
    /// so we prepend `ERROR: ` to the content for those providers.
    pub is_error: bool,
}

/// One turn in a tool-using conversation. Mirrors the shape every
/// provider's messages array maps to under the hood, with the
/// per-provider wire-format details handled in the helpers.
///
/// `User` carries the user's message. `AssistantText` is a model reply
/// without tool use (the conversation can terminate here). The
/// remaining two variants always come in pairs: an
/// `AssistantToolCalls` (model requested calls) is followed by a
/// `ToolResults` (caller dispatched and is feeding results back) so the
/// model can integrate them and either request more or emit final text.
#[derive(Debug, Clone)]
pub enum ChatMessage {
    User(String),
    AssistantText(String),
    AssistantToolCalls(Vec<ToolCall>),
    ToolResults(Vec<ToolResult>),
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        ChatMessage::User(text.into())
    }
    pub fn assistant_text(text: impl Into<String>) -> Self {
        ChatMessage::AssistantText(text.into())
    }
    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        ChatMessage::AssistantToolCalls(calls)
    }
    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        ChatMessage::ToolResults(results)
    }
}

/// What the LLM emitted at the end of a `chat_with_tools` call.
///
/// The agent loop in `thinkingroot-serve::intelligence::agent` (S3)
/// iterates while the response is `ToolCalls`, dispatching each call
/// and feeding the results back, and terminates when the response is
/// `Text`.
#[derive(Debug, Clone)]
pub enum ToolUseResponse {
    /// Terminal text answer; the conversation ends here.
    Text {
        text: String,
        truncated: bool,
        limits: HeaderRateLimits,
    },
    /// LLM wants to call N tools before producing its final answer.
    /// `text_preamble` carries any text the model emitted alongside
    /// the tool calls (Anthropic does this routinely; OpenAI/Azure
    /// rarely, but it's possible) so the caller can surface it
    /// before kicking off the dispatch.
    ToolCalls {
        calls: Vec<ToolCall>,
        text_preamble: String,
        limits: HeaderRateLimits,
    },
}

/// Tool selection policy passed to [`LlmClient::chat_with_tools`].
///
/// `Auto` lets the model choose between text and tools (the right
/// default for most conversations). `Any` forces the model to call
/// some tool. `Named` forces a specific tool. `None` disables tool
/// use entirely — useful for the final synthesis turn where the
/// caller already has all the data and just wants prose.
#[derive(Debug, Clone)]
pub enum ToolChoice {
    Auto,
    Any,
    None,
    Named(String),
}

impl Default for ToolChoice {
    fn default() -> Self {
        ToolChoice::Auto
    }
}

// ── Model-aware output token limits ─────────────────────────────

/// Returns the maximum output tokens for a known model.
/// Falls back to a conservative 8_192 for unknown models.
/// Whether an Azure / OpenAI deployment requires the newer
/// `max_completion_tokens` field instead of the deprecated `max_tokens`.
///
/// Applies to GPT-5.x family (2025-08-07 onwards) and the o-series
/// reasoning models (o1, o3, o4). Models that fail this check use the
/// legacy `max_tokens`. Called on both the model name ("gpt-5.4") and
/// deployment name ("gpt-5.4" or a custom deployment label) so callers
/// can pass whichever identifier they have.
pub fn requires_max_completion_tokens(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // GPT-5 family and everything labelled with a 5.x or 6.x version.
    lower.starts_with("gpt-5")
        || lower.starts_with("gpt-6")
        // Reasoning models: o1, o3, o4 + mini variants.
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

pub fn model_max_output_tokens(model: &str) -> i32 {
    let m = model.to_lowercase();

    // Claude Haiku 4.5 — 64k output
    if m.contains("haiku-4-5") || m.contains("haiku-4.5") {
        return 64_000;
    }
    // Claude Haiku 3 — 4k output
    if m.contains("haiku") {
        return 4_096;
    }
    // Claude Sonnet / Opus 4.x — 8k output
    if m.contains("sonnet") || m.contains("opus") {
        return 8_192;
    }
    // GPT-4.1 family (2025) — 32k output
    if m.contains("gpt-4.1") || m.contains("gpt-4-1") {
        return 32_768;
    }
    // GPT-4o family — 16k output
    if m.contains("gpt-4o") || m.contains("gpt-4-turbo") {
        return 16_384;
    }
    // GPT-3.5 — 4k output
    if m.contains("gpt-3.5") || m.contains("gpt-35") {
        return 4_096;
    }
    // Llama 3.x (Groq, Together, Ollama)
    if m.contains("llama-3") || m.contains("llama3") {
        return 8_192;
    }
    // Mistral / Mixtral
    if m.contains("mistral") || m.contains("mixtral") {
        return 8_192;
    }
    // DeepSeek
    if m.contains("deepseek") {
        return 8_192;
    }
    // Nova (Bedrock)
    if m.contains("nova") {
        return 5_120;
    }

    // Unknown model — safe default that works everywhere
    8_192
}

/// Returns the input context window (in tokens) for a known model.
/// Falls back to a conservative 32_768 for unknown models.
/// Sources: official provider documentation, April 2026.
pub fn model_context_window(model: &str) -> usize {
    let m = model.to_lowercase();

    // ── Anthropic Claude ────────────────────────────────────────────
    // Sonnet 4.6, Opus 4.6, Opus 4.7 — 1M context
    if m.contains("sonnet") || m.contains("opus") {
        return 1_000_000;
    }
    // Haiku 4.5 — 200K context
    if m.contains("haiku-4-5") || m.contains("haiku-4.5") {
        return 200_000;
    }
    // Haiku 3 — 200K context
    if m.contains("haiku") {
        return 200_000;
    }

    // ── OpenAI / Azure gpt-4.1 family (2025) — 1M on direct, 300K on Azure standard ──
    // We conservatively use 300K (the Azure standard cap) so the same table works for both.
    if m.contains("gpt-4.1") || m.contains("gpt-4-1") {
        return 300_000;
    }
    // gpt-4o family — 128K
    if m.contains("gpt-4o") || m.contains("gpt-4-turbo") {
        return 128_000;
    }
    // gpt-3.5 — 16K
    if m.contains("gpt-3.5") || m.contains("gpt-35") {
        return 16_384;
    }

    // ── Amazon Bedrock Nova ─────────────────────────────────────────
    // Nova Lite / Pro — 300K context
    if m.contains("nova-lite") || m.contains("nova-pro") {
        return 300_000;
    }
    // Nova Micro — 128K context
    if m.contains("nova-micro") || m.contains("nova") {
        return 128_000;
    }

    // ── Groq / Together / Meta Llama ───────────────────────────────
    // Llama 3.x — 131K (production Groq/Together limit)
    if m.contains("llama-3") || m.contains("llama3") || m.contains("llama-4") {
        return 131_072;
    }

    // ── Mistral / Mixtral ──────────────────────────────────────────
    // Mixtral-8x7b — 32K (legacy; new mistral-large is 128K)
    if m.contains("mixtral") {
        return 32_768;
    }
    if m.contains("mistral-large") || m.contains("mistral-medium") {
        return 128_000;
    }
    if m.contains("mistral") {
        return 32_768;
    }

    // ── DeepSeek ───────────────────────────────────────────────────
    if m.contains("deepseek") {
        return 128_000;
    }

    // ── Perplexity Sonar ───────────────────────────────────────────
    // Sonar models are search-grounded; web retrieval consumes ~30% of context.
    // We report the raw window but batch size is further capped in model_batch_size.
    if m.contains("sonar") {
        return 127_000;
    }

    // ── Ollama (local) ─────────────────────────────────────────────
    // Ollama default num_ctx is 2048 regardless of model native limit.
    // We return 2048 as the safe default; users who set num_ctx in their
    // Ollama server will benefit from a higher batch size via config override.
    if m.contains("ollama") {
        return 2_048;
    }

    // Unknown model — conservative safe default
    32_768
}

/// Returns the safe extraction batch size for a given provider + model combination.
///
/// Takes the minimum of two constraints:
///   input_safe  = floor((context_window * 0.80 - overhead) / max_chunk_tokens)
///   output_safe = floor(max_output_tokens / tokens_per_chunk_output)
///
/// Constants:
///   overhead            = 700   (system prompt ~500 + batch wrapper ~200)
///   tokens_per_chunk_output = 500   (typical JSON output per extracted chunk)
///   safety_margin       = 0.80  (guards tokenizer variance + prompt reformatting)
///
/// Clamped to [1, 64] — never zero (at least try 1 chunk), never more than 64
/// (empirical ceiling where LLMs reliably track chunk IDs and maintain JSON format).
pub fn model_batch_size(provider: &str, model: &str, max_chunk_tokens: usize) -> usize {
    let context = model_context_window(model);
    let max_output = model_max_output_tokens(model) as usize;

    // Perplexity sonar: search grounding consumes ~30% of context, not suitable for batching
    if provider == "perplexity" || model.to_lowercase().contains("sonar") {
        return 1;
    }

    // Ollama: default num_ctx of 2048 fits at most 1 chunk — user must override via config
    if provider == "ollama" {
        let m = model.to_lowercase();
        // Detect explicitly larger models by name (user chose them knowing the size)
        if m.contains("llama3.1") || m.contains("llama-3.1") || m.contains("llama-3.3") {
            // Still conservative — user must set num_ctx in Ollama to benefit
            return 2;
        }
        return 1;
    }

    const OVERHEAD: usize = 700;
    const OUTPUT_PER_CHUNK: usize = 500;
    const HARD_MAX: usize = 64;

    let safe_input = context * 4 / 5; // 80% safety margin (integer arithmetic, no floats)
    let input_safe_n = if safe_input > OVERHEAD + max_chunk_tokens {
        (safe_input - OVERHEAD) / max_chunk_tokens
    } else {
        1
    };

    let output_safe_n = max_output / OUTPUT_PER_CHUNK;

    let mut n = input_safe_n.min(output_safe_n).clamp(1, HARD_MAX);

    // Bedrock has account-level throughput caps that make >128K-token
    // batches stall under default quotas (TCP open, 0% CPU, no progress
    // for >5min). Cap at 8 so a single batch is ≤16K input tokens —
    // sub-second on the converse API in our smoke tests.
    if provider == "bedrock" {
        n = n.min(8);
    }

    tracing::debug!(
        "batch_size for {provider}/{model}: context={context} output={max_output} \
         input_safe={input_safe_n} output_safe={output_safe_n} → {n}"
    );

    n
}

// ── Provider Enum (enum dispatch — zero-cost, no dyn) ────────────

enum Provider {
    Bedrock(BedrockProvider),
    Azure(AzureProvider),
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    Ollama(OllamaProvider),
}

impl Provider {
    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        match self {
            Provider::Bedrock(p) => p.chat(system, user).await,
            Provider::Azure(p) => p.chat(system, user).await,
            Provider::OpenAi(p) => p.chat(system, user).await,
            Provider::Anthropic(p) => p.chat(system, user).await,
            Provider::Ollama(p) => p.chat(system, user).await,
        }
    }

    /// Streaming counterpart of `chat`. Each provider that supports
    /// native SSE (Anthropic, OpenAI-compatible, Azure) overrides
    /// this and yields chunks as the upstream emits them; providers
    /// without native SSE (Bedrock, Ollama) fall through to the
    /// one-shot wrap below so callers get a uniform surface — they
    /// always see at least one chunk and exactly one `ChatFinish` on
    /// success.
    async fn chat_stream(&self, system: &str, user: &str) -> Result<ChatStream> {
        match self {
            Provider::Anthropic(p) => p.chat_stream(system, user).await,
            Provider::Azure(p) => p.chat_stream(system, user).await,
            // OpenAi covers 9 providers (openai, groq, deepseek,
            // openrouter, together, perplexity, litellm, custom, plus
            // any OpenAI-compatible host) — one SSE parser unlocks
            // them all.
            Provider::OpenAi(p) => p.chat_stream(system, user).await,
            // Bedrock and Ollama fall through to the one-shot wrap —
            // their native streaming APIs (InvokeModelWithResponseStream
            // / NDJSON) follow different shapes and aren't load-bearing
            // for v1; the desktop chat surface routes through Anthropic
            // / OpenAI / Azure in practice.
            Provider::Bedrock(_) | Provider::Ollama(_) => {
                let out = self.chat(system, user).await?;
                let chunk = ChatChunk {
                    text: out.text,
                    finish: Some(ChatFinish {
                        truncated: out.truncated,
                        limits: out.limits,
                    }),
                };
                let stream = async_stream::stream! { yield Ok(chunk); };
                Ok(Box::pin(stream))
            }
        }
    }

    fn model_name(&self) -> &str {
        match self {
            Provider::Bedrock(p) => &p.model,
            Provider::Azure(p) => &p.model,
            Provider::OpenAi(p) => &p.model,
            Provider::Anthropic(p) => &p.model,
            Provider::Ollama(p) => &p.model,
        }
    }

    fn provider_name(&self) -> &str {
        match self {
            Provider::Bedrock(_) => "bedrock",
            Provider::Azure(_) => "azure",
            Provider::OpenAi(p) => p.provider_name.as_str(),
            Provider::Anthropic(_) => "anthropic",
            Provider::Ollama(_) => "ollama",
        }
    }

    /// Tool-calling chat. Dispatches to each provider's native tool-use
    /// wire format (Anthropic Messages `tool_use` blocks, OpenAI Chat
    /// Completions `tool_calls` array, Bedrock Converse `toolConfig`,
    /// Ollama OpenAI-compat function calling). Public surface is
    /// [`LlmClient::chat_with_tools`].
    async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse> {
        match self {
            Provider::Anthropic(p) => p.chat_with_tools(system, messages, tools, tool_choice).await,
            Provider::Azure(p) => p.chat_with_tools(system, messages, tools, tool_choice).await,
            Provider::OpenAi(p) => p.chat_with_tools(system, messages, tools, tool_choice).await,
            Provider::Bedrock(p) => p.chat_with_tools(system, messages, tools, tool_choice).await,
            Provider::Ollama(p) => p.chat_with_tools(system, messages, tools, tool_choice).await,
        }
    }
}

// ── Bedrock Provider (AWS) ───────────────────────────────────────

struct BedrockProvider {
    client: aws_sdk_bedrockruntime::Client,
    model: String,
    max_output_tokens: i32,
}

impl BedrockProvider {
    async fn new(model: &str, region: &str) -> Result<Self> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        let client = aws_sdk_bedrockruntime::Client::new(&config);
        let max_output_tokens = model_max_output_tokens(model);
        Ok(Self {
            client,
            model: model.to_string(),
            max_output_tokens,
        })
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        use aws_sdk_bedrockruntime::types::{
            ContentBlock, ConversationRole, InferenceConfiguration, Message, SystemContentBlock,
        };

        tracing::debug!(
            "bedrock: sending request to {} (input ~{} chars, max_output={})",
            self.model,
            user.len(),
            self.max_output_tokens
        );

        let response = self
            .client
            .converse()
            .model_id(&self.model)
            .system(SystemContentBlock::Text(system.to_string()))
            .inference_config(
                InferenceConfiguration::builder()
                    .max_tokens(self.max_output_tokens)
                    .temperature(0.1_f32)
                    .build(),
            )
            .messages(
                Message::builder()
                    .role(ConversationRole::User)
                    .content(ContentBlock::Text(user.to_string()))
                    .build()
                    .map_err(|e| Error::LlmProvider {
                        provider: "bedrock".into(),
                        message: format!("failed to build message: {e}"),
                    })?,
            )
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: format!("bedrock/{}", self.model),
                message: e.to_string(),
            })?;

        // Detect truncation via stop reason.
        let truncated = matches!(
            response.stop_reason(),
            aws_sdk_bedrockruntime::types::StopReason::MaxTokens
        );

        if truncated {
            tracing::warn!(
                "bedrock: output truncated for model {} (hit {} token limit)",
                self.model,
                self.max_output_tokens
            );
        } else {
            tracing::debug!("bedrock: got complete response");
        }

        let output = response.output().ok_or_else(|| Error::LlmProvider {
            provider: "bedrock".into(),
            message: "no output in response".into(),
        })?;

        match output {
            aws_sdk_bedrockruntime::types::ConverseOutput::Message(msg) => {
                for block in msg.content() {
                    if let ContentBlock::Text(text) = block {
                        return Ok(ChatOutput {
                            text: text.clone(),
                            truncated,
                            limits: HeaderRateLimits::default(), // Bedrock uses SDK, no HTTP headers
                        });
                    }
                }
                Err(Error::LlmProvider {
                    provider: "bedrock".into(),
                    message: "no text in response".into(),
                })
            }
            _ => Err(Error::LlmProvider {
                provider: "bedrock".into(),
                message: "unexpected output type".into(),
            }),
        }
    }

    /// Tool-calling chat for Bedrock via the Converse API. Translates
    /// our portable [`ChatMessage`] / [`Tool`] / [`ToolChoice`] shape
    /// into the AWS SDK types (`ToolConfiguration`, `ToolSpecification`,
    /// `ToolInputSchema::Json(Document)`, `ContentBlock::ToolUse`,
    /// `ContentBlock::ToolResult`) and back.
    ///
    /// Tool-use availability is model-dependent (Claude on Bedrock and
    /// Nova fully support it; not every Bedrock model does). The SDK
    /// surfaces this as a `ValidationException` which we propagate
    /// verbatim.
    async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse> {
        use aws_sdk_bedrockruntime::types::{
            AnyToolChoice, AutoToolChoice, ContentBlock, ConversationRole,
            InferenceConfiguration, Message, SpecificToolChoice, StopReason,
            SystemContentBlock, Tool as BedrockTool, ToolChoice as BedrockToolChoice,
            ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock,
            ToolResultStatus, ToolSpecification, ToolUseBlock,
        };

        // ── Build toolConfig ─────────────────────────────────────
        let bedrock_tools: Vec<BedrockTool> = tools
            .iter()
            .map(|t| {
                let spec = ToolSpecification::builder()
                    .name(&t.name)
                    .description(&t.description)
                    .input_schema(ToolInputSchema::Json(json_to_document(&t.input_schema)))
                    .build()
                    .map_err(|e| Error::LlmProvider {
                        provider: "bedrock".into(),
                        message: format!("build ToolSpecification: {e}"),
                    })?;
                Ok::<BedrockTool, Error>(BedrockTool::ToolSpec(spec))
            })
            .collect::<Result<Vec<_>>>()?;

        let bedrock_choice = match tool_choice {
            ToolChoice::Auto | ToolChoice::None => {
                BedrockToolChoice::Auto(AutoToolChoice::builder().build())
            }
            ToolChoice::Any => BedrockToolChoice::Any(AnyToolChoice::builder().build()),
            ToolChoice::Named(name) => BedrockToolChoice::Tool(
                SpecificToolChoice::builder()
                    .name(name)
                    .build()
                    .map_err(|e| Error::LlmProvider {
                        provider: "bedrock".into(),
                        message: format!("build SpecificToolChoice: {e}"),
                    })?,
            ),
        };

        let tool_config = ToolConfiguration::builder()
            .set_tools(Some(bedrock_tools))
            .tool_choice(bedrock_choice)
            .build()
            .map_err(|e| Error::LlmProvider {
                provider: "bedrock".into(),
                message: format!("build ToolConfiguration: {e}"),
            })?;

        // ── Build messages ───────────────────────────────────────
        let mut bedrock_messages: Vec<Message> = Vec::with_capacity(messages.len());
        for msg in messages {
            let m = match msg {
                ChatMessage::User(text) => Message::builder()
                    .role(ConversationRole::User)
                    .content(ContentBlock::Text(text.clone()))
                    .build(),
                ChatMessage::AssistantText(text) => Message::builder()
                    .role(ConversationRole::Assistant)
                    .content(ContentBlock::Text(text.clone()))
                    .build(),
                ChatMessage::AssistantToolCalls(calls) => {
                    let mut b = Message::builder().role(ConversationRole::Assistant);
                    for c in calls {
                        let block = ToolUseBlock::builder()
                            .tool_use_id(&c.id)
                            .name(&c.name)
                            .input(json_to_document(&c.input))
                            .build()
                            .map_err(|e| Error::LlmProvider {
                                provider: "bedrock".into(),
                                message: format!("build ToolUseBlock: {e}"),
                            })?;
                        b = b.content(ContentBlock::ToolUse(block));
                    }
                    b.build()
                }
                ChatMessage::ToolResults(results) => {
                    let mut b = Message::builder().role(ConversationRole::User);
                    for r in results {
                        let status = if r.is_error {
                            ToolResultStatus::Error
                        } else {
                            ToolResultStatus::Success
                        };
                        let block = ToolResultBlock::builder()
                            .tool_use_id(&r.tool_use_id)
                            .content(ToolResultContentBlock::Text(r.content.clone()))
                            .status(status)
                            .build()
                            .map_err(|e| Error::LlmProvider {
                                provider: "bedrock".into(),
                                message: format!("build ToolResultBlock: {e}"),
                            })?;
                        b = b.content(ContentBlock::ToolResult(block));
                    }
                    b.build()
                }
            }
            .map_err(|e| Error::LlmProvider {
                provider: "bedrock".into(),
                message: format!("build Message: {e}"),
            })?;
            bedrock_messages.push(m);
        }

        // ── Send ────────────────────────────────────────────────
        let response = self
            .client
            .converse()
            .model_id(&self.model)
            .system(SystemContentBlock::Text(system.to_string()))
            .inference_config(
                InferenceConfiguration::builder()
                    .max_tokens(self.max_output_tokens)
                    .temperature(0.1_f32)
                    .build(),
            )
            .tool_config(tool_config)
            .set_messages(Some(bedrock_messages))
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: format!("bedrock/{}", self.model),
                message: e.to_string(),
            })?;

        let truncated = matches!(response.stop_reason(), StopReason::MaxTokens);
        let stopped_for_tool_use = matches!(response.stop_reason(), StopReason::ToolUse);

        let output = response.output().ok_or_else(|| Error::LlmProvider {
            provider: "bedrock".into(),
            message: "no output in response".into(),
        })?;

        let aws_sdk_bedrockruntime::types::ConverseOutput::Message(msg) = output else {
            return Err(Error::LlmProvider {
                provider: "bedrock".into(),
                message: "unexpected output type".into(),
            });
        };

        // ── Parse content blocks ─────────────────────────────────
        let mut text_buf = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        for block in msg.content() {
            match block {
                ContentBlock::Text(text) => text_buf.push_str(text),
                ContentBlock::ToolUse(tu) => {
                    calls.push(ToolCall {
                        id: tu.tool_use_id().to_string(),
                        name: tu.name().to_string(),
                        input: document_to_json(tu.input()),
                    });
                }
                _ => { /* future block types — ignore */ }
            }
        }

        if !calls.is_empty() || stopped_for_tool_use {
            Ok(ToolUseResponse::ToolCalls {
                calls,
                text_preamble: text_buf,
                limits: HeaderRateLimits::default(),
            })
        } else {
            Ok(ToolUseResponse::Text {
                text: text_buf,
                truncated,
                limits: HeaderRateLimits::default(),
            })
        }
    }
}

// ── Azure OpenAI Provider ────────────────────────────────────────
// Auth: `api-key` header (not `Authorization: Bearer`).
// URL:  https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}
// The `model` field is OMITTED from the request body — it is implied by the deployment.

struct AzureProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,        // deployment name; used for display/logging
    endpoint_url: String, // pre-built full URL with api-version query param
    max_output_tokens: i32,
}

impl AzureProvider {
    fn new(api_key: &str, model: &str, cfg: &AzureConfig) -> Result<Self> {
        let deployment = cfg.deployment.as_deref().ok_or_else(|| {
            Error::MissingConfig("set [llm.providers.azure].deployment in your config".into())
        })?;
        let api_version = cfg.api_version.as_deref().unwrap_or("2024-12-01-preview");

        // endpoint_base overrides resource_name — used for AIServices/Foundry resources
        // that expose cognitiveservices.azure.com instead of openai.azure.com.
        let base = if let Some(base) = cfg.endpoint_base.as_deref() {
            base.trim_end_matches('/').to_string()
        } else {
            let resource = cfg.resource_name.as_deref().ok_or_else(|| {
                Error::MissingConfig(
                    "set [llm.providers.azure].resource_name or endpoint_base in your config"
                        .into(),
                )
            })?;
            format!("https://{resource}.openai.azure.com")
        };

        let endpoint_url = format!(
            "{base}/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
        );
        let max_output_tokens = model_max_output_tokens(model);

        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .unwrap_or_default(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            endpoint_url,
            max_output_tokens,
        })
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        // Azure AOAI: no `model` field in body — deployment is in the URL.
        //
        // GPT-5.x and the o-series reasoning models require
        // `max_completion_tokens` in place of the deprecated `max_tokens`,
        // and reject the latter with an "Unsupported parameter" 400. Detect
        // by model / deployment name so existing GPT-4.x callers are
        // unchanged.
        let uses_new_param = requires_max_completion_tokens(&self.model);
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user",   "content": user},
            ],
            "temperature": 0.1,
        });
        if uses_new_param {
            body["max_completion_tokens"] = serde_json::json!(self.max_output_tokens);
        } else {
            body["max_tokens"] = serde_json::json!(self.max_output_tokens);
        }

        let resp = self
            .client
            .post(&self.endpoint_url)
            .header("api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "azure".into(),
                message: e.to_string(),
            })?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: "azure".into(),
                retry_after_ms: retry_after,
            });
        }

        // Azure returns the same OpenAI rate-limit headers.
        let limits = HeaderRateLimits::from_headers(resp.headers());

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "azure".into(),
            message: e.to_string(),
        })?;

        let finish_reason = json["choices"][0]["finish_reason"].as_str().unwrap_or("");
        let truncated = finish_reason == "length";

        if truncated {
            tracing::warn!(
                "azure: output truncated for deployment {} (finish_reason=length, max_tokens={})",
                self.model,
                self.max_output_tokens,
            );
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| ChatOutput {
                text: s.to_string(),
                truncated,
                limits,
            })
            .ok_or_else(|| Error::LlmProvider {
                provider: "azure".into(),
                message: format!("unexpected response: {json}"),
            })
    }

    /// Real SSE streaming via the Azure deployment's
    /// `chat/completions` endpoint with `?api-version=...&stream=true`.
    ///
    /// Same OpenAI-compatible SSE shape as
    /// [`OpenAiProvider::chat_stream`], but the deployment is in the
    /// pre-built endpoint URL (not the body) and auth is via the
    /// `api-key` header rather than `Authorization: Bearer`.
    async fn chat_stream(&self, system: &str, user: &str) -> Result<ChatStream> {
        use eventsource_stream::Eventsource;
        use futures::StreamExt;

        let uses_new_param = requires_max_completion_tokens(&self.model);
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user",   "content": user},
            ],
            "temperature": 0.1,
            "stream": true,
        });
        if uses_new_param {
            body["max_completion_tokens"] = serde_json::json!(self.max_output_tokens);
        } else {
            body["max_tokens"] = serde_json::json!(self.max_output_tokens);
        }

        let resp = self
            .client
            .post(&self.endpoint_url)
            .header("api-key", &self.api_key)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "azure".into(),
                message: format!("connect: {e}"),
            })?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: "azure".into(),
                retry_after_ms: retry_after,
            });
        }
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::LlmProvider {
                provider: "azure".into(),
                message: format!("http {s}: {body}"),
            });
        }

        let limits = HeaderRateLimits::from_headers(resp.headers());
        let mut events = resp.bytes_stream().eventsource();

        let stream = async_stream::stream! {
            let mut truncated = false;
            while let Some(item) = events.next().await {
                match item {
                    Err(e) => {
                        yield Err(Error::LlmProvider {
                            provider: "azure".into(),
                            message: format!("sse parse: {e}"),
                        });
                        return;
                    }
                    Ok(ev) => {
                        if ev.data == "[DONE]" {
                            yield Ok(ChatChunk {
                                text: String::new(),
                                finish: Some(ChatFinish {
                                    truncated,
                                    limits: limits.clone(),
                                }),
                            });
                            return;
                        }
                        let json: serde_json::Value =
                            match serde_json::from_str(&ev.data) {
                                Ok(v) => v,
                                Err(e) => {
                                    yield Err(Error::LlmProvider {
                                        provider: "azure".into(),
                                        message: format!("decode delta: {e}"),
                                    });
                                    return;
                                }
                            };
                        let choice = &json["choices"][0];
                        if let Some(text) =
                            choice["delta"]["content"].as_str()
                        {
                            if !text.is_empty() {
                                yield Ok(ChatChunk {
                                    text: text.to_string(),
                                    finish: None,
                                });
                            }
                        }
                        if choice["finish_reason"].as_str()
                            == Some("length")
                        {
                            truncated = true;
                        }
                    }
                }
            }
            yield Ok(ChatChunk {
                text: String::new(),
                finish: Some(ChatFinish {
                    truncated,
                    limits: limits.clone(),
                }),
            });
        };

        Ok(Box::pin(stream))
    }

    /// Tool-calling chat for Azure OpenAI deployments. Same Chat
    /// Completions wire format as the OpenAI native provider —
    /// the only differences are the `api-key` header, the
    /// pre-built deployment URL with `?api-version=...`, and the
    /// omitted `model` field in the body. Tool-call response
    /// parsing is identical, so we share `parse_openai_tool_response`.
    async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse> {
        let uses_new_param = requires_max_completion_tokens(&self.model);
        let mut body = serde_json::json!({
            "messages": openai_messages_array(system, messages),
            "tools": openai_tools_array(tools),
            "tool_choice": openai_tool_choice(tool_choice),
            "temperature": 0.1,
        });
        if uses_new_param {
            body["max_completion_tokens"] = serde_json::json!(self.max_output_tokens);
        } else {
            body["max_tokens"] = serde_json::json!(self.max_output_tokens);
        }

        let resp = self
            .client
            .post(&self.endpoint_url)
            .header("api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "azure".into(),
                message: e.to_string(),
            })?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: "azure".into(),
                retry_after_ms: retry_after,
            });
        }
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::LlmProvider {
                provider: "azure".into(),
                message: format!("http {s}: {body}"),
            });
        }

        let limits = HeaderRateLimits::from_headers(resp.headers());
        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "azure".into(),
            message: e.to_string(),
        })?;
        parse_openai_tool_response(&json, limits, "azure")
    }
}

// ── OpenAI-compatible Provider ───────────────────────────────────

struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    provider_name: String,
    max_output_tokens: i32,
}

impl OpenAiProvider {
    fn new(api_key: &str, model: &str, base_url: &str, provider_name: &str) -> Self {
        let max_output_tokens = model_max_output_tokens(model);
        // Strip trailing /v1 so providers that store "https://host/v1" in config
        // don't end up with a double /v1 when chat() appends /v1/chat/completions.
        let base_url = base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches('/')
            .to_string();
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .unwrap_or_default(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            base_url,
            provider_name: provider_name.to_string(),
            max_output_tokens,
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        // Same max_tokens → max_completion_tokens switch as the Azure
        // provider: GPT-5.x / o-series require the newer field.
        let uses_new_param = requires_max_completion_tokens(&self.model);
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.1,
        });
        if uses_new_param {
            body["max_completion_tokens"] = serde_json::json!(self.max_output_tokens);
        } else {
            body["max_tokens"] = serde_json::json!(self.max_output_tokens);
        }

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: self.provider_name.clone(),
                message: e.to_string(),
            })?;

        // Detect rate-limit before consuming body.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: self.provider_name.clone(),
                retry_after_ms: retry_after,
            });
        }

        // Capture rate limit headers before consuming body.
        let limits = HeaderRateLimits::from_headers(resp.headers());

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: self.provider_name.clone(),
            message: e.to_string(),
        })?;

        // Detect truncation via finish_reason.
        let finish_reason = json["choices"][0]["finish_reason"].as_str().unwrap_or("");
        let truncated = finish_reason == "length";

        if truncated {
            tracing::warn!(
                "{}: output truncated for model {} (finish_reason=length, max_tokens={})",
                self.provider_name,
                self.model,
                self.max_output_tokens
            );
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| ChatOutput {
                text: s.to_string(),
                truncated,
                limits,
            })
            .ok_or_else(|| Error::LlmProvider {
                provider: self.provider_name.clone(),
                message: format!("unexpected response: {json}"),
            })
    }

    /// Real SSE streaming via the OpenAI-compatible
    /// `/v1/chat/completions?stream=true` endpoint.
    ///
    /// This impl is shared by **9 providers** the workspace wires
    /// through `OpenAiProvider`: openai, groq, deepseek, openrouter,
    /// together, perplexity, litellm, custom, plus any
    /// OpenAI-compatible host.
    ///
    /// SSE shape: each `data:` line carries a JSON object with
    /// `choices[0].delta.content` for text deltas, terminated by
    /// `data: [DONE]`. We project deltas into [`ChatChunk`]s and
    /// surface `finish_reason == "length"` as the
    /// [`ChatFinish::truncated`] flag.
    async fn chat_stream(&self, system: &str, user: &str) -> Result<ChatStream> {
        use eventsource_stream::Eventsource;
        use futures::StreamExt;

        let uses_new_param = requires_max_completion_tokens(&self.model);
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.1,
            "stream": true,
        });
        if uses_new_param {
            body["max_completion_tokens"] = serde_json::json!(self.max_output_tokens);
        } else {
            body["max_tokens"] = serde_json::json!(self.max_output_tokens);
        }

        let provider = self.provider_name.clone();
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: provider.clone(),
                message: format!("connect: {e}"),
            })?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider,
                retry_after_ms: retry_after,
            });
        }
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::LlmProvider {
                provider,
                message: format!("http {s}: {body}"),
            });
        }

        let limits = HeaderRateLimits::from_headers(resp.headers());
        let mut events = resp.bytes_stream().eventsource();

        let stream = async_stream::stream! {
            let mut truncated = false;
            while let Some(item) = events.next().await {
                match item {
                    Err(e) => {
                        yield Err(Error::LlmProvider {
                            provider: provider.clone(),
                            message: format!("sse parse: {e}"),
                        });
                        return;
                    }
                    Ok(ev) => {
                        // OpenAI-compatible SSE uses unnamed events
                        // (`data:` only, no `event:` line) and signals
                        // the end with the literal payload `[DONE]`.
                        if ev.data == "[DONE]" {
                            yield Ok(ChatChunk {
                                text: String::new(),
                                finish: Some(ChatFinish {
                                    truncated,
                                    limits: limits.clone(),
                                }),
                            });
                            return;
                        }
                        let json: serde_json::Value =
                            match serde_json::from_str(&ev.data) {
                                Ok(v) => v,
                                Err(e) => {
                                    yield Err(Error::LlmProvider {
                                        provider: provider.clone(),
                                        message: format!("decode delta: {e}"),
                                    });
                                    return;
                                }
                            };
                        let choice = &json["choices"][0];
                        if let Some(text) =
                            choice["delta"]["content"].as_str()
                        {
                            if !text.is_empty() {
                                yield Ok(ChatChunk {
                                    text: text.to_string(),
                                    finish: None,
                                });
                            }
                        }
                        if choice["finish_reason"].as_str()
                            == Some("length")
                        {
                            truncated = true;
                        }
                    }
                }
            }
            // Some upstreams close the stream without [DONE]; emit a
            // best-effort terminal chunk so callers always see a
            // ChatFinish.
            yield Ok(ChatChunk {
                text: String::new(),
                finish: Some(ChatFinish {
                    truncated,
                    limits: limits.clone(),
                }),
            });
        };

        Ok(Box::pin(stream))
    }

    /// Tool-calling chat for OpenAI-compatible hosts (openai, groq,
    /// deepseek, openrouter, together, perplexity, litellm, custom).
    ///
    /// Same Chat Completions endpoint as `chat`, with the addition of
    /// `tools` + `tool_choice` in the body and `tool_calls` parsing in
    /// the response. Unsupported tool requests get the OpenAI-shape
    /// 4xx error surfaced verbatim — we do not pre-flight against a
    /// model whitelist because the upstream's "supports tools" matrix
    /// changes too fast for that to be honest.
    async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse> {
        let uses_new_param = requires_max_completion_tokens(&self.model);
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": openai_messages_array(system, messages),
            "tools": openai_tools_array(tools),
            "tool_choice": openai_tool_choice(tool_choice),
            "temperature": 0.1,
        });
        if uses_new_param {
            body["max_completion_tokens"] = serde_json::json!(self.max_output_tokens);
        } else {
            body["max_tokens"] = serde_json::json!(self.max_output_tokens);
        }

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: self.provider_name.clone(),
                message: e.to_string(),
            })?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: self.provider_name.clone(),
                retry_after_ms: retry_after,
            });
        }
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::LlmProvider {
                provider: self.provider_name.clone(),
                message: format!("http {s}: {body}"),
            });
        }

        let limits = HeaderRateLimits::from_headers(resp.headers());
        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: self.provider_name.clone(),
            message: e.to_string(),
        })?;
        parse_openai_tool_response(&json, limits, &self.provider_name)
    }
}

// ── Anthropic Provider ───────────────────────────────────────────

struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_output_tokens: i32,
}

impl AnthropicProvider {
    fn new(api_key: &str, model: &str) -> Self {
        let max_output_tokens = model_max_output_tokens(model);
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .unwrap_or_default(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            max_output_tokens,
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_output_tokens,
            "temperature": 0.1,
            "system": system,
            "messages": [
                {"role": "user", "content": user},
            ],
        });

        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "anthropic".into(),
                message: e.to_string(),
            })?;

        // Detect rate-limit (429) or overloaded (529).
        let status = resp.status().as_u16();
        if status == 429 || status == 529 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: "anthropic".into(),
                retry_after_ms: retry_after,
            });
        }

        // Capture rate limit headers before consuming body.
        let limits = HeaderRateLimits::from_headers(resp.headers());

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "anthropic".into(),
            message: e.to_string(),
        })?;

        // Detect truncation via stop_reason.
        let stop_reason = json["stop_reason"].as_str().unwrap_or("");
        let truncated = stop_reason == "max_tokens";

        if truncated {
            tracing::warn!(
                "anthropic: output truncated for model {} (stop_reason=max_tokens, max_tokens={})",
                self.model,
                self.max_output_tokens
            );
        }

        json["content"][0]["text"]
            .as_str()
            .map(|s| ChatOutput {
                text: s.to_string(),
                truncated,
                limits,
            })
            .ok_or_else(|| Error::LlmProvider {
                provider: "anthropic".into(),
                message: format!("unexpected response: {json}"),
            })
    }

    /// Real SSE streaming via the `/v1/messages?stream=true` endpoint.
    ///
    /// The wire format is documented at
    /// <https://docs.anthropic.com/en/api/messages-streaming>. We
    /// project Anthropic's `content_block_delta` events (the only
    /// event type that carries text) into [`ChatChunk`]s and surface
    /// the `stop_reason` from `message_delta` as the
    /// [`ChatFinish::truncated`] signal on the closing chunk emitted
    /// at `message_stop`.
    ///
    /// Errors:
    /// - 429 / 529 → `Error::RateLimited` returned synchronously
    ///   from the open (callers can retry the open).
    /// - Non-2xx other → `Error::LlmProvider` synchronously.
    /// - Mid-stream `event: error` → yielded as an `Err` item.
    /// - Bytes that fail to parse as SSE → yielded as `Err` items
    ///   so the stream surfaces the failure rather than silently
    ///   truncating.
    async fn chat_stream(&self, system: &str, user: &str) -> Result<ChatStream> {
        use eventsource_stream::Eventsource;
        use futures::StreamExt;

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_output_tokens,
            "temperature": 0.1,
            "system": system,
            "messages": [
                {"role": "user", "content": user},
            ],
            "stream": true,
        });

        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "anthropic".into(),
                message: format!("connect: {e}"),
            })?;

        let status = resp.status().as_u16();
        if status == 429 || status == 529 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: "anthropic".into(),
                retry_after_ms: retry_after,
            });
        }
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::LlmProvider {
                provider: "anthropic".into(),
                message: format!("http {s}: {body}"),
            });
        }

        // Capture rate-limit headers up-front; we attach them to the
        // final chunk so the scheduler still sees them on streaming
        // calls.
        let limits = HeaderRateLimits::from_headers(resp.headers());

        let mut events = resp.bytes_stream().eventsource();

        let stream = async_stream::stream! {
            let mut truncated = false;
            while let Some(item) = events.next().await {
                match item {
                    Err(e) => {
                        yield Err(Error::LlmProvider {
                            provider: "anthropic".into(),
                            message: format!("sse parse: {e}"),
                        });
                        return;
                    }
                    Ok(ev) => {
                        // `event:` field carries the type; the data
                        // is JSON. We ignore `ping` (keep-alive) and
                        // `content_block_start` / `content_block_stop`
                        // (no payload of interest).
                        match ev.event.as_str() {
                            "content_block_delta" => {
                                let json: serde_json::Value =
                                    match serde_json::from_str(&ev.data) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            yield Err(Error::LlmProvider {
                                                provider: "anthropic".into(),
                                                message: format!("decode delta: {e}"),
                                            });
                                            return;
                                        }
                                    };
                                let delta_type =
                                    json["delta"]["type"].as_str().unwrap_or("");
                                if delta_type == "text_delta" {
                                    if let Some(text) =
                                        json["delta"]["text"].as_str()
                                    {
                                        if !text.is_empty() {
                                            yield Ok(ChatChunk {
                                                text: text.to_string(),
                                                finish: None,
                                            });
                                        }
                                    }
                                }
                                // `input_json_delta` (tool use) is
                                // intentionally ignored — chat
                                // streaming surfaces text only.
                            }
                            "message_delta" => {
                                let json: serde_json::Value =
                                    serde_json::from_str(&ev.data)
                                        .unwrap_or(serde_json::Value::Null);
                                let stop_reason = json["delta"]["stop_reason"]
                                    .as_str()
                                    .unwrap_or("");
                                if stop_reason == "max_tokens" {
                                    truncated = true;
                                }
                            }
                            "message_stop" => {
                                yield Ok(ChatChunk {
                                    text: String::new(),
                                    finish: Some(ChatFinish {
                                        truncated,
                                        limits: limits.clone(),
                                    }),
                                });
                                return;
                            }
                            "error" => {
                                let json: serde_json::Value =
                                    serde_json::from_str(&ev.data)
                                        .unwrap_or(serde_json::Value::Null);
                                let msg = json["error"]["message"]
                                    .as_str()
                                    .unwrap_or("(no message)")
                                    .to_string();
                                yield Err(Error::LlmProvider {
                                    provider: "anthropic".into(),
                                    message: msg,
                                });
                                return;
                            }
                            _ => { /* ping / message_start / etc. */ }
                        }
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    /// Tool-calling chat for Anthropic Messages API. Native tool_use
    /// content blocks make this the cleanest of the five — the
    /// response can carry both text and tool_use blocks in a single
    /// message, which we surface as `text_preamble` + `calls`.
    async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_output_tokens,
            "temperature": 0.1,
            "system": system,
            "messages": anthropic_messages_array(messages),
            "tools": anthropic_tools_array(tools),
            "tool_choice": anthropic_tool_choice(tool_choice),
        });

        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "anthropic".into(),
                message: e.to_string(),
            })?;

        let status = resp.status().as_u16();
        if status == 429 || status == 529 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: "anthropic".into(),
                retry_after_ms: retry_after,
            });
        }
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::LlmProvider {
                provider: "anthropic".into(),
                message: format!("http {s}: {body}"),
            });
        }

        let limits = HeaderRateLimits::from_headers(resp.headers());
        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "anthropic".into(),
            message: e.to_string(),
        })?;
        parse_anthropic_tool_response(&json, limits)
    }
}

// ── Ollama Provider ──────────────────────────────────────────────

struct OllamaProvider {
    client: reqwest::Client,
    model: String,
    base_url: String,
    max_output_tokens: i32,
}

impl OllamaProvider {
    fn new(model: &str, base_url: &str) -> Self {
        let max_output_tokens = model_max_output_tokens(model);
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .unwrap_or_default(),
            model: model.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            max_output_tokens,
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "stream": false,
            "options": {
                "num_predict": self.max_output_tokens,
            },
        });

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "ollama".into(),
                message: e.to_string(),
            })?;

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "ollama".into(),
            message: e.to_string(),
        })?;

        let finish_reason = json["choices"][0]["finish_reason"].as_str().unwrap_or("");
        let truncated = finish_reason == "length";

        if truncated {
            tracing::warn!(
                "ollama: output truncated for model {} (finish_reason=length)",
                self.model
            );
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| ChatOutput {
                text: s.to_string(),
                truncated,
                limits: HeaderRateLimits::default(), // Ollama has no rate limits
            })
            .ok_or_else(|| Error::LlmProvider {
                provider: "ollama".into(),
                message: format!("unexpected response: {json}"),
            })
    }

    /// Tool-calling chat for Ollama. Wire format mirrors OpenAI Chat
    /// Completions, so we share the helpers — the only differences are
    /// no auth header, no rate-limit headers, and the
    /// `options.num_predict` knob in place of `max_tokens`.
    ///
    /// Tool support is model-dependent on Ollama (llama3.1, mistral-nemo,
    /// command-r, etc.). When the local model doesn't support tools,
    /// Ollama either ignores the `tools` field or returns text only —
    /// either way `parse_openai_tool_response` correctly surfaces a
    /// `ToolUseResponse::Text`. We do not pre-flight against a model
    /// list.
    async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": openai_messages_array(system, messages),
            "tools": openai_tools_array(tools),
            "tool_choice": openai_tool_choice(tool_choice),
            "stream": false,
            "options": {
                "num_predict": self.max_output_tokens,
            },
        });

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "ollama".into(),
                message: e.to_string(),
            })?;

        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::LlmProvider {
                provider: "ollama".into(),
                message: format!("http {s}: {body}"),
            });
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "ollama".into(),
            message: e.to_string(),
        })?;
        parse_openai_tool_response(&json, HeaderRateLimits::default(), "ollama")
    }
}

// ── Provider config helpers ──────────────────────────────────────

/// Resolve the API key for a provider using a three-level priority chain:
///   1. Environment variable (highest priority — allows CI/CD injection without touching files)
///   2. `api_key` field stored in credentials.toml (set by `root setup`)
///   3. Hard error with a clear message pointing to `root setup`
fn resolve_key(cfg: Option<&ProviderConfig>, default_env: &str) -> Result<String> {
    let env_var = cfg
        .and_then(|p| p.api_key_env.as_deref())
        .unwrap_or(default_env);

    // Priority 1: live environment variable
    if let Ok(val) = std::env::var(env_var)
        && !val.is_empty()
    {
        return Ok(val);
    }

    // Priority 2: stored value in ProviderConfig.api_key (populated from credentials.toml
    // by GlobalConfig::load → Credentials::inject_into)
    if let Some(stored) = cfg.and_then(|p| p.api_key.as_deref())
        && !stored.is_empty()
    {
        return Ok(stored.to_string());
    }

    Err(Error::MissingConfig(format!(
        "API key not found. Run `root setup` to configure your LLM provider, \
         or set the {env_var} environment variable."
    )))
}

/// Same as `resolve_key` but returns an empty string rather than Err when no key is
/// available (used for optional-key providers like LiteLLM and Ollama).
fn resolve_key_optional(cfg: Option<&ProviderConfig>) -> String {
    // Priority 1: env var
    if let Some(env_var) = cfg.and_then(|p| p.api_key_env.as_deref())
        && let Ok(val) = std::env::var(env_var)
        && !val.is_empty()
    {
        return val;
    }
    // Priority 2: stored value
    cfg.and_then(|p| p.api_key.as_deref())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

fn resolve_base_url(cfg: Option<&ProviderConfig>, default: &str) -> String {
    cfg.and_then(|p| p.base_url.as_deref())
        .unwrap_or(default)
        .to_string()
}

fn resolve_base_url_required(cfg: Option<&ProviderConfig>, provider: &str) -> Result<String> {
    cfg.and_then(|p| p.base_url.as_deref())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            Error::MissingConfig(format!(
                "set [llm.providers.{provider}].base_url in your config"
            ))
        })
}

// ── Tool wire-format helpers (shared across provider impls) ──────
//
// Three families of helpers:
//
//   * OpenAI-shape: used by OpenAi, Azure, and Ollama (all three speak
//     OpenAI Chat Completions JSON modulo URL + auth).
//   * Anthropic-shape: used by Anthropic Messages API.
//   * Bedrock-shape: smithy Document conversion + Converse SDK types.
//
// The helpers are pure functions over JSON / typed inputs. They have
// no I/O and are unit-tested below in `mod tests`.

/// Build the `tools: [{type: "function", function: {...}}]` array for
/// OpenAI Chat Completions, Azure OpenAI, and OpenAI-compatible hosts
/// (groq, deepseek, openrouter, together, perplexity, litellm, ollama).
fn openai_tools_array(tools: &[Tool]) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                },
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

/// Build the `tool_choice` field for OpenAI-shape providers.
fn openai_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::Value::String("auto".to_string()),
        // OpenAI Chat Completions calls "must use a tool" `required`.
        ToolChoice::Any => serde_json::Value::String("required".to_string()),
        ToolChoice::None => serde_json::Value::String("none".to_string()),
        ToolChoice::Named(name) => serde_json::json!({
            "type": "function",
            "function": {"name": name},
        }),
    }
}

/// Turn the synthesizer's [`ChatMessage`] history into the OpenAI Chat
/// Completions `messages` array. The first element is always the
/// `system` role with `system_prompt`; the rest are mapped 1:N from
/// the input. `User` and `AssistantText` map 1:1; `AssistantToolCalls`
/// folds into one assistant message with a `tool_calls` array;
/// `ToolResults` fans out to one `{role: "tool"}` message per result.
fn openai_messages_array(system_prompt: &str, history: &[ChatMessage]) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(history.len() + 1);
    out.push(serde_json::json!({"role": "system", "content": system_prompt}));
    for msg in history {
        match msg {
            ChatMessage::User(text) => {
                out.push(serde_json::json!({"role": "user", "content": text}));
            }
            ChatMessage::AssistantText(text) => {
                out.push(serde_json::json!({"role": "assistant", "content": text}));
            }
            ChatMessage::AssistantToolCalls(calls) => {
                let tc: Vec<serde_json::Value> = calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": serde_json::to_string(&c.input).unwrap_or_else(|_| "{}".to_string()),
                            },
                        })
                    })
                    .collect();
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": serde_json::Value::Null,
                    "tool_calls": tc,
                }));
            }
            ChatMessage::ToolResults(results) => {
                for r in results {
                    let body = if r.is_error {
                        format!("ERROR: {}", r.content)
                    } else {
                        r.content.clone()
                    };
                    out.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": r.tool_use_id,
                        "content": body,
                    }));
                }
            }
        }
    }
    out
}

/// Parse an OpenAI Chat Completions response body into a
/// [`ToolUseResponse`]. The provider name is used in error messages.
fn parse_openai_tool_response(
    json: &serde_json::Value,
    limits: HeaderRateLimits,
    provider: &str,
) -> Result<ToolUseResponse> {
    let choice = json["choices"].get(0).ok_or_else(|| Error::LlmProvider {
        provider: provider.to_string(),
        message: format!("missing choices in response: {json}"),
    })?;
    let message = &choice["message"];
    let finish_reason = choice["finish_reason"].as_str().unwrap_or("");
    let truncated = finish_reason == "length";

    // Tool-call branch: `message.tool_calls` is a non-empty array.
    if let Some(tcs) = message["tool_calls"].as_array().filter(|a| !a.is_empty()) {
        let mut calls: Vec<ToolCall> = Vec::with_capacity(tcs.len());
        for tc in tcs {
            let id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"]
                .as_str()
                .ok_or_else(|| Error::LlmProvider {
                    provider: provider.to_string(),
                    message: format!("tool_call missing function.name: {tc}"),
                })?
                .to_string();
            // OpenAI returns `arguments` as a stringified JSON object.
            // Parse to serde_json::Value so callers don't need to.
            let raw_args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            let input: serde_json::Value = serde_json::from_str(raw_args).map_err(|e| {
                Error::LlmProvider {
                    provider: provider.to_string(),
                    message: format!("tool_call arguments not JSON: {e}; raw={raw_args}"),
                }
            })?;
            calls.push(ToolCall { id, name, input });
        }
        let text_preamble = message["content"].as_str().unwrap_or("").to_string();
        return Ok(ToolUseResponse::ToolCalls {
            calls,
            text_preamble,
            limits,
        });
    }

    // Plain text branch.
    let text = message["content"]
        .as_str()
        .ok_or_else(|| Error::LlmProvider {
            provider: provider.to_string(),
            message: format!("response missing message.content: {json}"),
        })?
        .to_string();
    Ok(ToolUseResponse::Text {
        text,
        truncated,
        limits,
    })
}

/// Build the `tools: [...]` array for Anthropic Messages API.
fn anthropic_tools_array(tools: &[Tool]) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

/// Build the `tool_choice` field for Anthropic.
fn anthropic_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!({"type": "auto"}),
        ToolChoice::Any => serde_json::json!({"type": "any"}),
        // Anthropic models the "no tools" case by simply not sending
        // tools at all; if we're here the caller already passed tools,
        // so the safest mapping is `auto` and let the model choose
        // text. Bedrock has the same behaviour.
        ToolChoice::None => serde_json::json!({"type": "auto"}),
        ToolChoice::Named(name) => serde_json::json!({"type": "tool", "name": name}),
    }
}

/// Turn [`ChatMessage`] history into the Anthropic Messages
/// `messages: [...]` array. Anthropic's first-message-must-be-user
/// rule is enforced by the caller pattern (the agent always starts
/// from a `User` message); we just translate.
fn anthropic_messages_array(history: &[ChatMessage]) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(history.len());
    for msg in history {
        match msg {
            ChatMessage::User(text) => {
                out.push(serde_json::json!({"role": "user", "content": text}));
            }
            ChatMessage::AssistantText(text) => {
                out.push(serde_json::json!({"role": "assistant", "content": text}));
            }
            ChatMessage::AssistantToolCalls(calls) => {
                let blocks: Vec<serde_json::Value> = calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "type": "tool_use",
                            "id": c.id,
                            "name": c.name,
                            "input": c.input,
                        })
                    })
                    .collect();
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": blocks,
                }));
            }
            ChatMessage::ToolResults(results) => {
                let blocks: Vec<serde_json::Value> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": r.tool_use_id,
                            "content": r.content,
                            "is_error": r.is_error,
                        })
                    })
                    .collect();
                out.push(serde_json::json!({
                    "role": "user",
                    "content": blocks,
                }));
            }
        }
    }
    out
}

/// Parse an Anthropic Messages response body into a [`ToolUseResponse`].
fn parse_anthropic_tool_response(
    json: &serde_json::Value,
    limits: HeaderRateLimits,
) -> Result<ToolUseResponse> {
    let stop_reason = json["stop_reason"].as_str().unwrap_or("");
    let truncated = stop_reason == "max_tokens";
    let content = json["content"].as_array().ok_or_else(|| Error::LlmProvider {
        provider: "anthropic".into(),
        message: format!("response missing content array: {json}"),
    })?;

    let mut text_buf = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();

    for block in content {
        let ty = block["type"].as_str().unwrap_or("");
        match ty {
            "text" => {
                if let Some(t) = block["text"].as_str() {
                    text_buf.push_str(t);
                }
            }
            "tool_use" => {
                let id = block["id"].as_str().unwrap_or("").to_string();
                let name = block["name"]
                    .as_str()
                    .ok_or_else(|| Error::LlmProvider {
                        provider: "anthropic".into(),
                        message: format!("tool_use block missing name: {block}"),
                    })?
                    .to_string();
                let input = block["input"].clone();
                calls.push(ToolCall { id, name, input });
            }
            _ => { /* ignore future block types (e.g. image, document) */ }
        }
    }

    if !calls.is_empty() {
        Ok(ToolUseResponse::ToolCalls {
            calls,
            text_preamble: text_buf,
            limits,
        })
    } else {
        Ok(ToolUseResponse::Text {
            text: text_buf,
            truncated,
            limits,
        })
    }
}

/// Recursively convert a `serde_json::Value` to an
/// `aws_smithy_types::Document`. Used to build Bedrock `toolSpec.
/// inputSchema.json` and `toolUse.input` payloads from our portable
/// JSON shape.
fn json_to_document(v: &serde_json::Value) -> aws_smithy_types::Document {
    use aws_smithy_types::{Document, Number};
    match v {
        serde_json::Value::Null => Document::Null,
        serde_json::Value::Bool(b) => Document::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(Number::NegInt(i))
            } else if let Some(f) = n.as_f64() {
                Document::Number(Number::Float(f))
            } else {
                // Number that isn't representable as u64/i64/f64 — surface
                // it as a string rather than silently dropping it.
                Document::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => Document::String(s.clone()),
        serde_json::Value::Array(arr) => {
            Document::Array(arr.iter().map(json_to_document).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map = std::collections::HashMap::with_capacity(obj.len());
            for (k, v) in obj {
                map.insert(k.clone(), json_to_document(v));
            }
            Document::Object(map)
        }
    }
}

/// Rough character count for a `ChatMessage` — used by the throughput
/// scheduler's token estimate. Cheap and pessimistic; off-by-2x in
/// either direction is fine for adaptive concurrency.
fn estimated_message_chars(msg: &ChatMessage) -> usize {
    match msg {
        ChatMessage::User(t) | ChatMessage::AssistantText(t) => t.len(),
        ChatMessage::AssistantToolCalls(calls) => calls
            .iter()
            .map(|c| c.name.len() + c.input.to_string().len() + 32)
            .sum(),
        ChatMessage::ToolResults(results) => {
            results.iter().map(|r| r.content.len() + 32).sum()
        }
    }
}

/// Inverse of [`json_to_document`] — used when parsing Bedrock
/// `toolUse.input` Documents back into our portable JSON shape.
fn document_to_json(doc: &aws_smithy_types::Document) -> serde_json::Value {
    use aws_smithy_types::{Document, Number};
    match doc {
        Document::Null => serde_json::Value::Null,
        Document::Bool(b) => serde_json::Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => serde_json::Value::Number((*u).into()),
            Number::NegInt(i) => serde_json::Value::Number((*i).into()),
            Number::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
        },
        Document::String(s) => serde_json::Value::String(s.clone()),
        Document::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(document_to_json).collect())
        }
        Document::Object(obj) => {
            let mut map = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                map.insert(k.clone(), document_to_json(v));
            }
            serde_json::Value::Object(map)
        }
    }
}

// ── LLM Client (unified wrapper with retry + truncation handling) ─

pub struct LlmClient {
    provider: Provider,
    max_retries: u32,
    /// Pre-emptive throughput scheduler — gates every send to stay under provider limits.
    pub(crate) scheduler: Option<Arc<ThroughputScheduler>>,
}

impl LlmClient {
    /// Create a new LLM client from config. Auto-detects provider.
    pub async fn new(config: &LlmConfig) -> Result<Self> {
        if !config.is_configured() {
            return Err(Error::MissingConfig(
                "No LLM provider configured.\n  Run `root setup` to get started (takes ~2 minutes).".into(),
            ));
        }
        let provider = match config.default_provider.as_str() {
            "bedrock" => {
                let region = config
                    .providers
                    .bedrock
                    .as_ref()
                    .and_then(|b| b.region.as_deref())
                    .unwrap_or("us-east-1");
                Provider::Bedrock(BedrockProvider::new(&config.extraction_model, region).await?)
            }
            "openai" => {
                let key = resolve_key(config.providers.openai.as_ref(), "OPENAI_API_KEY")?;
                let base_url =
                    resolve_base_url(config.providers.openai.as_ref(), "https://api.openai.com");
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "openai",
                ))
            }
            "azure" => {
                let azure_cfg = config.providers.azure.as_ref().ok_or_else(|| {
                    Error::MissingConfig(
                        "azure provider requires [llm.providers.azure] in your config".into(),
                    )
                })?;
                let key_env = azure_cfg
                    .api_key_env
                    .as_deref()
                    .unwrap_or("AZURE_OPENAI_API_KEY");
                // Priority 1: env var, Priority 2: stored value from credentials.toml
                let key = std::env::var(key_env)
                    .ok()
                    .filter(|s| !s.is_empty())
                    .or_else(|| azure_cfg.api_key.clone().filter(|s| !s.is_empty()))
                    .ok_or_else(|| {
                        Error::MissingConfig(format!(
                            "Azure API key not found. Run `root setup` to configure, \
                             or set the {key_env} environment variable."
                        ))
                    })?;
                Provider::Azure(AzureProvider::new(
                    &key,
                    &config.extraction_model,
                    azure_cfg,
                )?)
            }
            "anthropic" => {
                let key = resolve_key(config.providers.anthropic.as_ref(), "ANTHROPIC_API_KEY")?;
                Provider::Anthropic(AnthropicProvider::new(&key, &config.extraction_model))
            }
            "ollama" => {
                let base_url =
                    resolve_base_url(config.providers.ollama.as_ref(), "http://localhost:11434");
                Provider::Ollama(OllamaProvider::new(&config.extraction_model, &base_url))
            }
            "groq" => {
                let key = resolve_key(config.providers.groq.as_ref(), "GROQ_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.groq.as_ref(),
                    "https://api.groq.com/openai",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "groq",
                ))
            }
            "deepseek" => {
                let key = resolve_key(config.providers.deepseek.as_ref(), "DEEPSEEK_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.deepseek.as_ref(),
                    "https://api.deepseek.com",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "deepseek",
                ))
            }
            "openrouter" => {
                let key = resolve_key(config.providers.openrouter.as_ref(), "OPENROUTER_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.openrouter.as_ref(),
                    "https://openrouter.ai/api/v1",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "openrouter",
                ))
            }
            "together" => {
                let key = resolve_key(config.providers.together.as_ref(), "TOGETHER_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.together.as_ref(),
                    "https://api.together.xyz/v1",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "together",
                ))
            }
            "perplexity" => {
                let key = resolve_key(config.providers.perplexity.as_ref(), "PERPLEXITY_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.perplexity.as_ref(),
                    "https://api.perplexity.ai",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "perplexity",
                ))
            }
            "litellm" => {
                let key = resolve_key_optional(config.providers.litellm.as_ref());
                let base_url =
                    resolve_base_url(config.providers.litellm.as_ref(), "http://localhost:4000");
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "litellm",
                ))
            }
            "custom" => {
                let key = resolve_key(config.providers.custom.as_ref(), "CUSTOM_LLM_API_KEY")?;
                let base_url =
                    resolve_base_url_required(config.providers.custom.as_ref(), "custom")?;
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "custom",
                ))
            }
            other => {
                return Err(Error::MissingConfig(format!(
                    "unsupported provider: {other}. Supported: bedrock, azure, openai, anthropic, ollama, groq, deepseek, openrouter, together, perplexity, litellm, custom"
                )));
            }
        };

        tracing::info!(
            "LLM provider: {} / {} (max_output_tokens={})",
            config.default_provider,
            config.extraction_model,
            model_max_output_tokens(&config.extraction_model),
        );

        Ok(Self {
            provider,
            max_retries: 3,
            scheduler: None,
        })
    }

    /// Create an LlmClient pointed at a specific Azure deployment, bypassing LlmConfig.
    ///
    /// Used when you need a different deployment than the workspace's extraction model —
    /// e.g. a dedicated GPT-4o judge in the eval runner while synthesis uses GPT-4.1.
    /// The `azure_cfg` must have `deployment` set to the target deployment name.
    pub fn for_azure_deployment(
        api_key: &str,
        display_model: &str,
        azure_cfg: &AzureConfig,
    ) -> Result<Self> {
        let provider = Provider::Azure(AzureProvider::new(api_key, display_model, azure_cfg)?);
        Ok(Self {
            provider,
            max_retries: 3,
            scheduler: None,
        })
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn with_scheduler(mut self, s: Arc<ThroughputScheduler>) -> Self {
        self.scheduler = Some(s);
        self
    }

    /// The provider name configured on this client (e.g. `"anthropic"`,
    /// `"azure"`, `"openai"`). Used by surfaces like the desktop's pre-flight
    /// LLM health endpoint to render which backend a workspace will hit.
    pub fn provider_name(&self) -> &str {
        self.provider.provider_name()
    }

    /// The model name configured on this client (e.g. `"claude-sonnet-4-5"`).
    pub fn model_name(&self) -> &str {
        self.provider.model_name()
    }

    /// Streaming counterpart of [`chat`](Self::chat). Returns a pinned
    /// stream of [`ChatChunk`] results — Anthropic, OpenAI, and Azure
    /// emit one chunk per SSE delta; Bedrock and Ollama emit a single
    /// chunk wrapping their non-streaming response.
    ///
    /// **Retry semantics differ from `chat`.** This method does *one*
    /// connect attempt — once bytes are flowing, transient errors
    /// surface as `Err` items in the stream rather than triggering a
    /// fresh request, because we'd otherwise risk re-billing the user
    /// for content they already received. Rate-limit / overloaded
    /// responses (429 / 529) are returned synchronously from this
    /// `Result<ChatStream>` so callers can decide whether to retry
    /// the open.
    ///
    /// Scheduler tickets are taken on connect and released when the
    /// stream is dropped — see `chat` for the retry-loop variant.
    pub async fn chat_stream(&self, system: &str, user: &str) -> Result<ChatStream> {
        // Take a scheduler ticket if attached; we don't currently
        // record per-stream throughput because we can't observe usage
        // mid-flight without reading the response. The ticket is
        // released when this future drops.
        let _opt_ticket = if let Some(ref sched) = self.scheduler {
            Some(sched.wait_for_slot().await)
        } else {
            None
        };
        self.provider.chat_stream(system, user).await
    }

    /// Extract knowledge from a chunk of text.
    ///
    /// If the provider signals truncation, returns `Error::TruncatedOutput`
    /// so the caller can split the chunk and retry each half.
    ///
    /// **Rate-limit handling:** rate-limit errors (429, throttle, etc.)
    /// get up to `max_retries * 2` attempts with exponential backoff
    /// (1s → 2s → 4s → …, capped at 60s) plus random jitter.
    /// Non-rate-limit errors use the standard `max_retries` with shorter
    /// delays. When a rate-limit is detected and `AdaptiveConcurrency` is
    /// attached, the effective concurrency is also halved.
    pub async fn extract(&self, content: &str, context: &str) -> Result<ExtractionResult> {
        let user_prompt = prompts::build_extraction_prompt(content, context);
        self.extract_prompt(user_prompt).await
    }

    /// Extract knowledge with graph-primed context injected into the prompt.
    ///
    /// When `known_entities_section` is non-empty it is embedded in the prompt
    /// before the source content so the LLM can ground new extractions against
    /// existing entities rather than inventing names.  Falls back to the plain
    /// prompt when the section is empty (i.e. first-run, empty graph).
    pub async fn extract_with_graph_context(
        &self,
        content: &str,
        context: &str,
        known_entities_section: &str,
    ) -> Result<ExtractionResult> {
        let user_prompt =
            prompts::build_extraction_prompt_with_context(content, context, known_entities_section);
        self.extract_prompt(user_prompt).await
    }

    /// Send a pre-built batch prompt and return the raw LLM response text.
    ///
    /// The caller builds the prompt via `batch::build_batch_prompt` and parses
    /// the result via `batch::parse_batch_response`. This method handles only
    /// transport: retry, rate-limit backoff, and throughput scheduling.
    ///
    /// On truncation: returns the partial text rather than failing — the batch
    /// parser handles missing chunk sections gracefully.
    pub async fn extract_batch_raw(&self, batch_prompt: &str) -> Result<String> {
        let mut last_error = None;
        let max_rl_retries = self.max_retries * 2;
        let mut rl_attempts: u32 = 0;
        let mut normal_attempts: u32 = 0;

        loop {
            if normal_attempts >= self.max_retries && rl_attempts >= max_rl_retries {
                break;
            }

            let opt_ticket = if let Some(ref sched) = self.scheduler {
                Some(sched.wait_for_slot().await)
            } else {
                None
            };

            let chat_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(600),
                self.provider.chat(prompts::SYSTEM_PROMPT, batch_prompt),
            )
            .await;

            let provider_result = match chat_result {
                Ok(r) => r,
                Err(_) => {
                    normal_attempts += 1;
                    tracing::warn!(
                        attempt = normal_attempts,
                        max = self.max_retries,
                        "batch LLM call timed out after 600s, retrying..."
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
            };

            match provider_result {
                Ok(output) => {
                    let tokens = (prompts::SYSTEM_PROMPT.len()
                        + batch_prompt.len()
                        + output.text.len()) as u64
                        / 4;
                    if let (Some(sched), Some(ticket)) = (&self.scheduler, opt_ticket) {
                        sched.record_success(tokens, &output.limits, ticket).await;
                    }
                    if output.truncated {
                        tracing::warn!(
                            "batch LLM output truncated — partial results will be used by parser"
                        );
                    }
                    return Ok(output.text);
                }
                Err(e) if e.is_rate_limited() => {
                    rl_attempts += 1;
                    if let (Some(sched), Some(ticket)) = (&self.scheduler, opt_ticket) {
                        sched.record_throttle(ticket);
                    }
                    let provider_hint = match &e {
                        Error::RateLimited { retry_after_ms, .. } if *retry_after_ms > 0 => {
                            *retry_after_ms
                        }
                        _ => 0,
                    };
                    let backoff_ms =
                        (1000u64 * 2u64.pow(rl_attempts.saturating_sub(1))).min(60_000);
                    let base_delay = if provider_hint > 0 {
                        provider_hint
                    } else {
                        backoff_ms
                    };
                    let jitter = (base_delay as f64 * 0.25 * (rand_jitter() - 0.5)) as i64;
                    let delay = (base_delay as i64 + jitter).max(500) as u64;
                    tracing::warn!(
                        attempt = rl_attempts,
                        max = max_rl_retries,
                        delay_ms = delay,
                        "batch rate-limited — backing off"
                    );
                    last_error = Some(e);
                    if rl_attempts >= max_rl_retries {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Err(e) => {
                    normal_attempts += 1;
                    tracing::warn!(
                        attempt = normal_attempts,
                        max = self.max_retries,
                        "batch LLM request failed: {e}"
                    );
                    last_error = Some(e);
                    if normal_attempts >= self.max_retries {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * 2u64.pow(normal_attempts.saturating_sub(1)),
                    ))
                    .await;
                }
            }
        }

        Err(last_error.unwrap_or(Error::Extraction {
            source_id: "batch".into(),
            message: "all batch retry attempts exhausted".into(),
        }))
    }

    /// Send a raw chat completion with a custom system prompt.
    ///
    /// Unlike `extract()`, this does NOT parse the response as knowledge JSON.
    /// Used by the ReAct synthesis layer to generate natural language answers
    /// from retrieved memory notes. Same retry/rate-limit behaviour as `extract`.
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let max_rl_retries = self.max_retries * 2;
        let mut rl_attempts: u32 = 0;
        let mut normal_attempts: u32 = 0;
        let mut last_error: Option<Error> = None;

        loop {
            if normal_attempts >= self.max_retries && rl_attempts >= max_rl_retries {
                break;
            }

            let opt_ticket = if let Some(ref sched) = self.scheduler {
                Some(sched.wait_for_slot().await)
            } else {
                None
            };

            let chat_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(45),
                self.provider.chat(system, user),
            )
            .await;

            let provider_result = match chat_result {
                Ok(r) => r,
                Err(_) => {
                    // Timed out — count as a transient error and retry.
                    normal_attempts += 1;
                    tracing::warn!(
                        "LLM chat timed out after 45s, retrying ({normal_attempts}/{})...",
                        self.max_retries
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    continue;
                }
            };

            match provider_result {
                Ok(output) => {
                    if output.truncated {
                        return Err(Error::TruncatedOutput {
                            provider: self.provider.provider_name().to_string(),
                            model: self.provider.model_name().to_string(),
                        });
                    }
                    let tokens = (system.len() + user.len() + output.text.len()) as u64 / 4;
                    if let (Some(sched), Some(ticket)) = (&self.scheduler, opt_ticket) {
                        sched.record_success(tokens, &output.limits, ticket).await;
                    }
                    return Ok(output.text);
                }
                Err(e) if e.is_rate_limited() => {
                    rl_attempts += 1;
                    if let (Some(sched), Some(ticket)) = (&self.scheduler, opt_ticket) {
                        sched.record_throttle(ticket);
                    }
                    let delay = match &e {
                        Error::RateLimited { retry_after_ms, .. } if *retry_after_ms > 0 => {
                            *retry_after_ms
                        }
                        _ => (1000u64 * 2u64.pow(rl_attempts.saturating_sub(1))).min(60_000),
                    };
                    last_error = Some(e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                }
                Err(e) => {
                    normal_attempts += 1;
                    let delay = (500u64 * 2u64.pow(normal_attempts.saturating_sub(1))).min(10_000);
                    last_error = Some(e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                }
            }
        }

        Err(last_error.unwrap_or(Error::Extraction {
            source_id: "chat".into(),
            message: "all retry attempts exhausted".into(),
        }))
    }

    /// Tool-calling chat.
    ///
    /// One LLM turn: the caller passes the running conversation as a
    /// `&[ChatMessage]` history, plus the catalog of [`Tool`]s the
    /// model may call. The response is either a terminal
    /// [`ToolUseResponse::Text`] (we're done) or a
    /// [`ToolUseResponse::ToolCalls`] (model wants the caller to
    /// dispatch N tools and feed the results back as
    /// [`ChatMessage::ToolResults`] in the next call).
    ///
    /// All five providers are wired natively (Anthropic Messages,
    /// OpenAI Chat Completions, Azure OpenAI, Bedrock Converse,
    /// Ollama OpenAI-compat). Tool support is model-dependent on
    /// Bedrock (Claude / Nova) and Ollama (llama3.1, mistral-nemo,
    /// command-r, etc.); when the model rejects the tools field the
    /// upstream's 4xx is surfaced verbatim — we do not pre-flight
    /// against a model whitelist because the matrix changes too
    /// fast for that to be honest.
    ///
    /// Retries on rate-limit and transient errors with the same
    /// exponential-backoff shape as [`chat`](Self::chat). Truncation
    /// (`finish_reason == "length"` / `stop_reason == "max_tokens"`)
    /// produces an [`Error::TruncatedOutput`] rather than a partial
    /// response — callers can choose to re-prompt with smaller
    /// context.
    pub async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse> {
        let max_rl_retries = self.max_retries * 2;
        let mut rl_attempts: u32 = 0;
        let mut normal_attempts: u32 = 0;
        let mut last_error: Option<Error> = None;

        loop {
            if normal_attempts >= self.max_retries && rl_attempts >= max_rl_retries {
                break;
            }

            let opt_ticket = if let Some(ref sched) = self.scheduler {
                Some(sched.wait_for_slot().await)
            } else {
                None
            };

            let chat_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(60),
                self.provider
                    .chat_with_tools(system, messages, tools, tool_choice),
            )
            .await;

            let provider_result = match chat_result {
                Ok(r) => r,
                Err(_) => {
                    normal_attempts += 1;
                    tracing::warn!(
                        "LLM chat_with_tools timed out after 60s, retrying ({normal_attempts}/{})...",
                        self.max_retries
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    continue;
                }
            };

            match provider_result {
                Ok(response) => {
                    // Truncation handling: bail rather than return a
                    // partial. The caller can shorten context and retry.
                    if let ToolUseResponse::Text { truncated: true, .. } = &response {
                        return Err(Error::TruncatedOutput {
                            provider: self.provider.provider_name().to_string(),
                            model: self.provider.model_name().to_string(),
                        });
                    }
                    if let (Some(sched), Some(ticket)) = (&self.scheduler, opt_ticket) {
                        let limits = match &response {
                            ToolUseResponse::Text { limits, .. } => limits.clone(),
                            ToolUseResponse::ToolCalls { limits, .. } => limits.clone(),
                        };
                        // Best-effort token estimate for the scheduler.
                        // System + serialized messages + serialized tools
                        // is a stable upper bound on input; output we
                        // estimate from the response payload.
                        let approx_tokens =
                            ((system.len()
                                + messages
                                    .iter()
                                    .map(estimated_message_chars)
                                    .sum::<usize>()
                                + tools.iter().map(|t| t.name.len() + t.description.len()).sum::<usize>())
                                / 4) as u64;
                        sched.record_success(approx_tokens, &limits, ticket).await;
                    }
                    return Ok(response);
                }
                Err(e) if e.is_rate_limited() => {
                    rl_attempts += 1;
                    if let (Some(sched), Some(ticket)) = (&self.scheduler, opt_ticket) {
                        sched.record_throttle(ticket);
                    }
                    let delay = match &e {
                        Error::RateLimited { retry_after_ms, .. } if *retry_after_ms > 0 => {
                            *retry_after_ms
                        }
                        _ => (1000u64 * 2u64.pow(rl_attempts.saturating_sub(1))).min(60_000),
                    };
                    last_error = Some(e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                }
                Err(e) => {
                    normal_attempts += 1;
                    let delay = (500u64 * 2u64.pow(normal_attempts.saturating_sub(1))).min(10_000);
                    last_error = Some(e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                }
            }
        }

        Err(last_error.unwrap_or(Error::Extraction {
            source_id: "chat_with_tools".into(),
            message: "all retry attempts exhausted".into(),
        }))
    }

    /// Core retry/rate-limit loop shared by `extract` and
    /// `extract_with_graph_context`.  Accepts a fully-built user prompt string
    /// so callers can vary the prompt without duplicating retry logic.
    async fn extract_prompt(&self, user_prompt: String) -> Result<ExtractionResult> {
        let mut last_error = None;

        // Rate-limit errors get double the retries.
        let max_rl_retries = self.max_retries * 2;
        let mut rl_attempts: u32 = 0;
        let mut normal_attempts: u32 = 0;

        loop {
            // Stop if we've exhausted both budgets.
            if normal_attempts >= self.max_retries && rl_attempts >= max_rl_retries {
                break;
            }

            // Gate every send through the throughput scheduler.
            // This is the pre-emptive layer — prevents 429s from ever occurring.
            // The ticket tracks in-flight count via RAII: Drop decrements automatically
            // no matter which path (success, error, truncation) exits the match below.
            let opt_ticket = if let Some(ref sched) = self.scheduler {
                Some(sched.wait_for_slot().await)
            } else {
                None
            };

            let chat_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(600),
                self.provider.chat(prompts::SYSTEM_PROMPT, &user_prompt),
            )
            .await;

            let provider_output = match chat_result {
                Ok(r) => r,
                Err(_) => {
                    normal_attempts += 1;
                    tracing::warn!(
                        attempt = normal_attempts,
                        max = self.max_retries,
                        "LLM extraction call timed out after 600s, retrying..."
                    );
                    if normal_attempts >= self.max_retries {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * 2u64.pow(normal_attempts.saturating_sub(1)),
                    ))
                    .await;
                    continue;
                }
            };

            match provider_output {
                Ok(output) => {
                    if output.truncated {
                        return Err(Error::TruncatedOutput {
                            provider: self.provider.provider_name().to_string(),
                            model: self.provider.model_name().to_string(),
                        });
                    }

                    // Record success: update rolling token average and recalibrate send rate.
                    // Include system prompt in the estimate — on TPM-bound providers,
                    // missing it makes the scheduler run hotter than it thinks.
                    let tokens = (prompts::SYSTEM_PROMPT.len()
                        + user_prompt.len()
                        + output.text.len()) as u64
                        / 4;
                    if let (Some(sched), Some(ticket)) = (&self.scheduler, opt_ticket) {
                        sched.record_success(tokens, &output.limits, ticket).await;
                    }

                    match parse_extraction_result(&output.text) {
                        Ok(result) => {
                            return Ok(result);
                        }
                        Err(e) => {
                            normal_attempts += 1;
                            tracing::warn!(
                                attempt = normal_attempts,
                                max = self.max_retries,
                                "failed to parse LLM response: {e}"
                            );
                            last_error = Some(e);
                            if normal_attempts >= self.max_retries {
                                break;
                            }
                        }
                    }
                }
                Err(e) if e.is_rate_limited() => {
                    rl_attempts += 1;

                    // Safety net: scheduler should have prevented this, but providers
                    // can be inconsistent. Double the send interval and halve concurrency.
                    if let (Some(sched), Some(ticket)) = (&self.scheduler, opt_ticket) {
                        sched.record_throttle(ticket);
                    }

                    // Get provider-suggested delay, or compute our own.
                    let provider_hint = match &e {
                        Error::RateLimited { retry_after_ms, .. } if *retry_after_ms > 0 => {
                            *retry_after_ms
                        }
                        _ => 0,
                    };

                    // Exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s, capped at 60s.
                    let backoff_ms =
                        (1000u64 * 2u64.pow(rl_attempts.saturating_sub(1))).min(60_000);
                    let base_delay = if provider_hint > 0 {
                        provider_hint
                    } else {
                        backoff_ms
                    };

                    // Add jitter: ±25% random spread to prevent thundering herd.
                    let jitter = (base_delay as f64 * 0.25 * (rand_jitter() - 0.5)) as i64;
                    let delay = (base_delay as i64 + jitter).max(500) as u64;

                    tracing::warn!(
                        attempt = rl_attempts,
                        max = max_rl_retries,
                        delay_ms = delay,
                        "rate-limited by {} — backing off",
                        self.provider.provider_name()
                    );

                    last_error = Some(e);
                    if rl_attempts >= max_rl_retries {
                        break;
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Err(e) => {
                    normal_attempts += 1;
                    tracing::warn!(
                        attempt = normal_attempts,
                        max = self.max_retries,
                        "LLM request failed: {e}"
                    );
                    last_error = Some(e);
                    if normal_attempts >= self.max_retries {
                        break;
                    }

                    // Short backoff for non-rate-limit errors.
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * 2u64.pow(normal_attempts.saturating_sub(1)),
                    ))
                    .await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::Extraction {
            source_id: String::new(),
            message: "all retries exhausted".to_string(),
        }))
    }
}

/// Cheap pseudo-random jitter in [0.0, 2.0) — no external crate needed.
/// Uses the current time's nanosecond component as entropy source.
fn rand_jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Map nanoseconds to [0.0, 2.0)
    (nanos as f64 / u32::MAX as f64) * 2.0
}

// ── Response parsing ─────────────────────────────────────────────

fn parse_extraction_result(text: &str) -> Result<ExtractionResult> {
    if let Ok(result) = serde_json::from_str::<ExtractionResult>(text) {
        return Ok(result);
    }

    let json_str = extract_json_from_text(text);
    if let Ok(result) = serde_json::from_str::<ExtractionResult>(json_str) {
        return Ok(result);
    }

    // Some models (Nova, older Claude) emit trailing commas which are invalid JSON.
    // Strip them and retry before giving up.
    let cleaned = strip_trailing_commas(json_str);
    if let Ok(result) = serde_json::from_str::<ExtractionResult>(&cleaned) {
        return Ok(result);
    }

    // Attempt 4: repair bare array items (LLM forgot {} around objects)
    let repaired = repair_bare_array_items(&cleaned);
    serde_json::from_str::<ExtractionResult>(&repaired).map_err(|e| Error::StructuredOutput {
        message: format!(
            "failed to parse extraction result: {e}\nRaw response: {}",
            &text[..text.len().min(200)]
        ),
    })
}

/// Remove trailing commas before `]` or `}` — handles non-standard JSON from some LLMs.
/// Pure char scan, no regex dependency.
fn strip_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            // Peek ahead past whitespace to see if the next token closes an array/object.
            let mut j = i + 1;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b']' | b'}') {
                i += 1; // skip the comma
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Repair the specific malformation where LLMs omit `{}` around array items.
///
/// Handles:
/// ```text
/// "claims": ["statement": "...", "claim_type": "fact"]
/// ```
/// Repairs to:
/// ```text
/// "claims": [{"statement": "...", "claim_type": "fact"}]
/// ```
///
/// Uses the known first-field names of our schema to detect object boundaries.
fn repair_bare_array_items(s: &str) -> String {
    // First-field of each array item type in ExtractionResult.
    // A new object starts whenever one of these appears after a comma at depth 0.
    const BOUNDARY_KEYS: &[&str] = &[r#""statement":"#, r#""name":"#, r#""from_entity":"#];

    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 128);
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'[' {
            // Check if the first non-whitespace content after '[' is a bare key (not '{')
            let after = skip_whitespace(bytes, i + 1);
            let remaining = s.get(after..).unwrap_or("");
            let is_bare = BOUNDARY_KEYS.iter().any(|k| remaining.starts_with(k));

            if is_bare {
                // Find the matching ']'
                if let Some(close_rel) = find_close_bracket(&bytes[i..]) {
                    let inner_start = i + 1;
                    let inner_end = i + close_rel - 1; // content between '[' and ']'
                    let inner = s.get(inner_start..inner_end).unwrap_or("");

                    // Split inner content into individual object strings
                    let objects = split_bare_objects(inner, BOUNDARY_KEYS);

                    out.push('[');
                    for (idx, obj) in objects.iter().enumerate() {
                        if idx > 0 {
                            out.push_str(", ");
                        }
                        let trimmed = obj.trim().trim_end_matches(',');
                        out.push('{');
                        out.push_str(trimmed);
                        out.push('}');
                    }
                    out.push(']');

                    i += close_rel; // advance past ']'
                    continue;
                }
            }
        }

        out.push(bytes[i] as char);
        i += 1;
    }

    out
}

fn skip_whitespace(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Returns the length from the opening `[` up to and including the matching `]`.
fn find_close_bracket(bytes: &[u8]) -> Option<usize> {
    debug_assert_eq!(bytes.first(), Some(&b'['));
    let mut depth = 0i32;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => {
                i += 2;
                continue;
            }
            b'"' => {
                in_string = !in_string;
            }
            b'[' | b'{' if !in_string => depth += 1,
            b']' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b'}' if !in_string => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split the flat content of a bare array into individual object string slices.
/// Objects are delimited by a comma followed by one of the known boundary keys at depth 0.
fn split_bare_objects<'a>(inner: &'a str, boundary_keys: &[&str]) -> Vec<&'a str> {
    let bytes = inner.as_bytes();
    let mut objects: Vec<&str> = Vec::new();
    let mut current_start = 0usize;
    let mut i = 0usize;
    let mut in_string = false;
    let mut depth = 0i32;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => {
                i += 2;
                continue;
            }
            b'"' => {
                in_string = !in_string;
            }
            b'{' | b'[' if !in_string => depth += 1,
            b'}' | b']' if !in_string => depth -= 1,
            b',' if !in_string && depth == 0 => {
                // Check if what follows (after whitespace) is a boundary key
                let after = skip_whitespace(bytes, i + 1);
                let remaining = inner.get(after..).unwrap_or("");
                if boundary_keys.iter().any(|k| remaining.starts_with(k)) {
                    objects.push(inner[current_start..i].trim());
                    current_start = after; // new object starts after the whitespace
                    i = after;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }

    // Last object
    let last = inner[current_start..].trim();
    if !last.is_empty() {
        objects.push(last);
    }

    objects
}

fn extract_json_from_text(text: &str) -> &str {
    let text = text.trim();

    if let Some(start) = text.find("```json") {
        let content_start = start + 7;
        if let Some(end) = text[content_start..].find("```") {
            return text[content_start..content_start + end].trim();
        }
    }

    if let Some(start) = text.find("```") {
        let content_start = start + 3;
        let content_start = text[content_start..]
            .find('\n')
            .map(|i| content_start + i + 1)
            .unwrap_or(content_start);
        if let Some(end) = text[content_start..].find("```") {
            return text[content_start..content_start + end].trim();
        }
    }

    if let Some(start) = text.find('{')
        && let Some(end) = text.rfind('}')
    {
        return &text[start..=end];
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_max_tokens_haiku_45() {
        assert_eq!(
            model_max_output_tokens("eu.anthropic.claude-haiku-4-5-20251001-v1:0"),
            64_000
        );
        assert_eq!(model_max_output_tokens("claude-haiku-4-5-20251001"), 64_000);
    }

    #[test]
    fn model_max_tokens_haiku_3() {
        assert_eq!(model_max_output_tokens("claude-3-haiku-20240307"), 4_096);
        assert_eq!(
            model_max_output_tokens("anthropic.claude-3-haiku-20240307-v1:0"),
            4_096
        );
    }

    #[test]
    fn model_max_tokens_sonnet() {
        assert_eq!(model_max_output_tokens("claude-sonnet-4-6"), 8_192);
        assert_eq!(model_max_output_tokens("claude-3-5-sonnet-20241022"), 8_192);
    }

    #[test]
    fn model_max_tokens_gpt4o() {
        assert_eq!(model_max_output_tokens("gpt-4o"), 16_384);
        assert_eq!(model_max_output_tokens("gpt-4o-mini"), 16_384);
    }

    #[test]
    fn model_max_tokens_unknown_falls_back() {
        assert_eq!(model_max_output_tokens("some-unknown-model-v99"), 8_192);
    }

    // ── model_context_window ──────────────────────────────────────

    #[test]
    fn context_window_claude_sonnet() {
        assert_eq!(model_context_window("claude-sonnet-4-6"), 1_000_000);
        assert_eq!(model_context_window("claude-opus-4-6"), 1_000_000);
    }

    #[test]
    fn context_window_claude_haiku() {
        assert_eq!(model_context_window("claude-haiku-4-5-20251001"), 200_000);
        assert_eq!(model_context_window("claude-3-haiku-20240307"), 200_000);
    }

    #[test]
    fn context_window_gpt41_family() {
        // gpt-4.1 and gpt-4-1 (Azure deployment naming)
        assert_eq!(model_context_window("gpt-4.1"), 300_000);
        assert_eq!(model_context_window("gpt-4.1-mini"), 300_000);
        assert_eq!(model_context_window("gpt-4-1-mini"), 300_000);
    }

    #[test]
    fn context_window_gpt4o_family() {
        assert_eq!(model_context_window("gpt-4o"), 128_000);
        assert_eq!(model_context_window("gpt-4o-mini"), 128_000);
        assert_eq!(model_context_window("gpt-4-turbo"), 128_000);
    }

    #[test]
    fn context_window_nova_models() {
        assert_eq!(model_context_window("amazon.nova-micro-v1:0"), 128_000);
        assert_eq!(model_context_window("amazon.nova-lite-v1:0"), 300_000);
        assert_eq!(model_context_window("amazon.nova-pro-v1:0"), 300_000);
    }

    #[test]
    fn context_window_groq_llama() {
        assert_eq!(model_context_window("llama-3.1-8b-instant"), 131_072);
        assert_eq!(model_context_window("llama-3.3-70b-versatile"), 131_072);
        assert_eq!(
            model_context_window("meta-llama/llama-3.1-8b-instruct"),
            131_072
        );
    }

    #[test]
    fn context_window_deepseek() {
        assert_eq!(model_context_window("deepseek-chat"), 128_000);
        assert_eq!(model_context_window("deepseek-coder"), 128_000);
    }

    #[test]
    fn context_window_unknown_falls_back() {
        assert_eq!(model_context_window("some-unknown-v99"), 32_768);
    }

    // ── model_batch_size ─────────────────────────────────────────

    #[test]
    fn batch_size_azure_gpt41_mini() {
        // gpt-4.1-mini: context=300K, output=32K, chunk=2000
        // input_safe = (300000*0.8 - 700) / 2000 = (240000-700)/2000 = 119
        // output_safe = 32768 / 500 = 65
        // min(119, 65, 64) = 64
        let n = model_batch_size("azure", "gpt-4-1-mini", 2000);
        assert_eq!(n, 64, "azure gpt-4.1-mini must reach the hard cap of 64");
    }

    #[test]
    fn batch_size_gpt4o() {
        // context=128K, output=16K, chunk=2000
        // input_safe = (102400-700)/2000 = 50
        // output_safe = 16384/500 = 32
        // min(50, 32, 64) = 32
        let n = model_batch_size("openai", "gpt-4o", 2000);
        assert_eq!(n, 32);
    }

    #[test]
    fn batch_size_claude_sonnet() {
        // context=1M, output=8192, chunk=2000
        // output_safe = 8192/500 = 16
        let n = model_batch_size("anthropic", "claude-sonnet-4-6", 2000);
        assert_eq!(n, 16);
    }

    #[test]
    fn batch_size_claude_haiku_45() {
        // context=200K, output=64K, chunk=2000
        // input_safe = (160000-700)/2000 = 79
        // output_safe = 64000/500 = 128
        // min(79, 128, 64) = 64 (hits hard cap)
        let n = model_batch_size("anthropic", "claude-haiku-4-5-20251001", 2000);
        assert_eq!(n, 64);
    }

    #[test]
    fn batch_size_nova_micro_bedrock_capped() {
        // Nova micro: context=128K, output=5120, chunk=2000
        // output_safe = 5120/500 = 10
        // input_safe = (102400-700)/2000 = 50
        // min(50, 10) = 10, then bedrock cap (n.min(8)) drops to 8.
        // The bedrock cap exists because >128K-token batches stall under
        // default account-level throughput (TCP open, 0% CPU, no progress).
        let n = model_batch_size("bedrock", "amazon.nova-micro-v1:0", 2000);
        assert_eq!(n, 8, "bedrock cap must clamp nova-micro batch to 8");
    }

    #[test]
    fn batch_size_groq_llama() {
        // llama-3.1-8b: context=131K, output=8192, chunk=2000
        // input_safe = (104857-700)/2000 = 52
        // output_safe = 8192/500 = 16
        let n = model_batch_size("groq", "llama-3.1-8b-instant", 2000);
        assert_eq!(n, 16);
    }

    #[test]
    fn batch_size_perplexity_always_one() {
        let n = model_batch_size("perplexity", "sonar-pro", 2000);
        assert_eq!(
            n, 1,
            "perplexity sonar must always return 1 — search-grounded"
        );
    }

    #[test]
    fn batch_size_ollama_default_one() {
        let n = model_batch_size("ollama", "llama3", 2000);
        assert_eq!(n, 1, "ollama default num_ctx=2048 fits only 1 chunk");
    }

    #[test]
    fn batch_size_never_zero() {
        // Even tiny context must produce at least 1
        let n = model_batch_size("ollama", "tiny-model", 2000);
        assert!(n >= 1, "batch size must never be zero");
    }

    #[test]
    fn batch_size_hard_cap_64() {
        // Very large context must be capped at 64
        let n = model_batch_size("anthropic", "claude-sonnet-4-6-future", 100);
        assert!(n <= 64, "batch size must never exceed 64");
    }

    #[test]
    fn resolve_key_uses_default_env_when_config_is_none() {
        unsafe {
            std::env::set_var("TEST_DEFAULT_KEY", "mykey");
        }
        let result = resolve_key(None, "TEST_DEFAULT_KEY").unwrap();
        assert_eq!(result, "mykey");
        unsafe {
            std::env::remove_var("TEST_DEFAULT_KEY");
        }
    }

    #[test]
    fn resolve_key_uses_config_env_when_set() {
        unsafe {
            std::env::set_var("MY_CUSTOM_ENV", "customkey");
        }
        let cfg = thinkingroot_core::config::ProviderConfig {
            api_key_env: Some("MY_CUSTOM_ENV".to_string()),
            api_key: None,
            base_url: None,
            default_model: None,
        };
        let result = resolve_key(Some(&cfg), "IGNORED_DEFAULT").unwrap();
        assert_eq!(result, "customkey");
        unsafe {
            std::env::remove_var("MY_CUSTOM_ENV");
        }
    }

    #[test]
    fn resolve_base_url_returns_default_when_config_has_none() {
        let result = resolve_base_url(None, "https://default.example.com");
        assert_eq!(result, "https://default.example.com");
    }

    #[test]
    fn resolve_base_url_returns_config_url_when_set() {
        let cfg = thinkingroot_core::config::ProviderConfig {
            api_key_env: None,
            api_key: None,
            base_url: Some("https://custom.example.com".to_string()),
            default_model: None,
        };
        let result = resolve_base_url(Some(&cfg), "https://default.example.com");
        assert_eq!(result, "https://custom.example.com");
    }

    #[test]
    fn openai_provider_strips_trailing_v1_from_base_url() {
        // Providers like OpenRouter store "https://host/api/v1" in config.
        // OpenAiProvider must strip the /v1 so chat() doesn't produce a double /v1.
        let p = OpenAiProvider::new("key", "model", "https://openrouter.ai/api/v1", "openrouter");
        assert_eq!(p.base_url, "https://openrouter.ai/api");

        let p2 = OpenAiProvider::new("key", "model", "https://api.together.xyz/v1", "together");
        assert_eq!(p2.base_url, "https://api.together.xyz");

        // Providers without /v1 suffix must be unchanged.
        let p3 = OpenAiProvider::new("key", "model", "https://api.openai.com", "openai");
        assert_eq!(p3.base_url, "https://api.openai.com");

        // Groq's /openai path must not be stripped.
        let p4 = OpenAiProvider::new("key", "model", "https://api.groq.com/openai", "groq");
        assert_eq!(p4.base_url, "https://api.groq.com/openai");
    }

    #[test]
    fn resolve_key_falls_back_to_stored_api_key() {
        // When no env var is set but api_key is stored in ProviderConfig, resolve_key must
        // return the stored value — this is the path taken after `root setup` in a fresh shell.
        let env_var = "__TR_TEST_KEY_NOT_SET_7f3a9b__";
        // SAFETY: test-only mutation of env vars; tests using unique names avoid races.
        unsafe {
            std::env::remove_var(env_var);
        }

        let cfg = thinkingroot_core::config::ProviderConfig {
            api_key_env: Some(env_var.to_string()),
            api_key: Some("stored-secret-key".to_string()),
            base_url: None,
            default_model: None,
        };
        let result = resolve_key(Some(&cfg), env_var);
        assert_eq!(result.unwrap(), "stored-secret-key");
    }

    #[test]
    fn resolve_key_env_var_takes_priority_over_stored() {
        let env_var = "__TR_TEST_KEY_SET_9c1d2e__";
        // SAFETY: test-only mutation of env vars; tests using unique names avoid races.
        unsafe {
            std::env::set_var(env_var, "live-env-value");
        }

        let cfg = thinkingroot_core::config::ProviderConfig {
            api_key_env: Some(env_var.to_string()),
            api_key: Some("stored-value".to_string()),
            base_url: None,
            default_model: None,
        };
        let result = resolve_key(Some(&cfg), env_var);
        unsafe {
            std::env::remove_var(env_var);
        }
        assert_eq!(result.unwrap(), "live-env-value");
    }

    #[test]
    fn parse_valid_json() {
        let json = r#"{"claims":[],"entities":[],"relations":[]}"#;
        let result = parse_extraction_result(json).unwrap();
        assert!(result.claims.is_empty());
    }

    #[test]
    fn parse_json_with_trailing_commas() {
        // Some LLMs (Nova, older Claude) emit trailing commas — must not fail.
        let json = "{\"claims\":[],\"entities\":[],\"relations\":[],}";
        let result = parse_extraction_result(json).unwrap();
        assert!(result.claims.is_empty());
    }

    #[test]
    fn parse_json_in_code_block() {
        let text =
            "Here's the result:\n```json\n{\"claims\":[],\"entities\":[],\"relations\":[]}\n```";
        let result = parse_extraction_result(text).unwrap();
        assert!(result.claims.is_empty());
    }

    #[test]
    fn extract_json_from_text_with_preamble() {
        let text =
            "Sure! Here is the extraction:\n\n{\"claims\":[],\"entities\":[],\"relations\":[]}";
        let result = parse_extraction_result(text).unwrap();
        assert!(result.claims.is_empty());
    }

    #[test]
    fn repair_bare_array_single_claim() {
        // LLM forgot {} around the claim object
        let malformed = r#"{
  "claims": [
      "statement": "X is a function",
      "claim_type": "fact",
      "confidence": 0.9,
      "entities": ["X"],
      "source_quote": "fn x()"
  ],
  "entities": [],
  "relations": []
}"#;
        let repaired = repair_bare_array_items(malformed);
        let result: ExtractionResult =
            serde_json::from_str(&repaired).expect("repaired JSON should parse");
        assert_eq!(result.claims.len(), 1);
        assert_eq!(result.claims[0].statement, "X is a function");
    }

    #[test]
    fn repair_bare_array_multiple_claims() {
        // Two claims without {}, split at "statement":
        let malformed = r#"{
  "claims": [
      "statement": "A is a type",
      "claim_type": "definition",
      "confidence": 0.99,
      "entities": ["A"],
      "source_quote": "struct A {}",
      "statement": "B depends on A",
      "claim_type": "dependency",
      "confidence": 0.8,
      "entities": ["B", "A"],
      "source_quote": "use A;"
  ],
  "entities": [],
  "relations": []
}"#;
        let repaired = repair_bare_array_items(malformed);
        let result: ExtractionResult =
            serde_json::from_str(&repaired).expect("repaired JSON should parse");
        assert_eq!(result.claims.len(), 2);
    }

    #[test]
    fn repair_well_formed_json_unchanged() {
        // Properly formed JSON should pass through unchanged
        let good = r#"{"claims": [{"statement": "X", "claim_type": "fact", "confidence": 0.9, "entities": [], "source_quote": null}], "entities": [], "relations": []}"#;
        let repaired = repair_bare_array_items(good);
        assert_eq!(repaired, good);
    }

    #[test]
    fn parse_extraction_result_recovers_from_bare_array() {
        // Full parse_extraction_result pipeline handles the bare-array failure
        let malformed = r#"{
  "claims": [
      "statement": "The engine compiles code",
      "claim_type": "fact",
      "confidence": 0.85,
      "entities": ["engine"],
      "source_quote": "fn compile()"
  ],
  "entities": [
      "name": "engine",
      "entity_type": "system",
      "aliases": [],
      "description": "The extraction engine"
  ],
  "relations": []
}"#;
        let result =
            parse_extraction_result(malformed).expect("parse_extraction_result should recover");
        assert_eq!(result.claims.len(), 1);
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "engine");
    }

    // ── LlmClient::new() unconfigured guard ───────────────────────

    #[tokio::test]
    async fn llm_client_new_fails_when_provider_empty() {
        let config = thinkingroot_core::config::LlmConfig::default();
        // default() now has empty strings — is_configured() = false
        assert!(!config.is_configured());
        let result = LlmClient::new(&config).await;
        assert!(result.is_err());
        let msg = result.err().expect("should be Err").to_string();
        assert!(
            msg.contains("root setup") || msg.contains("No LLM provider"),
            "expected setup hint in error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn llm_client_new_fails_when_model_empty() {
        let config = thinkingroot_core::config::LlmConfig {
            default_provider: "openai".to_string(),
            extraction_model: String::new(),
            compilation_model: String::new(),
            max_concurrent_requests: 5,
            request_timeout_secs: 60,
            providers: thinkingroot_core::config::ProvidersConfig::default(),
        };
        assert!(!config.is_configured());
        let result = LlmClient::new(&config).await;
        assert!(result.is_err());
        let msg = result.err().expect("should be Err").to_string();
        assert!(msg.contains("root setup") || msg.contains("No LLM provider"));
    }

    // ─────────────────────────────────────────────────────────────────
    // S2 — Tool-calling wire-format mappers
    //
    // These tests prove each provider's request body builder + response
    // parser against synthetic JSON. No live API calls. Live integration
    // tests sit in a separate `#[ignore]`-gated module so CI can run
    // them on demand with API keys present, the same convention used
    // for the live Azure / Anthropic SSE tests below.
    // ─────────────────────────────────────────────────────────────────

    fn fixture_search_tool() -> Tool {
        Tool::new(
            "search",
            "Search the knowledge graph for claims related to a query.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Free-text query" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "required": ["query"]
            }),
        )
    }

    fn fixture_create_branch_tool() -> Tool {
        Tool::new(
            "create_branch",
            "Create a new knowledge branch from main.",
            serde_json::json!({
                "type": "object",
                "properties": { "name": {"type": "string"} },
                "required": ["name"]
            }),
        )
    }

    // ── OpenAI shape ────────────────────────────────────────────

    #[test]
    fn openai_tools_array_emits_function_wrapper() {
        let tools = [fixture_search_tool()];
        let arr = openai_tools_array(&tools);
        let arr = arr.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "search");
        assert!(
            arr[0]["function"]["description"]
                .as_str()
                .unwrap()
                .contains("knowledge graph")
        );
        assert_eq!(arr[0]["function"]["parameters"]["type"], "object");
        assert_eq!(
            arr[0]["function"]["parameters"]["required"][0], "query"
        );
    }

    #[test]
    fn openai_tool_choice_maps_each_variant() {
        assert_eq!(openai_tool_choice(&ToolChoice::Auto), serde_json::json!("auto"));
        assert_eq!(openai_tool_choice(&ToolChoice::None), serde_json::json!("none"));
        assert_eq!(
            openai_tool_choice(&ToolChoice::Any),
            serde_json::json!("required")
        );
        assert_eq!(
            openai_tool_choice(&ToolChoice::Named("search".to_string())),
            serde_json::json!({
                "type": "function",
                "function": {"name": "search"}
            })
        );
    }

    #[test]
    fn openai_messages_array_threads_full_tool_use_round_trip() {
        let history = vec![
            ChatMessage::user("find me providers"),
            ChatMessage::assistant_tool_calls(vec![ToolCall {
                id: "call_abc".to_string(),
                name: "search".to_string(),
                input: serde_json::json!({"query": "providers", "limit": 5}),
            }]),
            ChatMessage::tool_results(vec![ToolResult {
                tool_use_id: "call_abc".to_string(),
                content: "Azure, Anthropic, OpenAI".to_string(),
                is_error: false,
            }]),
            ChatMessage::assistant_text("There are three configured providers."),
        ];
        let arr = openai_messages_array("you are helpful", &history);
        // [0] system, [1] user, [2] assistant tool_calls, [3] tool, [4] assistant text
        assert_eq!(arr.len(), 5);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "you are helpful");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[1]["content"], "find me providers");
        assert_eq!(arr[2]["role"], "assistant");
        assert!(arr[2]["content"].is_null());
        assert_eq!(arr[2]["tool_calls"][0]["id"], "call_abc");
        assert_eq!(arr[2]["tool_calls"][0]["function"]["name"], "search");
        // OpenAI requires arguments as a JSON string, not an object.
        let raw_args = arr[2]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments must be a string");
        let parsed: serde_json::Value = serde_json::from_str(raw_args).unwrap();
        assert_eq!(parsed["query"], "providers");
        assert_eq!(parsed["limit"], 5);
        assert_eq!(arr[3]["role"], "tool");
        assert_eq!(arr[3]["tool_call_id"], "call_abc");
        assert_eq!(arr[3]["content"], "Azure, Anthropic, OpenAI");
        assert_eq!(arr[4]["role"], "assistant");
        assert_eq!(arr[4]["content"], "There are three configured providers.");
    }

    #[test]
    fn openai_messages_array_marks_tool_errors_in_content() {
        let history = vec![ChatMessage::tool_results(vec![ToolResult {
            tool_use_id: "call_xyz".to_string(),
            content: "branch already exists".to_string(),
            is_error: true,
        }])];
        let arr = openai_messages_array("sys", &history);
        assert_eq!(arr[1]["role"], "tool");
        assert!(
            arr[1]["content"]
                .as_str()
                .unwrap()
                .starts_with("ERROR: branch already exists")
        );
    }

    #[test]
    fn parse_openai_tool_response_text_branch() {
        let json = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "There are 3 providers."},
                "finish_reason": "stop"
            }]
        });
        let resp =
            parse_openai_tool_response(&json, HeaderRateLimits::default(), "openai").unwrap();
        match resp {
            ToolUseResponse::Text { text, truncated, .. } => {
                assert_eq!(text, "There are 3 providers.");
                assert!(!truncated);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn parse_openai_tool_response_marks_truncated() {
        let json = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "..."},
                "finish_reason": "length"
            }]
        });
        let resp =
            parse_openai_tool_response(&json, HeaderRateLimits::default(), "openai").unwrap();
        match resp {
            ToolUseResponse::Text { truncated, .. } => assert!(truncated),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn parse_openai_tool_response_tool_call_branch() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_001",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": "{\"query\":\"foo\",\"limit\":3}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp =
            parse_openai_tool_response(&json, HeaderRateLimits::default(), "azure").unwrap();
        match resp {
            ToolUseResponse::ToolCalls { calls, text_preamble, .. } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_001");
                assert_eq!(calls[0].name, "search");
                assert_eq!(calls[0].input["query"], "foo");
                assert_eq!(calls[0].input["limit"], 3);
                assert_eq!(text_preamble, "");
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn parse_openai_tool_response_carries_text_preamble_when_present() {
        // Some OpenAI-compat upstreams emit prose alongside tool_calls.
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Let me look that up.",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "search", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp =
            parse_openai_tool_response(&json, HeaderRateLimits::default(), "openai").unwrap();
        match resp {
            ToolUseResponse::ToolCalls { text_preamble, .. } => {
                assert_eq!(text_preamble, "Let me look that up.");
            }
            _ => panic!("expected ToolCalls"),
        }
    }

    #[test]
    fn parse_openai_tool_response_rejects_malformed_arguments() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "x", "arguments": "this is not json"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let err =
            parse_openai_tool_response(&json, HeaderRateLimits::default(), "openai").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not JSON") || msg.contains("not a valid"));
    }

    // ── Anthropic shape ─────────────────────────────────────────

    #[test]
    fn anthropic_tools_array_uses_input_schema_field() {
        let tools = [fixture_search_tool(), fixture_create_branch_tool()];
        let arr = anthropic_tools_array(&tools);
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "search");
        // Anthropic uses `input_schema` (not `parameters`).
        assert_eq!(arr[0]["input_schema"]["type"], "object");
        assert_eq!(arr[1]["name"], "create_branch");
    }

    #[test]
    fn anthropic_tool_choice_maps_each_variant() {
        assert_eq!(
            anthropic_tool_choice(&ToolChoice::Auto),
            serde_json::json!({"type": "auto"})
        );
        assert_eq!(
            anthropic_tool_choice(&ToolChoice::Any),
            serde_json::json!({"type": "any"})
        );
        // None → auto (Anthropic models "no tools" by simply not sending tools).
        assert_eq!(
            anthropic_tool_choice(&ToolChoice::None),
            serde_json::json!({"type": "auto"})
        );
        assert_eq!(
            anthropic_tool_choice(&ToolChoice::Named("search".to_string())),
            serde_json::json!({"type": "tool", "name": "search"})
        );
    }

    #[test]
    fn anthropic_messages_array_threads_tool_use_blocks() {
        let history = vec![
            ChatMessage::user("find providers"),
            ChatMessage::assistant_tool_calls(vec![ToolCall {
                id: "tu_001".to_string(),
                name: "search".to_string(),
                input: serde_json::json!({"query": "providers"}),
            }]),
            ChatMessage::tool_results(vec![ToolResult {
                tool_use_id: "tu_001".to_string(),
                content: "Azure, Anthropic".to_string(),
                is_error: false,
            }]),
        ];
        let arr = anthropic_messages_array(&history);
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "find providers");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[1]["content"][0]["type"], "tool_use");
        assert_eq!(arr[1]["content"][0]["id"], "tu_001");
        assert_eq!(arr[1]["content"][0]["name"], "search");
        // Anthropic input is a JSON object (not stringified).
        assert_eq!(arr[1]["content"][0]["input"]["query"], "providers");
        assert_eq!(arr[2]["role"], "user");
        assert_eq!(arr[2]["content"][0]["type"], "tool_result");
        assert_eq!(arr[2]["content"][0]["tool_use_id"], "tu_001");
        assert_eq!(arr[2]["content"][0]["content"], "Azure, Anthropic");
        assert_eq!(arr[2]["content"][0]["is_error"], false);
    }

    #[test]
    fn parse_anthropic_tool_response_text_only() {
        let json = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "There are 3 providers."}]
        });
        let resp = parse_anthropic_tool_response(&json, HeaderRateLimits::default()).unwrap();
        match resp {
            ToolUseResponse::Text { text, truncated, .. } => {
                assert_eq!(text, "There are 3 providers.");
                assert!(!truncated);
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn parse_anthropic_tool_response_max_tokens_marks_truncated() {
        let json = serde_json::json!({
            "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "long response..."}]
        });
        let resp = parse_anthropic_tool_response(&json, HeaderRateLimits::default()).unwrap();
        match resp {
            ToolUseResponse::Text { truncated, .. } => assert!(truncated),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn parse_anthropic_tool_response_mixed_text_and_tool_use() {
        // Anthropic routinely emits a brief preamble alongside tool_use.
        let json = serde_json::json!({
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "Let me search."},
                {
                    "type": "tool_use",
                    "id": "tu_42",
                    "name": "search",
                    "input": {"query": "foo", "limit": 5}
                }
            ]
        });
        let resp = parse_anthropic_tool_response(&json, HeaderRateLimits::default()).unwrap();
        match resp {
            ToolUseResponse::ToolCalls { calls, text_preamble, .. } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "tu_42");
                assert_eq!(calls[0].name, "search");
                assert_eq!(calls[0].input["query"], "foo");
                assert_eq!(text_preamble, "Let me search.");
            }
            _ => panic!("expected ToolCalls"),
        }
    }

    #[test]
    fn parse_anthropic_tool_response_handles_multiple_tool_calls() {
        // Anthropic supports parallel tool calls in one response.
        let json = serde_json::json!({
            "stop_reason": "tool_use",
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": "search", "input": {"q": "a"}},
                {"type": "tool_use", "id": "tu_2", "name": "search", "input": {"q": "b"}}
            ]
        });
        let resp = parse_anthropic_tool_response(&json, HeaderRateLimits::default()).unwrap();
        match resp {
            ToolUseResponse::ToolCalls { calls, .. } => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].id, "tu_1");
                assert_eq!(calls[1].id, "tu_2");
            }
            _ => panic!("expected ToolCalls"),
        }
    }

    // ── Bedrock shape (smithy Document round-trip) ──────────────

    #[test]
    fn json_to_document_round_trips_primitives() {
        let cases = vec![
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(false),
            serde_json::json!(42_u64),
            serde_json::json!(-7_i64),
            serde_json::json!(3.14_f64),
            serde_json::json!(""),
            serde_json::json!("hello"),
        ];
        for case in cases {
            let doc = json_to_document(&case);
            let back = document_to_json(&doc);
            assert_eq!(back, case, "round-trip mismatch on {case}");
        }
    }

    #[test]
    fn json_to_document_round_trips_arrays_and_objects() {
        let original = serde_json::json!({
            "query": "providers",
            "limit": 5,
            "tags": ["a", "b", "c"],
            "nested": {
                "key": "value",
                "flag": false,
                "items": [1, 2, 3]
            }
        });
        let doc = json_to_document(&original);
        let back = document_to_json(&doc);
        assert_eq!(back, original);
    }

    #[test]
    fn document_to_json_preserves_unsigned_floats() {
        use aws_smithy_types::{Document, Number};
        let doc = Document::Number(Number::Float(2.71828));
        let v = document_to_json(&doc);
        let f = v.as_f64().expect("expected f64");
        assert!((f - 2.71828).abs() < 1e-9);
    }

    // ── ChatMessage helper constructors ─────────────────────────

    #[test]
    fn chat_message_constructors_produce_expected_variants() {
        match ChatMessage::user("hi") {
            ChatMessage::User(s) => assert_eq!(s, "hi"),
            _ => panic!("expected User"),
        }
        match ChatMessage::assistant_text("hello") {
            ChatMessage::AssistantText(s) => assert_eq!(s, "hello"),
            _ => panic!("expected AssistantText"),
        }
        let calls = vec![ToolCall {
            id: "c".into(),
            name: "n".into(),
            input: serde_json::json!({}),
        }];
        match ChatMessage::assistant_tool_calls(calls.clone()) {
            ChatMessage::AssistantToolCalls(v) => assert_eq!(v, calls),
            _ => panic!("expected AssistantToolCalls"),
        }
        let results = vec![ToolResult {
            tool_use_id: "c".into(),
            content: "ok".into(),
            is_error: false,
        }];
        match ChatMessage::tool_results(results) {
            ChatMessage::ToolResults(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].tool_use_id, "c");
            }
            _ => panic!("expected ToolResults"),
        }
    }

    #[test]
    fn tool_choice_default_is_auto() {
        assert!(matches!(ToolChoice::default(), ToolChoice::Auto));
    }

    #[test]
    fn estimated_message_chars_counts_each_variant() {
        // Baseline floor — this is a budget estimate, not a precise count.
        let m1 = ChatMessage::user("hello");
        assert!(estimated_message_chars(&m1) >= 5);
        let m2 = ChatMessage::assistant_text("hi there");
        assert!(estimated_message_chars(&m2) >= 8);
        let m3 = ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "c".into(),
            name: "search".into(),
            input: serde_json::json!({"query": "x"}),
        }]);
        assert!(estimated_message_chars(&m3) >= "search".len());
        let m4 = ChatMessage::tool_results(vec![ToolResult {
            tool_use_id: "c".into(),
            content: "result text".into(),
            is_error: false,
        }]);
        assert!(estimated_message_chars(&m4) >= "result text".len());
    }
}
