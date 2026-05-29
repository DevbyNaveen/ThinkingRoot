//! JIT (Just-In-Time) capability acquisition.
//!
//! When the agent can't make progress — a tool errored, or the model
//! signalled it lacks something — the JIT layer classifies the gap and
//! picks an acquisition strategy instead of giving up. Two pieces:
//!
//! 1. [`classify_gap`] — an LLM call that buckets the gap into
//!    [`GapKind`] (`knowledge` / `capability` / `impossible`). This is
//!    the taxonomy named in the cloud's CLAUDE.md.
//! 2. [`ladder_for`] — maps a gap to an ordered list of
//!    [`AcquisitionRung`]s. Each rung is a mechanism that actually
//!    exists in this engine (see the linked tools), so the ladder is a
//!    real plan, not a wish-list:
//!
//!    | rung | mechanism |
//!    |------|-----------|
//!    | `ReuseExistingTool`    | re-prompt with a tool hint |
//!    | `LoadSkill`            | `use_skill` (existing) |
//!    | `DefineSkill`          | `skill_define` (`acquisition_tools`) |
//!    | `InstallMcpServer`     | `mcp_server_install` + live remount |
//!    | `DeployRootFunction`   | author JS → `deno_core` executor |
//!    | `ShellAcquire`         | `thinkingroot-sandbox` (OS-native) |
//!    | `EscalateOrImpossible` | route to human / give up honestly |
//!
//! The agent loop calls [`classify_gap`] + [`ladder_for`] at a failure
//! boundary, emits `AgentEvent::GapClassified` / `AcquisitionAttempt`,
//! and tries rungs in order, bounded by the existing iteration ceiling
//! (no new unbounded loop). Cross-ref the cloud roadmap +
//! `../thinkingroot/docs/2026-05-25-jit-capability-acquisition-brainstorm.md`.

use thinkingroot_core::Result;
use thinkingroot_llm::llm::{ChatMessage, ToolChoice, ToolUseResponse};

use crate::intelligence::agent::LlmBackend;

/// The three gap classes. `Knowledge` → retrieve/ask; `Capability` →
/// acquire a tool/skill/function; `Impossible` → stop honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    Knowledge,
    Capability,
    Impossible,
}

impl GapKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GapKind::Knowledge => "knowledge",
            GapKind::Capability => "capability",
            GapKind::Impossible => "impossible",
        }
    }
}

/// One rung of the acquisition ladder. Ordering in [`ladder_for`]
/// reflects cost: cheap/reversible first, expensive/risky last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquisitionRung {
    ReuseExistingTool,
    LoadSkill,
    DefineSkill,
    InstallMcpServer,
    DeployRootFunction,
    ShellAcquire,
    EscalateOrImpossible,
}

impl AcquisitionRung {
    /// Stable wire name surfaced in `AgentEvent::AcquisitionAttempt`.
    pub fn as_str(&self) -> &'static str {
        match self {
            AcquisitionRung::ReuseExistingTool => "reuse_existing_tool",
            AcquisitionRung::LoadSkill => "load_skill",
            AcquisitionRung::DefineSkill => "define_skill",
            AcquisitionRung::InstallMcpServer => "install_mcp_server",
            AcquisitionRung::DeployRootFunction => "deploy_root_function",
            AcquisitionRung::ShellAcquire => "shell_acquire",
            AcquisitionRung::EscalateOrImpossible => "escalate_or_impossible",
        }
    }

    /// The engine tool/mechanism that implements this rung — what the
    /// agent should actually invoke. `None` for the terminal rung.
    pub fn mechanism_tool(&self) -> Option<&'static str> {
        match self {
            AcquisitionRung::ReuseExistingTool => None,
            AcquisitionRung::LoadSkill => Some("use_skill"),
            AcquisitionRung::DefineSkill => Some("skill_define"),
            AcquisitionRung::InstallMcpServer => Some("mcp_server_install"),
            AcquisitionRung::DeployRootFunction => Some("root_function"),
            AcquisitionRung::ShellAcquire => Some("shell_exec"),
            AcquisitionRung::EscalateOrImpossible => None,
        }
    }
}

/// The ordered ladder for a gap kind. Knowledge gaps don't acquire
/// capability — they retrieve — so their ladder is short. Capability
/// gaps walk the full mechanism ladder. Impossible short-circuits.
pub fn ladder_for(kind: GapKind) -> Vec<AcquisitionRung> {
    use AcquisitionRung::*;
    match kind {
        GapKind::Knowledge => vec![ReuseExistingTool, LoadSkill, EscalateOrImpossible],
        GapKind::Capability => vec![
            ReuseExistingTool,
            LoadSkill,
            DefineSkill,
            InstallMcpServer,
            DeployRootFunction,
            ShellAcquire,
            EscalateOrImpossible,
        ],
        GapKind::Impossible => vec![EscalateOrImpossible],
    }
}

