// crates/thinkingroot-serve/src/intelligence/trace.rs
//
// Hash-chained, Ed25519-signed agent trace log.
//
// Every step the agent takes — LLM call, tool dispatch, approval
// check, tool result, terminal text — is appended to a JSONL file
// at `.thinkingroot/traces/{conversation_id}.jsonl` as a [`TraceEntry`]:
//
//   {
//     "seq":            0,
//     "timestamp":      "2026-04-28T17:30:00Z",
//     "kind":           "agent.tool_call.proposed",
//     "payload":        { "name": "search", "input": {...}, ... },
//     "prev_blake3":    "all-zeros for the genesis entry",
//     "blake3":         "blake3 of (prev_blake3 || canonical_payload)",
//     "signed_by":      "ed25519:agent-pubkey-hex",
//     "signature":      "ed25519:hex-signature"
//   }
//
// The chain link is `blake3(prev_blake3 || canonical_payload)` where
// `canonical_payload` is the canonical JSON serialisation of the
// (seq, timestamp, kind, payload, prev_blake3) tuple. The signature
// covers the same canonical bytes.
//
// What the trace gives you:
//   * Tamper-evidence — flipping any byte in any historical entry
//     breaks the hash chain at that point and every subsequent hash.
//   * Authorship — every entry is signed by the agent's Ed25519 key,
//     so a third party can verify the trace was produced by that
//     specific key (and not, say, replayed or forged).
//   * Replayability — JSONL is human-readable; ops can grep, the UI
//     can tail it, and a future verifier crate can reconstruct the
//     full agent reasoning chain.
//
// Out of scope for S4 (deferred to S5+):
//   * Trace verification CLI (`root verify-trace path.jsonl`).
//   * Public key registry / revocation.
//   * Compaction / pruning.
//
// Storage: append-only JSONL under
// `<workspace_root>/.thinkingroot/traces/<conversation_id>.jsonl`. The
// directory is created on demand. We hold an open file handle for the
// duration of one conversation; the writer is `tokio::sync::Mutex`-
// guarded so the agent can append from any task without races.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use thinkingroot_core::{Error, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::intelligence::agent::AgentEvent;

/// A single entry in the trace log. JSON-serialised one entry per
/// line in `.jsonl`. Ordered fields here match the canonical hash
/// pre-image — if you reorder them, the chain verification breaks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraceEntry {
    /// Zero-based sequence number within this conversation. The first
    /// entry is `seq = 0`; every subsequent entry is the previous
    /// `seq + 1`. Out-of-order writes are a programming bug — the
    /// writer enforces monotonicity.
    pub seq: u64,
    /// RFC3339 UTC timestamp at write time.
    pub timestamp: DateTime<Utc>,
    /// Stable "namespace.action" identifier. See [`TraceKind`] for
    /// the full enumeration.
    pub kind: String,
    /// Free-form JSON payload describing the entry. Different `kind`s
    /// have different shapes, but every payload is canonical-JSON
    /// serialisable.
    pub payload: serde_json::Value,
    /// BLAKE3 hash of the previous entry's `blake3` field, as hex.
    /// The genesis entry uses `"0" * 64`.
    pub prev_blake3: String,
    /// `blake3(prev_blake3 || canonical_pre_image)` as hex. See
    /// [`canonical_pre_image`] for the exact pre-image.
    pub blake3: String,
    /// `"ed25519:" + hex(public_key)` of the signing agent.
    pub signed_by: String,
    /// `"ed25519:" + base64(signature)` over the same canonical
    /// pre-image as `blake3`.
    pub signature: String,
}

/// Stable "namespace.action" identifiers for trace entry kinds. Free
/// strings rather than an enum so future event types don't require a
/// schema bump — the verifier ignores unknown kinds.
pub mod kind {
    pub const AGENT_RUN_STARTED: &str = "agent.run.started";
    pub const AGENT_LLM_CALL: &str = "agent.llm.call";
    pub const AGENT_TOOL_PROPOSED: &str = "agent.tool.proposed";
    pub const AGENT_TOOL_REJECTED: &str = "agent.tool.rejected";
    pub const AGENT_TOOL_EXECUTING: &str = "agent.tool.executing";
    pub const AGENT_TOOL_FINISHED: &str = "agent.tool.finished";
    pub const AGENT_TEXT: &str = "agent.text";
    pub const AGENT_RUN_DONE: &str = "agent.run.done";
    pub const AGENT_RUN_ERROR: &str = "agent.run.error";
}

