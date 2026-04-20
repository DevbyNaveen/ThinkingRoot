use std::collections::HashSet;

use thinkingroot_graph::graph::{EntityContext, TopEntity};

/// A density-compressed knowledge packet ready to deliver to an LLM agent.
///
/// Uses structured text format (not JSON) for 2-4x better token efficiency:
/// JSON overhead (quotes, brackets, keys) costs ~50-100% extra tokens while
/// adding zero semantic value for an LLM reader.
#[derive(Debug, Clone)]
pub struct KnowledgePacket {
    pub sections: Vec<PacketSection>,
    pub entity_name: Option<String>,
    /// All claim IDs included — used to update the session delivered set.
    pub claim_ids: Vec<String>,
    /// Estimated token count for the formatted output.
    pub estimated_tokens: usize,
}

#[derive(Debug, Clone)]
pub struct PacketSection {
    pub title: String,
    pub lines: Vec<String>,
}

/// Approximate token count: ~4 characters per token (Claude tokeniser estimate).
pub fn estimate_tokens(s: &str) -> usize {
    (s.len() / 4).max(1)
}

/// Compress an `EntityContext` into a token-efficient `KnowledgePacket`.
///
/// Session-aware: claims already delivered are marked `~` (stale) so the agent
/// recognises them as context rather than new information. The budget prevents
/// runaway context growth — older delivered claims are included only when the
/// budget allows it.
pub fn compress(
    ctx: &EntityContext,
    budget: usize,
    delivered: &HashSet<String>,
) -> KnowledgePacket {
    let mut sections: Vec<PacketSection> = Vec::new();
    let mut claim_ids: Vec<String> = Vec::new();
    let mut used_tokens: usize = 0;

    // ── Header ──────────────────────────────────────────────────
    let mut header_lines = if ctx.description.is_empty() {
        vec![format!("## {} [{}]", ctx.name, ctx.entity_type)]
    } else {
        vec![
            format!("## {} [{}]", ctx.name, ctx.entity_type),
            format!("desc: {}", ctx.description),
        ]
    };
    if !ctx.aliases.is_empty() {
        header_lines.push(format!("aliases: {}", ctx.aliases.join(", ")));
    }
    for l in &header_lines {
        used_tokens += estimate_tokens(l);
    }
    sections.push(PacketSection {
        title: String::new(),
        lines: header_lines,
    });

    // ── Relations ────────────────────────────────────────────────
    if (!ctx.outgoing_relations.is_empty() || !ctx.incoming_relations.is_empty())
        && used_tokens < budget
    {
        let mut rel_lines: Vec<String> = Vec::new();
        for (target, rel_type, strength) in &ctx.outgoing_relations {
            if used_tokens >= budget {
                break;
            }
            let line = format!("  → {target} [{rel_type}] {strength:.2}");
            used_tokens += estimate_tokens(&line);
            rel_lines.push(line);
        }
        for (source, rel_type, strength) in &ctx.incoming_relations {
            if used_tokens >= budget {
                break;
            }
            let line = format!("  ← {source} [{rel_type}] {strength:.2} (reverse)");
            used_tokens += estimate_tokens(&line);
            rel_lines.push(line);
        }
        if !rel_lines.is_empty() {
            sections.push(PacketSection {
                title: "RELATIONS".to_string(),
                lines: rel_lines,
            });
        }
    }

    // ── Claims — grouped by type, new claims first ───────────────
    let mut by_type: std::collections::BTreeMap<
        String,
        Vec<&thinkingroot_graph::graph::ContextClaim>,
    > = std::collections::BTreeMap::new();
    for claim in &ctx.claims {
        by_type
            .entry(claim.claim_type.clone())
            .or_default()
            .push(claim);
    }

    for (claim_type, claims) in &by_type {
        if used_tokens >= budget {
            break;
        }

        // New claims first (undelivered), then stale for context.
        let mut ordered: Vec<&thinkingroot_graph::graph::ContextClaim> = Vec::new();
        for claim in claims.iter() {
            if !delivered.contains(&claim.id) {
                ordered.push(claim);
            }
        }
        for claim in claims.iter() {
            if delivered.contains(&claim.id) {
                ordered.push(claim);
            }
        }

        let mut type_lines: Vec<String> = Vec::new();

        for claim in &ordered {
            if used_tokens >= budget {
                break;
            }
            let tier_abbrev = match claim.extraction_tier.as_str() {
                "structural" => "ast",
                "agent_inferred" => "agent",
                _ => "llm",
            };
            // `~` prefix marks stale (already-delivered) claims for context only.
            let marker = if delivered.contains(&claim.id) {
                "~"
            } else {
                ""
            };
            let line = format!(
                "  {marker}[{:.2},{tier_abbrev}] {}",
                claim.confidence, claim.statement
            );
            used_tokens += estimate_tokens(&line);
            type_lines.push(line);
            claim_ids.push(claim.id.clone());
        }

        if !type_lines.is_empty() {
            sections.push(PacketSection {
                title: claim_type.to_uppercase(),
                lines: type_lines,
            });
        }
    }

    // ── Contradictions ───────────────────────────────────────────
    if !ctx.contradictions.is_empty() && used_tokens < budget {
        let mut contra_lines: Vec<String> = Vec::new();
        for c in &ctx.contradictions {
            if used_tokens >= budget {
                break;
            }
            let line = format!("  [{}] {}", c.status, c.explanation);
            used_tokens += estimate_tokens(&line);
            contra_lines.push(line);
        }
        if !contra_lines.is_empty() {
            sections.push(PacketSection {
                title: "⚠ CONTRADICTIONS".to_string(),
                lines: contra_lines,
            });
        }
    }

    KnowledgePacket {
        sections,
        entity_name: Some(ctx.name.clone()),
        claim_ids,
        estimated_tokens: used_tokens,
    }
}

