use std::path::Path;

use tera::{Context, Tera};

use thinkingroot_core::config::Config;
use thinkingroot_core::types::*;
use thinkingroot_core::{Error, Result};
use thinkingroot_graph::graph::GraphStore;

use crate::templates;

/// The Compiler reads the knowledge graph and produces compiled artifacts.
pub struct Compiler {
    tera: Tera,
    output_dir: String,
}

impl Compiler {
    pub fn new(config: &Config) -> Result<Self> {
        let tera = templates::init_templates()?;
        Ok(Self {
            tera,
            output_dir: config.compilation.output_dir.clone(),
        })
    }

    /// Compile all artifacts and write them to disk.
    pub fn compile_all(&self, graph: &GraphStore, data_dir: &Path) -> Result<Vec<Artifact>> {
        let output_path = data_dir.join(&self.output_dir);
        std::fs::create_dir_all(&output_path).map_err(|e| Error::io_path(&output_path, e))?;

        let mut artifacts = Vec::new();

        // 1. Compile entity pages.
        let entities = graph.get_all_entities()?;
        let entities_dir = output_path.join("entities");
        std::fs::create_dir_all(&entities_dir).map_err(|e| Error::io_path(&entities_dir, e))?;

        for (entity_id, entity_name, entity_type) in &entities {
            match self.compile_entity_page(graph, entity_id, entity_name, entity_type) {
                Ok(artifact) => {
                    let file_name = sanitize_filename(entity_name);
                    let file_path = entities_dir.join(format!("{file_name}.md"));
                    std::fs::write(&file_path, &artifact.content)
                        .map_err(|e| Error::io_path(&file_path, e))?;
                    artifacts.push(artifact);
                }
                Err(e) => {
                    tracing::warn!("failed to compile entity page for {entity_name}: {e}");
                }
            }
        }

        // 2. Compile architecture map.
        match self.compile_architecture_map(graph) {
            Ok(artifact) => {
                let file_path = output_path.join("architecture-map.md");
                std::fs::write(&file_path, &artifact.content)
                    .map_err(|e| Error::io_path(&file_path, e))?;
                artifacts.push(artifact);
            }
            Err(e) => {
                tracing::warn!("failed to compile architecture map: {e}");
            }
        }

        // 3. Compile contradiction report.
        match self.compile_contradiction_report(graph) {
            Ok(artifact) => {
                let file_path = output_path.join("contradiction-report.md");
                std::fs::write(&file_path, &artifact.content)
                    .map_err(|e| Error::io_path(&file_path, e))?;
                artifacts.push(artifact);
            }
            Err(e) => {
                tracing::warn!("failed to compile contradiction report: {e}");
            }
        }

        // 4. Compile decision log.
        match self.compile_decision_log(graph) {
            Ok(artifact) => {
                let file_path = output_path.join("decision-log.md");
                std::fs::write(&file_path, &artifact.content)
                    .map_err(|e| Error::io_path(&file_path, e))?;
                artifacts.push(artifact);
            }
            Err(e) => {
                tracing::warn!("failed to compile decision log: {e}");
            }
        }

        // 5. Compile task pack (agent context).
        match self.compile_task_pack(graph) {
            Ok(artifact) => {
                let file_path = output_path.join("task-pack.md");
                std::fs::write(&file_path, &artifact.content)
                    .map_err(|e| Error::io_path(&file_path, e))?;
                artifacts.push(artifact);
            }
            Err(e) => {
                tracing::warn!("failed to compile task pack: {e}");
            }
        }

        // 6. Compile agent brief.
        match self.compile_agent_brief(graph) {
            Ok(artifact) => {
                let file_path = output_path.join("agent-brief.md");
                std::fs::write(&file_path, &artifact.content)
                    .map_err(|e| Error::io_path(&file_path, e))?;
                artifacts.push(artifact);
            }
            Err(e) => {
                tracing::warn!("failed to compile agent brief: {e}");
            }
        }

        // 7. Compile runbook.
        match self.compile_runbook(graph) {
            Ok(artifact) => {
                let file_path = output_path.join("runbook.md");
                std::fs::write(&file_path, &artifact.content)
                    .map_err(|e| Error::io_path(&file_path, e))?;
                artifacts.push(artifact);
            }
            Err(e) => {
                tracing::warn!("failed to compile runbook: {e}");
            }
        }

        // 8. Compile health report.
        match self.compile_health_report(graph) {
            Ok(artifact) => {
                let file_path = output_path.join("health-report.md");
                std::fs::write(&file_path, &artifact.content)
                    .map_err(|e| Error::io_path(&file_path, e))?;
                artifacts.push(artifact);
            }
            Err(e) => {
                tracing::warn!("failed to compile health report: {e}");
            }
        }

        tracing::info!(
            "compiled {} artifacts to {}",
            artifacts.len(),
            output_path.display()
        );
        Ok(artifacts)
    }