/// The canonical pre-image is what BLAKE3 hashes and what Ed25519
/// signs. It is a UTF-8 string with a fixed key order:
///
/// ```text
/// seq:<seq>\n
/// timestamp:<rfc3339>\n
/// kind:<kind>\n
/// payload:<canonical_json(payload)>\n
/// prev_blake3:<prev_blake3>
/// ```
///
/// `canonical_json` is `serde_json::to_string` with sort_keys
/// behaviour — we use serde_json's BTreeMap re-serialisation trick
/// for stability. Whitespace in the payload is fixed by
/// `serde_json::Value::to_string` so callers don't need to think
/// about it.
pub fn canonical_pre_image(
    seq: u64,
    timestamp: &DateTime<Utc>,
    kind: &str,
    payload: &serde_json::Value,
    prev_blake3: &str,
) -> String {
    let canonical_payload = canonical_json(payload);
    format!(
        "seq:{seq}\ntimestamp:{}\nkind:{kind}\npayload:{canonical_payload}\nprev_blake3:{prev_blake3}",
        timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// Stable JSON serialisation: object keys sorted recursively. Used as
/// the canonical payload form so two structurally-equal payloads
/// produce the same bytes regardless of insertion order in the
/// originating Rust HashMap.
fn canonical_json(value: &serde_json::Value) -> String {
    fn sort_recursive(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                    std::collections::BTreeMap::new();
                for (k, val) in map {
                    sorted.insert(k.clone(), sort_recursive(val));
                }
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(sort_recursive).collect())
            }
            other => other.clone(),
        }
    }
    let sorted = sort_recursive(value);
    serde_json::to_string(&sorted).unwrap_or_else(|_| "null".to_string())
}

/// All-zeros prev hash — used for the genesis entry.
pub const GENESIS_PREV_BLAKE3: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Compute the BLAKE3 chain link for an entry.
fn blake3_link(prev_blake3: &str, pre_image: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prev_blake3.as_bytes());
    hasher.update(b"\n");
    hasher.update(pre_image.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Format an Ed25519 verifying key as `"ed25519:" + hex`.
pub fn format_pubkey(vk: &VerifyingKey) -> String {
    format!("ed25519:{}", hex::encode(vk.to_bytes()))
}

/// Format an Ed25519 signature as `"ed25519:" + base64(signature)`.
fn format_signature(sig: &Signature) -> String {
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
    )
}