/// Format a `KnowledgePacket` to structured text ready for an LLM context window.
pub fn format_packet(packet: &KnowledgePacket) -> String {
    let mut out = String::with_capacity(packet.estimated_tokens * 4 + 64);
    for (i, section) in packet.sections.iter().enumerate() {
        if i > 0 && !section.title.is_empty() {
            out.push('\n');
            out.push_str(&section.title);
            out.push('\n');
        }
        for line in &section.lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Format a workspace summary to a token-efficient brief (~100-200 tokens).
///
/// This is the Tier-0 "index" response — gives an agent orientation without
/// dumping the full graph. The agent then calls `investigate` for specifics.
#[allow(clippy::too_many_arguments)]
pub fn format_workspace_brief(
    workspace: &str,
    entity_count: usize,
    claim_count: usize,
    source_count: usize,
    top_entities: &[TopEntity],
    recent_decisions: &[(String, f64)],
    contradiction_count: usize,
) -> String {
    let mut out = format!(
        "## {workspace} — Knowledge Graph\n\
         stats: {entity_count} entities · {claim_count} claims · {source_count} sources\n"
    );

    if !top_entities.is_empty() {
        out.push_str("top entities (by claim count):\n");
        for e in top_entities.iter().take(8) {
            out.push_str(&format!(
                "  {} [{}] {} claims\n",
                e.name, e.entity_type, e.claim_count
            ));
        }
    }

    if !recent_decisions.is_empty() {
        out.push_str("recent decisions:\n");
        for (stmt, conf) in recent_decisions.iter().take(5) {
            out.push_str(&format!("  [{conf:.2}] {stmt}\n"));
        }
    }

    if contradiction_count > 0 {
        out.push_str(&format!(
            "⚠ {contradiction_count} unresolved contradiction(s)\n"
        ));
    }

    out.push_str("--- use investigate(entity_name) to deep-dive\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_graph::graph::{ContextClaim, ContextContradiction, EntityContext};

    fn make_context(claim_count: usize) -> EntityContext {
        let claims: Vec<ContextClaim> = (0..claim_count)
            .map(|i| ContextClaim {
                id: format!("c{i}"),
                statement: format!("Claim {i}: AuthService validates JWT tokens on every request"),
                claim_type: "Fact".to_string(),
                confidence: 0.9,
                source_uri: "src/auth.rs".to_string(),
                extraction_tier: "llm".to_string(),
            })
            .collect();
        EntityContext {
            id: "e1".to_string(),
            name: "AuthService".to_string(),
            entity_type: "Service".to_string(),
            description: "Handles user authentication".to_string(),
            aliases: vec!["auth-service".to_string()],
            outgoing_relations: vec![
                ("PostgreSQL".to_string(), "depends_on".to_string(), 0.95),
                ("JWTValidator".to_string(), "calls".to_string(), 0.85),
            ],
            incoming_relations: vec![("APIGateway".to_string(), "calls".to_string(), 0.92)],
            claims,
            contradictions: vec![ContextContradiction {
                explanation: "Two claims disagree on token expiry".to_string(),
                status: "Detected".to_string(),
            }],
        }
    }

    #[test]
    fn estimate_tokens_reasonable() {
        let s = "AuthService uses RS256 for JWT signing and token validation per RFC 7519";
        let tokens = estimate_tokens(s);
        assert!((12..=22).contains(&tokens), "got {tokens}");
    }

    #[test]
    fn compress_respects_token_budget() {
        let ctx = make_context(50);
        let budget = 120;
        let packet = compress(&ctx, budget, &HashSet::new());
        // Allow a small overshoot due to the last line that crosses the boundary.
        assert!(
            packet.estimated_tokens <= budget + 20,
            "expected ≤{} tokens, got {}",
            budget + 20,
            packet.estimated_tokens
        );
    }

    #[test]
    fn compress_marks_delivered_claims_stale() {
        let ctx = make_context(3);
        let mut delivered = HashSet::new();
        delivered.insert("c0".to_string());
        delivered.insert("c1".to_string());

        let packet = compress(&ctx, 4_000, &delivered);
        let text = format_packet(&packet);
        // Delivered claims should be marked with `~`.
        let tilde_count = text.lines().filter(|l| l.contains("~[")).count();
        assert_eq!(
            tilde_count, 2,
            "expected 2 stale markers, got {tilde_count}"
        );
    }

    #[test]
    fn format_packet_produces_structured_text() {
        let ctx = make_context(2);
        let packet = compress(&ctx, 4_000, &HashSet::new());
        let text = format_packet(&packet);

        assert!(text.contains("## AuthService [Service]"));
        assert!(text.contains("RELATIONS"));
        assert!(text.contains("→ PostgreSQL [depends_on]"));
        assert!(text.contains("← APIGateway [calls]"));
        assert!(text.contains("FACT"));
        assert!(text.contains("⚠ CONTRADICTIONS"));
        // Must NOT look like JSON.
        assert!(
            !text.contains("{\""),
            "should not contain JSON object syntax"
        );
    }

    #[test]
    fn format_workspace_brief_includes_all_sections() {
        let top = vec![
            TopEntity {
                name: "AuthService".to_string(),
                entity_type: "Service".to_string(),
                claim_count: 42,
            },
            TopEntity {
                name: "PostgreSQL".to_string(),
                entity_type: "Database".to_string(),
                claim_count: 18,
            },
        ];
        let decisions = vec![("Use RS256 signing".to_string(), 0.92f64)];
        let text = format_workspace_brief("my-repo", 120, 450, 30, &top, &decisions, 2);

        assert!(text.contains("120 entities"));
        assert!(text.contains("450 claims"));
        assert!(text.contains("AuthService"));
        assert!(text.contains("Use RS256"));
        assert!(text.contains("⚠ 2 unresolved"));
        assert!(text.contains("investigate(entity_name)"));
    }
}