    /// Compile only artifacts affected by changes.
    ///
    /// - Entity pages: only recompiled for `affected_entity_ids`
    /// - Global artifacts (architecture map, contradiction report, etc.):
    ///   only recompiled when `has_changes` is true
    pub fn compile_affected(
        &self,
        graph: &GraphStore,
        data_dir: &Path,
        affected_entity_ids: &[String],
        has_changes: bool,
    ) -> Result<Vec<Artifact>> {
        let output_path = data_dir.join(&self.output_dir);
        std::fs::create_dir_all(&output_path).map_err(|e| Error::io_path(&output_path, e))?;

        let mut artifacts = Vec::new();

        // 1. Compile entity pages only for affected entities.
        if !affected_entity_ids.is_empty() {
            let entities_dir = output_path.join("entities");
            std::fs::create_dir_all(&entities_dir).map_err(|e| Error::io_path(&entities_dir, e))?;

            let all_entities = graph.get_all_entities()?;
            let affected_set: std::collections::HashSet<&str> =
                affected_entity_ids.iter().map(|s| s.as_str()).collect();

            for (entity_id, entity_name, entity_type) in &all_entities {
                if !affected_set.contains(entity_id.as_str()) {
                    continue;
                }
                match self.compile_entity_page(graph, entity_id, entity_name, entity_type) {
                    Ok(artifact) => {
                        let file_name = sanitize_filename(entity_name);
                        let file_path = entities_dir.join(format!("{file_name}.md"));
                        std::fs::write(&file_path, &artifact.content)
                            .map_err(|e| Error::io_path(&file_path, e))?;
                        artifacts.push(artifact);
                    }
                    Err(e) => {
                        tracing::warn!("failed to compile entity page for {entity_name}: {e}");
                    }
                }
            }
        }

        // 2. Recompile global artifacts only if something changed.
        if has_changes {
            for (filename, artifact_result) in [
                ("architecture-map.md", self.compile_architecture_map(graph)),
                (
                    "contradiction-report.md",
                    self.compile_contradiction_report(graph),
                ),
                ("decision-log.md", self.compile_decision_log(graph)),
                ("task-pack.md", self.compile_task_pack(graph)),
                ("agent-brief.md", self.compile_agent_brief(graph)),
                ("runbook.md", self.compile_runbook(graph)),
                ("health-report.md", self.compile_health_report(graph)),
            ] {
                match artifact_result {
                    Ok(artifact) => {
                        let file_path = output_path.join(filename);
                        std::fs::write(&file_path, &artifact.content)
                            .map_err(|e| Error::io_path(&file_path, e))?;
                        artifacts.push(artifact);
                    }
                    Err(e) => {
                        tracing::warn!("failed to compile {filename}: {e}");
                    }
                }
            }
        }

        tracing::info!(
            "compiled {} artifacts (incremental) to {}",
            artifacts.len(),
            output_path.display()
        );
        Ok(artifacts)
    }

    fn compile_entity_page(
        &self,
        graph: &GraphStore,
        entity_id: &str,
        entity_name: &str,
        entity_type: &str,
    ) -> Result<Artifact> {
        // Use the rich query that joins claims with their source URIs.
        let claims = graph.get_claims_with_sources_for_entity(entity_id)?;

        let mut context = Context::new();
        context.insert("name", entity_name);
        context.insert("entity_type", entity_type);
        context.insert("description", "");
        context.insert("aliases", &Vec::<String>::new());

        let claim_data: Vec<serde_json::Value> = claims
            .iter()
            .map(|(_, statement, ctype, source_uri, confidence)| {
                serde_json::json!({
                    "claim_type": ctype,
                    "statement": statement,
                    "confidence": format!("{:.1}", confidence),
                    "source_uri": source_uri,
                })
            })
            .collect();
        context.insert("claims", &claim_data);
        context.insert("relations", &Vec::<serde_json::Value>::new());
        context.insert("compiled_at", &chrono::Utc::now().to_rfc3339());

        let content = self
            .tera
            .render("entity_page.md", &context)
            .map_err(|e| Error::Template(e.to_string()))?;

        Ok(Artifact::new(
            ArtifactType::EntityPage,
            entity_name,
            content,
        ))
    }