/// Parse the `signed_by` / `signature` fields back into typed values.
/// Used by the verifier.
pub fn parse_pubkey(s: &str) -> Result<VerifyingKey> {
    let hex_part = s.strip_prefix("ed25519:").ok_or_else(|| {
        Error::Verification(format!(
            "malformed signed_by (missing 'ed25519:' prefix): {s}"
        ))
    })?;
    let bytes = hex::decode(hex_part)
        .map_err(|e| Error::Verification(format!("signed_by hex decode failed: {e}")))?;
    if bytes.len() != 32 {
        return Err(Error::Verification(format!(
            "ed25519 public key must be 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr)
        .map_err(|e| Error::Verification(format!("ed25519 public key invalid: {e}")))
}

fn parse_signature(s: &str) -> Result<Signature> {
    let b64 = s.strip_prefix("ed25519:").ok_or_else(|| {
        Error::Verification(format!(
            "malformed signature (missing 'ed25519:' prefix): {s}"
        ))
    })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| Error::Verification(format!("signature base64 decode failed: {e}")))?;
    if bytes.len() != 64 {
        return Err(Error::Verification(format!(
            "ed25519 signature must be 64 bytes, got {}",
            bytes.len()
        )));
    }
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&bytes);
    Ok(Signature::from_bytes(&arr))
}

/// Verify a single trace entry's hash and signature against the
/// supplied `prev_blake3`. Used by [`verify_chain`] (which threads the
/// prev hash through every entry) and by tests.
pub fn verify_entry(entry: &TraceEntry, expected_prev_blake3: &str) -> Result<()> {
    if entry.prev_blake3 != expected_prev_blake3 {
        return Err(Error::Verification(format!(
            "trace entry seq={} prev_blake3 mismatch: expected {expected_prev_blake3}, got {}",
            entry.seq, entry.prev_blake3
        )));
    }
    let pre_image = canonical_pre_image(
        entry.seq,
        &entry.timestamp,
        &entry.kind,
        &entry.payload,
        &entry.prev_blake3,
    );
    let computed = blake3_link(&entry.prev_blake3, &pre_image);
    if computed != entry.blake3 {
        return Err(Error::Verification(format!(
            "trace entry seq={} blake3 mismatch: expected {computed}, got {}",
            entry.seq, entry.blake3
        )));
    }
    let pubkey = parse_pubkey(&entry.signed_by)?;
    let signature = parse_signature(&entry.signature)?;
    pubkey
        .verify(pre_image.as_bytes(), &signature)
        .map_err(|e| {
            Error::Verification(format!(
                "trace entry seq={} signature verify failed: {e}",
                entry.seq
            ))
        })?;
    Ok(())
}

/// Verify a complete trace from genesis. Returns `Ok(())` iff the
/// chain is well-formed: monotone seq, sane prev_blake3 linkage,
/// every blake3 / signature valid against the entry's own public key.
pub fn verify_chain(entries: &[TraceEntry]) -> Result<()> {
    let mut prev = GENESIS_PREV_BLAKE3.to_string();
    let mut expected_seq: u64 = 0;
    for entry in entries {
        if entry.seq != expected_seq {
            return Err(Error::Verification(format!(
                "trace seq gap: expected {expected_seq}, got {}",
                entry.seq
            )));
        }
        verify_entry(entry, &prev)?;
        prev = entry.blake3.clone();
        expected_seq += 1;
    }
    Ok(())
}

/// Async writer interface so the agent loop can pipe through any
/// backing store. Production wires [`FileTraceLog`]; tests use
/// [`InMemoryTraceLog`].
#[async_trait]
pub trait TraceLog: Send + Sync {
    async fn append(&self, kind: &str, payload: serde_json::Value) -> Result<TraceEntry>;
}

/// Helper: project an [`AgentEvent`] into a (kind, payload) pair so
/// the agent loop can write a trace entry per event without coupling
/// the trace module to AgentEvent's exact shape.
pub fn event_to_trace(event: &AgentEvent) -> (&'static str, serde_json::Value) {
    use serde_json::json;
    match event {
        AgentEvent::Text { content } => (kind::AGENT_TEXT, json!({"content": content})),
        AgentEvent::ToolCallProposed {
            id,
            name,
            input,
            is_write,
        } => (
            kind::AGENT_TOOL_PROPOSED,
            json!({
                "id": id,
                "name": name,
                "input": input,
                "is_write": is_write,
            }),
        ),
        AgentEvent::ToolCallRejected { id, name, reason } => (
            kind::AGENT_TOOL_REJECTED,
            json!({"id": id, "name": name, "reason": reason}),
        ),
        AgentEvent::ToolCallExecuting { id, name } => {
            (kind::AGENT_TOOL_EXECUTING, json!({"id": id, "name": name}))
        }
        AgentEvent::ToolCallFinished {
            id,
            name,
            content,
            is_error,
        } => (
            kind::AGENT_TOOL_FINISHED,
            json!({
                "id": id,
                "name": name,
                "content": content,
                "is_error": is_error,
            }),
        ),
        AgentEvent::Done {
            final_text,
            iterations,
        } => (
            kind::AGENT_RUN_DONE,
            json!({"final_text": final_text, "iterations": iterations}),
        ),
        AgentEvent::Error { message } => (kind::AGENT_RUN_ERROR, json!({"message": message})),
    }
}

/// Shared writer state: signing key, current sequence, current
/// `prev_blake3`. Owned by [`FileTraceLog`] and [`InMemoryTraceLog`]
/// alike.
struct TraceWriterCore {
    signing_key: SigningKey,
    pubkey_str: String,
    seq: u64,
    prev_blake3: String,
}

impl TraceWriterCore {
    fn new(signing_key: SigningKey) -> Self {
        let pubkey_str = format_pubkey(&signing_key.verifying_key());
        Self {
            signing_key,
            pubkey_str,
            seq: 0,
            prev_blake3: GENESIS_PREV_BLAKE3.to_string(),
        }
    }

    fn make_entry(&mut self, kind: &str, payload: serde_json::Value) -> TraceEntry {
        let seq = self.seq;
        let timestamp = Utc::now();
        let pre_image = canonical_pre_image(seq, &timestamp, kind, &payload, &self.prev_blake3);
        let blake3 = blake3_link(&self.prev_blake3, &pre_image);
        let signature = self.signing_key.sign(pre_image.as_bytes());
        let entry = TraceEntry {
            seq,
            timestamp,
            kind: kind.to_string(),
            payload,
            prev_blake3: self.prev_blake3.clone(),
            blake3: blake3.clone(),
            signed_by: self.pubkey_str.clone(),
            signature: format_signature(&signature),
        };
        self.seq += 1;
        self.prev_blake3 = blake3;
        entry
    }
}

/// In-memory trace log — for tests and debug surfaces. Keeps every
/// entry in a `Vec<TraceEntry>` accessible via [`Self::entries`].
pub struct InMemoryTraceLog {
    inner: Mutex<InMemoryTraceLogInner>,
}

struct InMemoryTraceLogInner {
    core: TraceWriterCore,
    entries: Vec<TraceEntry>,
}

impl InMemoryTraceLog {
    pub fn new() -> Self {
        Self::with_signing_key(generate_signing_key())
    }

    pub fn with_signing_key(signing_key: SigningKey) -> Self {
        Self {
            inner: Mutex::new(InMemoryTraceLogInner {
                core: TraceWriterCore::new(signing_key),
                entries: Vec::new(),
            }),
        }
    }

    pub async fn entries(&self) -> Vec<TraceEntry> {
        self.inner.lock().await.entries.clone()
    }

    pub async fn pubkey(&self) -> VerifyingKey {
        self.inner.lock().await.core.signing_key.verifying_key()
    }
}

impl Default for InMemoryTraceLog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TraceLog for InMemoryTraceLog {
    async fn append(&self, kind: &str, payload: serde_json::Value) -> Result<TraceEntry> {
        let mut guard = self.inner.lock().await;
        let entry = guard.core.make_entry(kind, payload);
        guard.entries.push(entry.clone());
        Ok(entry)
    }
}

/// File-backed JSONL trace log. Holds an open `tokio::fs::File`
/// handle for the duration of the conversation. Each `append` writes
/// one JSON line (terminated `\n`) and flushes — durability matters
/// more than throughput for an audit log.
pub struct FileTraceLog {
    path: PathBuf,
    inner: Mutex<FileTraceLogInner>,
}

struct FileTraceLogInner {
    core: TraceWriterCore,
    file: tokio::fs::File,
}

impl FileTraceLog {
    /// Open `path` for append, creating it (and any missing parents)
    /// on demand. Generates a fresh Ed25519 signing key — pair with
    /// `with_signing_key` if the agent has a stable identity loaded
    /// from disk.
    pub async fn open(path: PathBuf) -> Result<Self> {
        Self::open_with_signing_key(path, generate_signing_key()).await
    }

    pub async fn open_with_signing_key(path: PathBuf, signing_key: SigningKey) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::io_path(parent.to_path_buf(), e))?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| Error::io_path(path.clone(), e))?;
        Ok(Self {
            path,
            inner: Mutex::new(FileTraceLogInner {
                core: TraceWriterCore::new(signing_key),
                file,
            }),
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

#[async_trait]
impl TraceLog for FileTraceLog {
    async fn append(&self, kind: &str, payload: serde_json::Value) -> Result<TraceEntry> {
        let mut guard = self.inner.lock().await;
        let entry = guard.core.make_entry(kind, payload);
        let mut line = serde_json::to_string(&entry)
            .map_err(|e| Error::Serialization(format!("trace serialise failed: {e}")))?;
        line.push('\n');
        guard
            .file
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::io_path(self.path.clone(), e))?;
        guard
            .file
            .flush()
            .await
            .map_err(|e| Error::io_path(self.path.clone(), e))?;
        Ok(entry)
    }
}

/// Convenience: a no-op trace log. Used as the default when the
/// agent is constructed without a trace log argument.
pub struct NullTraceLog;

#[async_trait]
impl TraceLog for NullTraceLog {
    async fn append(&self, _kind: &str, _payload: serde_json::Value) -> Result<TraceEntry> {
        Err(Error::Verification(
            "NullTraceLog does not produce entries; use InMemoryTraceLog or FileTraceLog"
                .to_string(),
        ))
    }
}

/// Cheap-clone helper for the agent loop to share one trace log
/// across the inner async tasks the streaming variant will spawn in
/// S5.
pub type SharedTraceLog = Arc<dyn TraceLog>;

/// Generate a fresh Ed25519 signing key. Pulled out so tests can use
/// a deterministic seed when needed.
pub fn generate_signing_key() -> SigningKey {
    use rand::TryRngCore;
    let mut rng = rand::rngs::OsRng;
    let mut seed = [0u8; 32];
    rng.try_fill_bytes(&mut seed)
        .expect("OsRng must produce seed bytes for ed25519 key generation");
    SigningKey::from_bytes(&seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use tempfile::tempdir;

    fn fixed_signing_key() -> SigningKey {
        // Stable seed so chain hashes / signatures are reproducible
        // across test runs without depending on CSPRNG output.
        let seed = [42u8; 32];
        SigningKey::from_bytes(&seed)
    }

    #[tokio::test]
    async fn first_entry_chains_from_genesis_and_verifies() {
        let log = InMemoryTraceLog::with_signing_key(fixed_signing_key());
        let entry = log
            .append(kind::AGENT_RUN_STARTED, serde_json::json!({"k": "v"}))
            .await
            .unwrap();
        assert_eq!(entry.seq, 0);
        assert_eq!(entry.prev_blake3, GENESIS_PREV_BLAKE3);
        assert_ne!(entry.blake3, GENESIS_PREV_BLAKE3);
        assert!(entry.signed_by.starts_with("ed25519:"));
        assert!(entry.signature.starts_with("ed25519:"));
        verify_entry(&entry, GENESIS_PREV_BLAKE3).expect("first entry must verify");
    }

    #[tokio::test]
    async fn subsequent_entries_chain_from_previous_blake3() {
        let log = InMemoryTraceLog::with_signing_key(fixed_signing_key());
        let e0 = log
            .append(kind::AGENT_RUN_STARTED, serde_json::json!({}))
            .await
            .unwrap();
        let e1 = log
            .append(kind::AGENT_TEXT, serde_json::json!({"content": "hi"}))
            .await
            .unwrap();
        let e2 = log
            .append(kind::AGENT_RUN_DONE, serde_json::json!({"iterations": 1}))
            .await
            .unwrap();
        assert_eq!(e1.prev_blake3, e0.blake3);
        assert_eq!(e2.prev_blake3, e1.blake3);
        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
    }

    #[tokio::test]
    async fn verify_chain_passes_for_well_formed_log() {
        let log = InMemoryTraceLog::with_signing_key(fixed_signing_key());
        for i in 0..5u64 {
            log.append("test.kind", serde_json::json!({"i": i}))
                .await
                .unwrap();
        }
        let entries = log.entries().await;
        verify_chain(&entries).expect("chain must verify");
    }

    #[tokio::test]
    async fn verify_chain_detects_seq_gap() {
        let log = InMemoryTraceLog::with_signing_key(fixed_signing_key());
        let e0 = log.append("k", serde_json::json!({})).await.unwrap();
        let mut e1 = log.append("k", serde_json::json!({})).await.unwrap();
        e1.seq = 5; // tamper
        let res = verify_chain(&[e0, e1]);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("seq gap"));
    }

    #[tokio::test]
    async fn verify_chain_detects_payload_tamper() {
        let log = InMemoryTraceLog::with_signing_key(fixed_signing_key());
        log.append(kind::AGENT_RUN_STARTED, serde_json::json!({}))
            .await
            .unwrap();
        let mut entries = log.entries().await;
        entries[0].payload = serde_json::json!({"tampered": true});
        let res = verify_chain(&entries);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("blake3 mismatch") || msg.contains("signature verify failed"));
    }

    #[tokio::test]
    async fn verify_chain_detects_signature_tamper() {
        let log = InMemoryTraceLog::with_signing_key(fixed_signing_key());
        log.append(kind::AGENT_RUN_STARTED, serde_json::json!({}))
            .await
            .unwrap();
        let mut entries = log.entries().await;
        // Replace the signature with a syntactically-valid but wrong one.
        let other_key = SigningKey::from_bytes(&[7u8; 32]);
        let pre_image = canonical_pre_image(
            entries[0].seq,
            &entries[0].timestamp,
            &entries[0].kind,
            &entries[0].payload,
            &entries[0].prev_blake3,
        );
        let bad_sig = other_key.sign(pre_image.as_bytes());
        entries[0].signature = format_signature(&bad_sig);
        let res = verify_chain(&entries);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("signature verify failed"));
    }

    #[tokio::test]
    async fn verify_chain_detects_chain_link_tamper() {
        // Mutate the prev_blake3 of entry 1; chain check must detect.
        let log = InMemoryTraceLog::with_signing_key(fixed_signing_key());
        log.append("k", serde_json::json!({})).await.unwrap();
        log.append("k", serde_json::json!({})).await.unwrap();
        let mut entries = log.entries().await;
        entries[1].prev_blake3 = GENESIS_PREV_BLAKE3.to_string();
        let res = verify_chain(&entries);
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn canonical_payload_is_key_order_independent() {
        // Two payloads with the same keys in different insertion
        // order must produce the same canonical pre-image and hence
        // the same blake3 / signature.
        let mut a = serde_json::Map::new();
        a.insert("z".to_string(), serde_json::json!(1));
        a.insert("a".to_string(), serde_json::json!(2));
        let mut b = serde_json::Map::new();
        b.insert("a".to_string(), serde_json::json!(2));
        b.insert("z".to_string(), serde_json::json!(1));

        let ca = canonical_json(&serde_json::Value::Object(a));
        let cb = canonical_json(&serde_json::Value::Object(b));
        assert_eq!(ca, cb);
    }

    #[tokio::test]
    async fn file_trace_log_writes_jsonl_and_round_trips_through_verify_chain() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("traces").join("conv-001.jsonl");
        let log = FileTraceLog::open_with_signing_key(path.clone(), fixed_signing_key())
            .await
            .unwrap();
        log.append(kind::AGENT_RUN_STARTED, serde_json::json!({"who": "agent"}))
            .await
            .unwrap();
        log.append(kind::AGENT_TEXT, serde_json::json!({"content": "hi"}))
            .await
            .unwrap();
        log.append(kind::AGENT_RUN_DONE, serde_json::json!({"iterations": 1}))
            .await
            .unwrap();

        // Read back from disk and verify the chain.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let entries: Vec<TraceEntry> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line must be a TraceEntry"))
            .collect();
        assert_eq!(entries.len(), 3);
        verify_chain(&entries).expect("file-loaded chain must verify");
    }

    #[test]
    fn event_to_trace_covers_every_agent_event_variant() {
        // For every variant, ensure we map to a stable kind and a
        // serialisable payload. New variants must be added here.
        use serde_json::json;
        let cases: Vec<(AgentEvent, &str)> = vec![
            (
                AgentEvent::Text {
                    content: "hi".into(),
                },
                kind::AGENT_TEXT,
            ),
            (
                AgentEvent::ToolCallProposed {
                    id: "c".into(),
                    name: "search".into(),
                    input: json!({}),
                    is_write: false,
                },
                kind::AGENT_TOOL_PROPOSED,
            ),
            (
                AgentEvent::ToolCallRejected {
                    id: "c".into(),
                    name: "x".into(),
                    reason: "no".into(),
                },
                kind::AGENT_TOOL_REJECTED,
            ),
            (
                AgentEvent::ToolCallExecuting {
                    id: "c".into(),
                    name: "x".into(),
                },
                kind::AGENT_TOOL_EXECUTING,
            ),
            (
                AgentEvent::ToolCallFinished {
                    id: "c".into(),
                    name: "x".into(),
                    content: "ok".into(),
                    is_error: false,
                },
                kind::AGENT_TOOL_FINISHED,
            ),
            (
                AgentEvent::Done {
                    final_text: "done".into(),
                    iterations: 1,
                },
                kind::AGENT_RUN_DONE,
            ),
            (
                AgentEvent::Error {
                    message: "oops".into(),
                },
                kind::AGENT_RUN_ERROR,
            ),
        ];
        for (event, expected_kind) in cases {
            let (k, _payload) = event_to_trace(&event);
            assert_eq!(k, expected_kind);
        }
    }

    #[tokio::test]
    async fn null_trace_log_returns_error_consistently() {
        let log = NullTraceLog;
        let res = log.append("k", serde_json::json!({})).await;
        assert!(res.is_err());
    }

    #[test]
    fn parse_pubkey_round_trips_through_format() {
        let key = fixed_signing_key().verifying_key();
        let s = format_pubkey(&key);
        let parsed = parse_pubkey(&s).unwrap();
        assert_eq!(parsed.to_bytes(), key.to_bytes());
    }

    #[test]
    fn parse_pubkey_rejects_malformed_input() {
        assert!(parse_pubkey("notvalid").is_err());
        assert!(parse_pubkey("ed25519:zz").is_err());
        assert!(parse_pubkey("ed25519:").is_err());
    }
}