/// Parse a classifier response into `(kind, rationale)`. Tolerant: looks
/// for the keyword anywhere (LLMs wrap it in prose), defaulting to
/// `Impossible` when no class is found — the safe, non-fabricating
/// fallback (we'd rather stop than loop on an unclassifiable gap).
pub fn parse_gap_classification(response: &str) -> (GapKind, String) {
    let lower = response.to_lowercase();
    let kind = if lower.contains("capability") {
        GapKind::Capability
    } else if lower.contains("knowledge") {
        GapKind::Knowledge
    } else if lower.contains("impossible") {
        GapKind::Impossible
    } else {
        GapKind::Impossible
    };
    (kind, response.trim().to_string())
}

const CLASSIFY_SYSTEM: &str = "You are a capability-gap classifier inside an AI agent. \
The agent just failed to make progress. Classify the gap as exactly ONE of: \
`knowledge` (the agent lacks information it could retrieve or be told), \
`capability` (the agent lacks a tool/skill/function it could acquire), or \
`impossible` (the task cannot be done with any acquirable capability). \
Reply with the single classification word, then a one-sentence rationale.";

/// Classify a stuck situation via the agent's LLM backend. Calls
/// `chat_with_tools` with NO tools so the model can only return text,
/// then parses the classification word. Returns the gap kind + the
/// model's rationale (surfaced in `GapClassified`).
pub async fn classify_gap(llm: &dyn LlmBackend, situation: &str) -> Result<(GapKind, String)> {
    let messages = [ChatMessage::user(situation)];
    let response = llm
        .chat_with_tools(CLASSIFY_SYSTEM, &messages, &[], &ToolChoice::Auto)
        .await?;
    let text = match response {
        ToolUseResponse::Text { text, .. } => text,
        // No tools were offered, so this branch is unexpected; fall back
        // to whatever preamble the model produced.
        ToolUseResponse::ToolCalls { text_preamble, .. } => text_preamble,
    };
    Ok(parse_gap_classification(&text))
}

/// Build a one-line hint the agent appends to the failing tool result so
/// the model sees the recommended next acquisition step. Names the first
/// non-reuse rung's mechanism tool for the gap's ladder.
pub fn acquisition_hint(kind: GapKind) -> (AcquisitionRung, String) {
    // Pick the first rung that names a concrete mechanism tool (skip the
    // bare "reuse existing tool" rung — there's nothing to invoke).
    let rung = ladder_for(kind)
        .into_iter()
        .find(|r| r.mechanism_tool().is_some())
        .unwrap_or(AcquisitionRung::EscalateOrImpossible);
    let outcome = match rung.mechanism_tool() {
        Some(tool) => format!(
            "gap classified `{}` — try acquiring the capability via the `{}` tool before giving up",
            kind.as_str(),
            tool
        ),
        None => format!(
            "gap classified `{}` — no acquirable capability fits; report the limitation honestly",
            kind.as_str()
        ),
    };
    (rung, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_class() {
        assert_eq!(
            parse_gap_classification("capability — needs a GitHub tool").0,
            GapKind::Capability
        );
        assert_eq!(
            parse_gap_classification("knowledge: just needs the API docs").0,
            GapKind::Knowledge
        );
        assert_eq!(
            parse_gap_classification("impossible, requires physical access").0,
            GapKind::Impossible
        );
        // Unclassifiable → safe Impossible fallback (no fabrication).
        assert_eq!(parse_gap_classification("hmm not sure").0, GapKind::Impossible);
    }

    #[test]
    fn capability_ladder_is_full_and_ordered() {
        let ladder = ladder_for(GapKind::Capability);
        // Cheapest first, terminal last.
        assert_eq!(ladder.first(), Some(&AcquisitionRung::ReuseExistingTool));
        assert_eq!(ladder.last(), Some(&AcquisitionRung::EscalateOrImpossible));
        // The 7-rung ladder.
        assert_eq!(ladder.len(), 7);
        // Every non-terminal rung maps to a real engine mechanism.
        for rung in &ladder {
            match rung {
                AcquisitionRung::ReuseExistingTool | AcquisitionRung::EscalateOrImpossible => {}
                other => assert!(
                    other.mechanism_tool().is_some(),
                    "{other:?} must name a mechanism tool"
                ),
            }
        }
    }

    #[test]
    fn impossible_short_circuits() {
        assert_eq!(ladder_for(GapKind::Impossible), vec![AcquisitionRung::EscalateOrImpossible]);
    }

    #[test]
    fn acquisition_hint_names_first_mechanism_rung() {
        // Capability ladder's first mechanism rung is LoadSkill (use_skill).
        let (rung, outcome) = acquisition_hint(GapKind::Capability);
        assert_eq!(rung, AcquisitionRung::LoadSkill);
        assert!(outcome.contains("use_skill"));
        assert!(outcome.contains("capability"));
        // Impossible → terminal rung, honest "report the limitation".
        let (rung, outcome) = acquisition_hint(GapKind::Impossible);
        assert_eq!(rung, AcquisitionRung::EscalateOrImpossible);
        assert!(outcome.contains("honestly"));
    }

    #[test]
    fn rung_wire_names_are_stable() {
        assert_eq!(AcquisitionRung::InstallMcpServer.as_str(), "install_mcp_server");
        assert_eq!(AcquisitionRung::DeployRootFunction.mechanism_tool(), Some("root_function"));
    }
}