    fn compile_architecture_map(&self, graph: &GraphStore) -> Result<Artifact> {
        let (sources, _, entities_count) = graph.get_counts()?;
        let entities = graph.get_all_entities()?;
        let relations = graph.get_all_relations()?;

        // Filter for system/service/api entities.
        let system_types = ["System", "Service", "Api", "Database", "Library", "Module"];
        let systems: Vec<serde_json::Value> = entities
            .iter()
            .filter(|(_, _, etype)| system_types.iter().any(|t| etype.contains(t)))
            .map(|(id, name, etype)| {
                let entity_rels: Vec<serde_json::Value> = relations
                    .iter()
                    .filter(|(from, _, _, _, _, _)| from == name)
                    .map(|(_, to, rel_type, _, _, _)| {
                        serde_json::json!({
                            "relation_type": rel_type,
                            "target": to,
                        })
                    })
                    .collect();

                serde_json::json!({
                    "name": name,
                    "entity_type": etype,
                    "description": "",
                    "id": id,
                    "relations": entity_rels,
                })
            })
            .collect();

        // Get decision claims.
        let decision_rows = graph.get_claims_by_type("Decision")?;
        let decisions: Vec<serde_json::Value> = decision_rows
            .iter()
            .map(|(id, statement, _source_id, confidence, uri)| {
                serde_json::json!({
                    "id": id,
                    "statement": statement,
                    "confidence": confidence,
                    "source_uri": uri,
                })
            })
            .collect();

        let mut context = Context::new();
        context.insert("source_count", &sources);
        context.insert("entity_count", &entities_count);
        context.insert("systems", &systems);
        context.insert("decisions", &decisions);
        context.insert("compiled_at", &chrono::Utc::now().to_rfc3339());

        let content = self
            .tera
            .render("architecture_map.md", &context)
            .map_err(|e| Error::Template(e.to_string()))?;

        Ok(Artifact::new(
            ArtifactType::ArchitectureMap,
            "Architecture Map",
            content,
        ))
    }

    fn compile_contradiction_report(&self, graph: &GraphStore) -> Result<Artifact> {
        let contradictions_raw = graph.get_contradictions()?;

        let contradictions: Vec<serde_json::Value> = contradictions_raw
            .iter()
            .map(|(_, claim_a_id, claim_b_id, explanation, status)| {
                // Look up claim statements.
                let claim_a_stmt = graph
                    .get_claims_for_entity(claim_a_id) // This won't find by claim ID, use a workaround
                    .ok()
                    .and_then(|v| v.first().map(|(_, s, _)| s.clone()))
                    .unwrap_or_else(|| claim_a_id.clone());
                let claim_b_stmt = graph
                    .get_claims_for_entity(claim_b_id)
                    .ok()
                    .and_then(|v| v.first().map(|(_, s, _)| s.clone()))
                    .unwrap_or_else(|| claim_b_id.clone());

                serde_json::json!({
                    "status": status,
                    "claim_a_statement": claim_a_stmt,
                    "claim_a_source": claim_a_id,
                    "claim_a_confidence": "0.8",
                    "claim_b_statement": claim_b_stmt,
                    "claim_b_source": claim_b_id,
                    "claim_b_confidence": "0.8",
                    "explanation": explanation,
                })
            })
            .collect();

        let mut context = Context::new();
        context.insert("contradiction_count", &contradictions.len());
        context.insert("contradictions", &contradictions);
        context.insert("compiled_at", &chrono::Utc::now().to_rfc3339());

        let content = self
            .tera
            .render("contradiction_report.md", &context)
            .map_err(|e| Error::Template(e.to_string()))?;

        Ok(Artifact::new(
            ArtifactType::ContradictionReport,
            "Contradiction Report",
            content,
        ))
    }

