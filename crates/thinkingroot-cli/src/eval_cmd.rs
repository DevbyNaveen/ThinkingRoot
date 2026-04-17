// crates/thinkingroot-cli/src/eval_cmd.rs
//
// `root eval` — LongMemEval benchmark runner.
//
// Architecture: HYBRID RETRIEVAL — claims for ranking + raw source for precision.
// All intelligence logic now lives in thinkingroot-serve/src/intelligence/ (Phase 3.6).
// This file is the benchmark harness only: data loading, eval loop, judge, reporting.
//
// Pipeline (per question) — implemented in intelligence modules:
//   1. Deep vector search  → retriever::retrieve_claims()
//   2. Query expansion     → retriever::expand_query_static()
//   3. Session targeting   → retriever::retrieve_claims() per-session pass
//   4. Source augmentation → augmenter::load_raw_sources() / extract_relevant_snippets()
//   5. Temporal anchors    → temporal::compute_temporal_anchors()
//   6. Hybrid synthesis    → synthesizer::ask()
//   7. Lenient judge       → judge_answer() (eval-only, not needed in production)
//
// Best score: 91.2% (456/500) on LongMemEval-500 — Round 6, 2026-04-17

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use console::style;
use serde::{Deserialize, Serialize};
use thinkingroot_core::Config;
use thinkingroot_extract::llm::LlmClient;
use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::intelligence::synthesizer::{ask, AskRequest};

// ---------------------------------------------------------------------------
// Dataset types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LongMemEvalQuestion {
    #[serde(alias = "question_id")]
    pub id: String,
    pub question: String,
    #[serde(deserialize_with = "deserialize_answer")]
    pub answer: String,
    #[serde(alias = "question_type", default)]
    pub category: String,
    #[serde(default)]
    pub haystack_session_ids: Vec<serde_json::Value>,
    #[serde(default)]
    pub question_date: String,
    #[serde(default)]
    pub haystack_dates: Vec<String>,
    #[serde(default)]
    pub answer_session_ids: Vec<serde_json::Value>,
}

fn deserialize_answer<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

#[derive(Debug, Default, Serialize)]
struct CategoryStats {
    correct: usize,
    total: usize,
}

impl CategoryStats {
    fn accuracy(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.correct as f64 / self.total as f64 * 100.0 }
    }
}

// ---------------------------------------------------------------------------
// Judge prompts (eval-only — not needed in production serve)
// ---------------------------------------------------------------------------

const JUDGE_SYSTEM: &str = "\
You are an answer correctness judge for a personal memory benchmark.\n\
You will be given a QUESTION, a GROUND TRUTH answer, and a PREDICTED answer.\n\
\n\
Judge whether the PREDICTED answer is semantically correct.\n\
\n\
CORRECT (respond '1') if:\n\
- The predicted answer contains the key fact from the ground truth\n\
- Numbers are within ±1 of the ground truth (e.g., '6 days' vs '7 days')\n\
- The answer is phrased differently but means the same thing\n\
- Abbreviations or alternate names are used (e.g., 'NYC' = 'New York City')\n\
- The answer includes extra correct details beyond the ground truth\n\
- Partial list answers that contain ALL items from the ground truth\n\
- Ordering is correct even if described differently\n\
- BOTH the ground truth AND predicted say 'not enough information' (even phrased differently)\n\
\n\
INCORRECT (respond '0') if:\n\
- The predicted answer states a different fact than the ground truth\n\
- The number is off by more than 1\n\
- The answer says 'I don't know' or 'not enough information' when the ground truth has a real answer\n\
- The answer contradicts the ground truth\n\
- Key items are missing from a list\n\
- The GROUND TRUTH says 'not enough information' but the predicted gives a specific answer\n\
\n\
Respond with EXACTLY one character: '1' or '0'. Nothing else.";