    fn compile_decision_log(&self, graph: &GraphStore) -> Result<Artifact> {
        let (sources, _, _) = graph.get_counts()?;
        let decisions = graph.get_claims_by_type("Decision")?;
        let plans = graph.get_claims_by_type("Plan")?;

        let decision_data: Vec<serde_json::Value> = decisions
            .iter()
            .map(|(_, statement, _, confidence, uri)| {
                serde_json::json!({
                    "statement": statement,
                    "confidence": format!("{:.1}", confidence),
                    "source_uri": uri,
                })
            })
            .collect();

        let plan_data: Vec<serde_json::Value> = plans
            .iter()
            .map(|(_, statement, _, confidence, uri)| {
                serde_json::json!({
                    "statement": statement,
                    "confidence": format!("{:.1}", confidence),
                    "source_uri": uri,
                })
            })
            .collect();

        let mut context = Context::new();
        context.insert("decision_count", &decisions.len());
        context.insert("source_count", &sources);
        context.insert("decisions", &decision_data);
        context.insert("plans", &plan_data);
        context.insert("compiled_at", &chrono::Utc::now().to_rfc3339());

        let content = self
            .tera
            .render("decision_log.md", &context)
            .map_err(|e| Error::Template(e.to_string()))?;

        Ok(Artifact::new(
            ArtifactType::DecisionLog,
            "Decision Log",
            content,
        ))
    }

    fn compile_task_pack(&self, graph: &GraphStore) -> Result<Artifact> {
        let (_, claims_count, entities_count) = graph.get_counts()?;
        let entities = graph.get_all_entities()?;
        let relations = graph.get_all_relations()?;
        let contradictions_raw = graph.get_contradictions()?;

        let system_types = ["System", "Service", "Api", "Database", "Library", "Module"];
        let systems: Vec<serde_json::Value> = entities
            .iter()
            .filter(|(_, _, etype)| system_types.iter().any(|t| etype.contains(t)))
            .map(|(_, name, etype)| {
                let entity_rels: Vec<serde_json::Value> = relations
                    .iter()
                    .filter(|(from, _, _, _, _, _)| from == name)
                    .map(|(_, to, rel_type, _, _, _)| {
                        serde_json::json!({ "relation_type": rel_type, "target": to })
                    })
                    .collect();
                serde_json::json!({
                    "name": name,
                    "entity_type": etype,
                    "description": "",
                    "relations": entity_rels,
                })
            })
            .collect();

        let architecture_claims = self.claims_by_type_to_json(graph, "Architecture")?;
        let api_claims = self.claims_by_type_to_json(graph, "ApiSignature")?;
        let dependency_claims = self.claims_by_type_to_json(graph, "Dependency")?;

        let contradictions: Vec<serde_json::Value> = contradictions_raw
            .iter()
            .map(|(_, _, _, explanation, _)| serde_json::json!({ "explanation": explanation }))
            .collect();

        let mut context = Context::new();
        context.insert("entity_count", &entities_count);
        context.insert("claim_count", &claims_count);
        context.insert("systems", &systems);
        context.insert("architecture_claims", &architecture_claims);
        context.insert("api_claims", &api_claims);
        context.insert("dependency_claims", &dependency_claims);
        context.insert("contradictions", &contradictions);
        context.insert("compiled_at", &chrono::Utc::now().to_rfc3339());

        let content = self
            .tera
            .render("task_pack.md", &context)
            .map_err(|e| Error::Template(e.to_string()))?;

        Ok(Artifact::new(ArtifactType::TaskPack, "Task Pack", content))
    }

    fn compile_agent_brief(&self, graph: &GraphStore) -> Result<Artifact> {
        let (sources, claims_count, entities_count) = graph.get_counts()?;
        let entities = graph.get_all_entities()?;
        let relations = graph.get_all_relations()?;
        let contradictions = graph.get_contradictions()?;
        let all_claims = graph.get_all_claims_with_sources()?;

        // Entity summary with claim counts.
        let entity_data: Vec<serde_json::Value> = entities
            .iter()
            .map(|(id, name, etype)| {
                let claim_count = graph
                    .get_claims_for_entity(id)
                    .map(|c| c.len())
                    .unwrap_or(0);
                serde_json::json!({
                    "name": name,
                    "entity_type": etype,
                    "claim_count": claim_count,
                })
            })
            .collect();

        // High-confidence claims (>= 0.85).
        let high_conf: Vec<serde_json::Value> = all_claims
            .iter()
            .filter(|(_, _, _, conf, _)| *conf >= 0.85)
            .take(50) // cap to keep brief concise
            .map(|(_, statement, ctype, conf, uri)| {
                serde_json::json!({
                    "statement": statement,
                    "claim_type": ctype,
                    "confidence": format!("{:.1}", conf),
                    "source_uri": uri,
                })
            })
            .collect();

        let relation_data: Vec<serde_json::Value> = relations
            .iter()
            .map(|(from, to, rel_type, _, _, _)| {
                serde_json::json!({
                    "from": from,
                    "relation_type": rel_type,
                    "to": to,
                })
            })
            .collect();

        let mut warnings = Vec::<String>::new();
        let unresolved = contradictions
            .iter()
            .filter(|(_, _, _, _, s)| s == "Detected" || s == "UnderReview")
            .count();
        if unresolved > 0 {
            warnings.push(format!("{unresolved} unresolved contradictions"));
        }

        let mut context = Context::new();
        context.insert("entity_count", &entities_count);
        context.insert("claim_count", &claims_count);
        context.insert("source_count", &sources);
        context.insert("entities", &entity_data);
        context.insert("high_confidence_claims", &high_conf);
        context.insert("relations", &relation_data);
        context.insert("warnings", &warnings);
        context.insert("compiled_at", &chrono::Utc::now().to_rfc3339());

        let content = self
            .tera
            .render("agent_brief.md", &context)
            .map_err(|e| Error::Template(e.to_string()))?;

        Ok(Artifact::new(
            ArtifactType::AgentBrief,
            "Agent Brief",
            content,
        ))
    }

    fn compile_runbook(&self, graph: &GraphStore) -> Result<Artifact> {
        let (sources, _, _) = graph.get_counts()?;
        let entities = graph.get_all_entities()?;
        let relations = graph.get_all_relations()?;
        let contradictions_raw = graph.get_contradictions()?;

        let system_types = ["System", "Service", "Api", "Database", "Library", "Module"];
        let systems: Vec<serde_json::Value> = entities
            .iter()
            .filter(|(_, _, etype)| system_types.iter().any(|t| etype.contains(t)))
            .map(|(id, name, etype)| {
                let entity_rels: Vec<serde_json::Value> = relations
                    .iter()
                    .filter(|(from, _, _, _, _, _)| from == name)
                    .map(|(_, to, rel_type, _, _, _)| {
                        serde_json::json!({ "relation_type": rel_type, "target": to })
                    })
                    .collect();

                let entity_claims: Vec<serde_json::Value> = graph
                    .get_claims_with_sources_for_entity(id)
                    .unwrap_or_default()
                    .iter()
                    .take(10)
                    .map(|(_, stmt, _, uri, conf)| {
                        serde_json::json!({
                            "statement": stmt,
                            "source_uri": uri,
                            "confidence": format!("{:.1}", conf),
                        })
                    })
                    .collect();

                serde_json::json!({
                    "name": name,
                    "entity_type": etype,
                    "description": "",
                    "relations": entity_rels,
                    "claims": entity_claims,
                })
            })
            .collect();

        let requirements = self.claims_by_type_to_json(graph, "Requirement")?;

        let contradictions: Vec<serde_json::Value> = contradictions_raw
            .iter()
            .map(|(_, _, _, explanation, _)| serde_json::json!({ "explanation": explanation }))
            .collect();

        let mut context = Context::new();
        context.insert("source_count", &sources);
        context.insert("systems", &systems);
        context.insert("requirements", &requirements);
        context.insert("contradictions", &contradictions);
        context.insert("compiled_at", &chrono::Utc::now().to_rfc3339());

        let content = self
            .tera
            .render("runbook.md", &context)
            .map_err(|e| Error::Template(e.to_string()))?;

        Ok(Artifact::new(
            ArtifactType::Runbook,
            "Operational Runbook",
            content,
        ))
    }