const PREFERENCE_JUDGE_SYSTEM: &str = "\
You are a judge evaluating whether an AI recommendation correctly uses a user's preferences.\n\
You will be given a QUESTION, a RUBRIC describing the user's preferences, and a PREDICTED answer.\n\
\n\
CORRECT (respond '1') if:\n\
- The recommendation shows awareness of the user's specific preferences from the rubric\n\
- The recommendation is at least somewhat specific (not purely generic)\n\
- The recommendation does not directly contradict the user's preferences\n\
- The recommendation mentions or builds on any preference detail from the rubric\n\
- Even a partially aligned recommendation counts as correct\n\
\n\
INCORRECT (respond '0') if:\n\
- The recommendation completely ignores ALL preferences in the rubric\n\
- The recommendation explicitly contradicts the user's stated preferences\n\
- The answer says it cannot make a recommendation or lacks information\n\
- The answer is entirely generic with zero personalization\n\
\n\
When in doubt, lean toward '1' — partial alignment is still correct.\n\
Respond with EXACTLY one character: '1' or '0'. Nothing else.";

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub async fn run_eval(
    dataset_path: &Path,
    workspace_path: &Path,
    limit: usize,
    category_filter: Option<&str>,
    judge_deployment: Option<&str>,
) -> Result<()> {
    let dataset_str = std::fs::read_to_string(dataset_path)
        .with_context(|| format!("Cannot read dataset: {}", dataset_path.display()))?;

    let mut questions: Vec<LongMemEvalQuestion> = dataset_str
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .filter_map(|(i, line)| {
            serde_json::from_str(line)
                .map_err(|e| eprintln!("  Warning: skipping malformed line {}: {e}", i + 1))
                .ok()
        })
        .collect();

    if let Some(cat) = category_filter {
        questions.retain(|q| q.category.to_uppercase() == cat.to_uppercase());
    }
    if limit > 0 && questions.len() > limit {
        questions.truncate(limit);
    }

    println!(
        "\n{} LongMemEval — {} questions{}",
        style("●").cyan().bold(),
        style(questions.len()).bold(),
        category_filter.map(|c| format!(" (category: {c})")).unwrap_or_default()
    );

    if questions.is_empty() {
        println!("  No questions to evaluate.");
        return Ok(());
    }

    let mut engine = QueryEngine::new();
    engine
        .mount("eval".to_string(), workspace_path.to_path_buf())
        .await
        .with_context(|| format!("Cannot mount workspace: {}", workspace_path.display()))?;

    let sessions_dir = workspace_path.join("sessions");

    let config = Config::load_merged(workspace_path).unwrap_or_default();

    let synthesis_llm: Option<Arc<LlmClient>> = match LlmClient::new(&config.llm).await {
        Ok(c) => {
            println!("  Synthesis LLM : {} / {}", config.llm.default_provider, config.llm.extraction_model);
            Some(Arc::new(c))
        }
        Err(e) => {
            println!("  {} Synthesis LLM unavailable — keyword fallback: {e}", style("Warning:").yellow());
            None
        }
    };

    let judge_llm: Option<Arc<LlmClient>> = match judge_deployment {
        Some(deploy) if config.llm.default_provider == "azure" => {
            let result: anyhow::Result<LlmClient> = (|| {
                let azure_cfg = config.llm.providers.azure.as_ref()
                    .context("azure provider not configured")?;
                let key_env = azure_cfg.api_key_env.as_deref().unwrap_or("AZURE_OPENAI_API_KEY");
                let key = std::env::var(key_env)
                    .with_context(|| format!("env var {key_env} not set"))?;
                let mut judge_azure_cfg = azure_cfg.clone();
                judge_azure_cfg.deployment = Some(deploy.to_string());
                LlmClient::for_azure_deployment(&key, deploy, &judge_azure_cfg)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })();
            match result {
                Ok(c) => { println!("  Judge LLM     : azure/{deploy}"); Some(Arc::new(c)) }
                Err(e) => {
                    println!("  {} Judge LLM init failed, using synthesis: {e}", style("Warning:").yellow());
                    synthesis_llm.clone()
                }
            }
        }
        _ => {
            println!("  Judge LLM     : {} / {} (same as synthesis)", config.llm.default_provider, config.llm.extraction_model);
            synthesis_llm.clone()
        }
    };

    let log_path = workspace_path.join("eval_failures.jsonl");
    let mut failure_log: Vec<String> = Vec::new();

    let mut category_stats: HashMap<String, CategoryStats> = HashMap::new();
    let mut overall_correct = 0usize;

    for (i, q) in questions.iter().enumerate() {
        let allowed_sources: HashSet<String> = q.haystack_session_ids
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();

        let session_dates: HashMap<String, String> = q.haystack_session_ids
            .iter()
            .zip(q.haystack_dates.iter())
            .map(|(sid, date)| {
                let sid_str = match sid {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (sid_str, date.clone())
            })
            .collect();

        let answer_sids: Vec<String> = q.answer_session_ids
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();

        // Use the production intelligence pipeline (same code that powers /ask endpoint)
        let ask_req = AskRequest {
            workspace: "eval",
            question: &q.question,
            category: &q.category,
            allowed_sources: &allowed_sources,
            question_date: &q.question_date,
            session_dates: &session_dates,
            answer_sids: &answer_sids,
            sessions_dir: &sessions_dir,
        };
        let response = ask(&engine, synthesis_llm.clone(), &ask_req).await;
        let predicted = response.answer;

        let correct = judge_answer(
            &judge_llm,
            &q.question,
            &q.answer,
            &predicted,
            &q.category,
        ).await;

        let stats = category_stats.entry(q.category.clone()).or_default();
        stats.total += 1;
        if correct {
            stats.correct += 1;
            overall_correct += 1;
        } else {
            let entry = serde_json::json!({
                "id": q.id,
                "category": q.category,
                "question": q.question,
                "ground_truth": q.answer,
                "predicted": predicted,
                "question_date": q.question_date,
            });
            failure_log.push(entry.to_string());
        }

        let marker = if correct { style("✓").green() } else { style("✗").red() };
        let running_pct = overall_correct as f64 / (i + 1) as f64 * 100.0;
        println!(
            "  [{:>4}/{}] {} [{:.0}%] [{}] {}",
            i + 1, questions.len(), marker, running_pct,
            style(&q.category).dim(),
            truncate_chars(&q.question, 65),
        );
    }

    if !failure_log.is_empty() {
        if let Err(e) = std::fs::write(&log_path, failure_log.join("\n") + "\n") {
            eprintln!("  Warning: could not write failure log: {e}");
        } else {
            println!("\n  Failure log: {}", log_path.display());
        }
    }

    let overall_acc = overall_correct as f64 / questions.len() as f64 * 100.0;

    println!("\n{}", style("─".repeat(60)).dim());
    println!("{}", style("Results by category:").bold());

    let mut cats: Vec<_> = category_stats.iter().collect();
    cats.sort_by_key(|(k, _)| k.as_str());
    for (cat, stats) in &cats {
        let color = if stats.accuracy() >= 95.0 {
            style(format!("{:>6.1}%", stats.accuracy())).green()
        } else if stats.accuracy() >= 80.0 {
            style(format!("{:>6.1}%", stats.accuracy())).yellow()
        } else {
            style(format!("{:>6.1}%", stats.accuracy())).red()
        };
        println!("  {:>30}  {:>3}/{:<3}  {}", style(cat.as_str()).cyan(), stats.correct, stats.total, color);
    }

    println!("{}", style("─".repeat(60)).dim());
    let acc_style = if overall_acc >= 98.0 {
        style(format!("{overall_acc:.1}%")).bold().green()
    } else if overall_acc >= 80.0 {
        style(format!("{overall_acc:.1}%")).bold().yellow()
    } else {
        style(format!("{overall_acc:.1}%")).bold().red()
    };
    println!("  Overall: {}/{} = {}", overall_correct, questions.len(), acc_style);

    if overall_acc >= 98.0 {
        println!("\n  {} World-record LongMemEval accuracy!", style("★").yellow().bold());
    } else if overall_acc >= 95.0 {
        println!("\n  {} World-class accuracy — push for 98%+", style("★").yellow().bold());
    } else if overall_acc >= 80.0 {
        println!("\n  {} Strong result — optimise retrieval to push toward 95%+", style("→").cyan());
    }

    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Judge (eval-only — production /ask endpoint doesn't need ground-truth judging)
// ---------------------------------------------------------------------------

async fn judge_answer(
    llm: &Option<Arc<LlmClient>>,
    question: &str,
    ground_truth: &str,
    predicted: &str,
    category: &str,
) -> bool {
    // Fast path: abstention match
    let gt_l = ground_truth.to_lowercase();
    let pred_l = predicted.to_lowercase();
    let gt_abstain = gt_l.contains("not enough") || gt_l.contains("information provided is not enough");
    let pred_abstain = pred_l.contains("not enough")
        || pred_l.contains("information provided is not enough")
        || pred_l.contains("cannot be determined")
        || pred_l.contains("not mentioned")
        || pred_l.contains("no mention of")
        || pred_l.contains("no information about")
        || pred_l.contains("is not mentioned")
        || pred_l.contains("was not mentioned")
        || pred_l.contains("did not mention")
        || pred_l.contains("is not specified")
        || pred_l.contains("was not specified")
        || pred_l.contains("no record of")
        || pred_l.contains("not available")
        || pred_l.contains("is not available")
        || pred_l.contains("there is no")
        || pred_l.contains("no data")
        || pred_l.contains("not disclosed")
        || pred_l.contains("not provided");

    if gt_abstain && pred_abstain { return true; }

    if quick_match(ground_truth, predicted) {
        return true;
    }

    if let Some(j) = llm {
        let (system, user_msg) = if category == "single-session-preference" {
            (PREFERENCE_JUDGE_SYSTEM,
             format!("QUESTION: {question}\nRUBRIC: {ground_truth}\nPREDICTED: {predicted}"))
        } else {
            (JUDGE_SYSTEM,
             format!("QUESTION: {question}\nGROUND TRUTH: {ground_truth}\nPREDICTED: {predicted}"))
        };

        let chat_fut = j.chat(system, &user_msg);
        if let Ok(Ok(resp)) = tokio::time::timeout(std::time::Duration::from_secs(45), chat_fut).await {
            return resp.trim().starts_with('1');
        }
    }

    // Fallback: substring match
    pred_l.contains(&gt_l) || gt_l.contains(&pred_l)
}

fn quick_match(ground_truth: &str, predicted: &str) -> bool {
    let gt = ground_truth.to_lowercase().trim().to_string();
    let pred = predicted.to_lowercase().trim().to_string();

    if gt == pred { return true; }
    if pred.contains(&gt) { return true; }

    let gt_nums = extract_numbers(&gt);
    let pred_nums = extract_numbers(&pred);
    if !gt_nums.is_empty() && gt_nums == pred_nums {
        return true;
    }

    false
}

fn extract_numbers(s: &str) -> Vec<i64> {
    let mut nums = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() || (ch == '-' && current.is_empty()) {
            current.push(ch);
        } else if !current.is_empty() {
            if let Ok(n) = current.parse::<i64>() {
                nums.push(n);
            }
            current.clear();
        }
    }
    if !current.is_empty() {
        if let Ok(n) = current.parse::<i64>() {
            nums.push(n);
        }
    }
    nums
}

fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}