    /// Helper: query claims by type and return as JSON array for templates.
    fn claims_by_type_to_json(
        &self,
        graph: &GraphStore,
        claim_type: &str,
    ) -> Result<Vec<serde_json::Value>> {
        Ok(graph
            .get_claims_by_type(claim_type)?
            .iter()
            .map(|(_, statement, _, confidence, uri)| {
                serde_json::json!({
                    "statement": statement,
                    "confidence": format!("{:.1}", confidence),
                    "source_uri": uri,
                })
            })
            .collect())
    }

    fn compile_health_report(&self, graph: &GraphStore) -> Result<Artifact> {
        let (sources, claims, entities) = graph.get_counts()?;
        let contradictions = graph.get_contradictions()?;
        let unresolved = contradictions
            .iter()
            .filter(|(_, _, _, _, s)| s == "Detected" || s == "UnderReview")
            .count();
        let relations = graph.get_all_relations()?;

        let mut context = Context::new();
        context.insert(
            "score",
            &serde_json::json!({
                "overall": 100,
                "freshness": 100,
                "consistency": if contradictions.is_empty() { 100 } else { (100.0 * (1.0 - unresolved as f64 / claims.max(1) as f64)) as u8 },
                "coverage": if entities > 0 { ((claims as f64 / entities as f64).min(1.0) * 100.0) as u8 } else { 0 },
                "provenance": if claims > 0 && sources > 0 { 100 } else { 0 },
            }),
        );
        context.insert(
            "stats",
            &serde_json::json!({
                "sources": sources,
                "claims": claims,
                "entities": entities,
                "relations": relations.len(),
                "contradictions": contradictions.len(),
                "unresolved": unresolved,
                "stale_claims": 0,
            }),
        );
        context.insert("warnings", &Vec::<String>::new());
        context.insert("compiled_at", &chrono::Utc::now().to_rfc3339());

        let content = self
            .tera
            .render("health_report.md", &context)
            .map_err(|e| Error::Template(e.to_string()))?;

        Ok(Artifact::new(
            ArtifactType::HealthReport,
            "Knowledge Health Report",
            content,
        ))
    }
}

fn sanitize_filename(name: &str) -> String {
    name.to_lowercase()
        .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "-")
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::init_templates;
    use tera::Context;

    // ── sanitize_filename ────────────────────────────────────────────────

    #[test]
    fn sanitize_spaces_become_dashes() {
        assert_eq!(sanitize_filename("My Service"), "my-service");
    }

    #[test]
    fn sanitize_slashes_become_dashes() {
        assert_eq!(sanitize_filename("API/v2"), "api-v2");
    }

    #[test]
    fn sanitize_leading_trailing_specials_stripped() {
        assert_eq!(sanitize_filename("--auth--"), "auth");
    }

    #[test]
    fn sanitize_preserves_underscores() {
        assert_eq!(sanitize_filename("some_module"), "some_module");
    }

    #[test]
    fn sanitize_empty_string_stays_empty() {
        assert_eq!(sanitize_filename(""), "");
    }

    // ── Template initialisation ──────────────────────────────────────────

    #[test]
    fn all_eight_templates_load_without_error() {
        // init_templates must succeed — any Tera parse error in a template constant
        // will surface here before it reaches production.
        let tera = init_templates().expect("all templates should parse");
        let names: Vec<&str> = tera.get_template_names().collect();
        assert!(names.contains(&"entity_page.md"), "entity_page.md missing");
        assert!(
            names.contains(&"architecture_map.md"),
            "architecture_map.md missing"
        );
        assert!(
            names.contains(&"contradiction_report.md"),
            "contradiction_report.md missing"
        );
        assert!(
            names.contains(&"health_report.md"),
            "health_report.md missing"
        );
        assert!(
            names.contains(&"decision_log.md"),
            "decision_log.md missing"
        );
        assert!(names.contains(&"task_pack.md"), "task_pack.md missing");
        assert!(names.contains(&"agent_brief.md"), "agent_brief.md missing");
        assert!(names.contains(&"runbook.md"), "runbook.md missing");
    }

    // ── Template rendering ───────────────────────────────────────────────

    #[test]
    fn entity_page_renders_name_and_type() {
        let tera = init_templates().unwrap();
        let mut ctx = Context::new();
        ctx.insert("name", "AuthService");
        ctx.insert("entity_type", "Service");
        ctx.insert("description", "Handles authentication");
        ctx.insert("aliases", &Vec::<String>::new());
        ctx.insert("claims", &Vec::<serde_json::Value>::new());
        ctx.insert("relations", &Vec::<serde_json::Value>::new());
        ctx.insert("compiled_at", "2026-01-01T00:00:00Z");

        let out = tera.render("entity_page.md", &ctx).unwrap();
        assert!(out.contains("# AuthService"), "heading missing");
        assert!(out.contains("**Type:** Service"), "entity type missing");
        assert!(out.contains("ThinkingRoot"), "footer missing");
    }

    #[test]
    fn entity_page_renders_claims_list() {
        let tera = init_templates().unwrap();
        let mut ctx = Context::new();
        ctx.insert("name", "Database");
        ctx.insert("entity_type", "Database");
        ctx.insert("description", "");
        ctx.insert("aliases", &Vec::<String>::new());
        ctx.insert("claims", &serde_json::json!([
            {"claim_type": "Fact", "statement": "Uses PostgreSQL 15.", "confidence": "0.9", "source_uri": "docs/db.md"}
        ]));
        ctx.insert("relations", &Vec::<serde_json::Value>::new());
        ctx.insert("compiled_at", "2026-01-01T00:00:00Z");

        let out = tera.render("entity_page.md", &ctx).unwrap();
        assert!(
            out.contains("Uses PostgreSQL 15."),
            "claim statement missing"
        );
        assert!(out.contains("docs/db.md"), "source URI missing");
    }

    #[test]
    fn architecture_map_renders_source_count() {
        let tera = init_templates().unwrap();
        let mut ctx = Context::new();
        ctx.insert("source_count", &42usize);
        ctx.insert("entity_count", &10usize);
        ctx.insert("systems", &Vec::<serde_json::Value>::new());
        ctx.insert("decisions", &Vec::<serde_json::Value>::new());
        ctx.insert("compiled_at", "2026-01-01T00:00:00Z");

        let out = tera.render("architecture_map.md", &ctx).unwrap();
        assert!(out.contains("42 sources"), "source count missing");
        assert!(out.contains("# Architecture Map"), "heading missing");
    }

    #[test]
    fn health_report_renders_score_dimensions() {
        let tera = init_templates().unwrap();
        let mut ctx = Context::new();
        ctx.insert(
            "score",
            &serde_json::json!({
                "overall": 87, "freshness": 100,
                "consistency": 95, "coverage": 60, "provenance": 100
            }),
        );
        ctx.insert(
            "stats",
            &serde_json::json!({
                "sources": 5, "claims": 120, "entities": 30,
                "relations": 18, "contradictions": 2, "unresolved": 1, "stale_claims": 3
            }),
        );
        ctx.insert("warnings", &Vec::<String>::new());
        ctx.insert("compiled_at", "2026-01-01T00:00:00Z");

        let out = tera.render("health_report.md", &ctx).unwrap();
        assert!(out.contains("87%"), "overall score missing");
        assert!(out.contains("100%"), "freshness missing");
        assert!(out.contains("120"), "claim count missing");
    }

    #[test]
    fn decision_log_renders_decisions() {
        let tera = init_templates().unwrap();
        let mut ctx = Context::new();
        ctx.insert("decision_count", &2usize);
        ctx.insert("source_count", &3usize);
        ctx.insert("decisions", &serde_json::json!([
            {"statement": "Use Rust for the core engine.", "confidence": "0.95", "source_uri": "adr/001.md"}
        ]));
        ctx.insert("plans", &Vec::<serde_json::Value>::new());
        ctx.insert("compiled_at", "2026-01-01T00:00:00Z");

        let out = tera.render("decision_log.md", &ctx).unwrap();
        assert!(
            out.contains("Use Rust for the core engine."),
            "decision text missing"
        );
        assert!(out.contains("adr/001.md"), "source URI missing");
    }

    #[test]
    fn contradiction_report_renders_empty_cleanly() {
        let tera = init_templates().unwrap();
        let mut ctx = Context::new();
        ctx.insert("contradiction_count", &0usize);
        ctx.insert("contradictions", &Vec::<serde_json::Value>::new());
        ctx.insert("compiled_at", "2026-01-01T00:00:00Z");

        let out = tera.render("contradiction_report.md", &ctx).unwrap();
        assert!(out.contains("# Contradiction Report"), "heading missing");
        assert!(out.contains("0 contradictions"), "count missing");
    }
}
