use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Emitter};

use crate::AppState;
use crate::db::{first_non_empty_setting, get_app_setting_value, insert_message, with_db};
use crate::models::*;

/// Convert stored structured history into Anthropic-style messages.
fn history_to_anthropic(history: &[HistoryMessage]) -> Vec<Value> {
    history
        .iter()
        .filter(|h| !h.blocks.is_empty())
        .map(|h| json!({ "role": h.role, "content": h.blocks }))
        .collect()
}

/// Convert stored structured history into OpenAI chat/completions messages. A single
/// HistoryMessage can expand into multiple OpenAI messages because each tool_result block
/// becomes its own `role:"tool"` entry.
fn history_to_openai(history: &[HistoryMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for h in history {
        match h.role.as_str() {
            "assistant" => {
                let mut text_acc = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for b in &h.blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                text_acc.push_str(t);
                            }
                        }
                        Some("tool_use") => {
                            let id = b.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                            let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                            let args = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": args}
                            }));
                        }
                        _ => {}
                    }
                }
                let mut msg = json!({"role": "assistant"});
                if !text_acc.is_empty() {
                    msg["content"] = json!(text_acc);
                } else {
                    msg["content"] = Value::Null;
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            "user" => {
                let mut text_acc = String::new();
                for b in &h.blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                if !text_acc.is_empty() {
                                    text_acc.push('\n');
                                }
                                text_acc.push_str(t);
                            }
                        }
                        Some("tool_result") => {
                            let id = b
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let content_str = match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Array(arr)) => {
                                    let mut s = String::new();
                                    for c in arr {
                                        if let Some(t) = c.get("text").and_then(Value::as_str) {
                                            s.push_str(t);
                                        }
                                    }
                                    s
                                }
                                Some(v) => v.to_string(),
                                None => String::new(),
                            };
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": content_str,
                            }));
                        }
                        _ => {}
                    }
                }
                if !text_acc.is_empty() {
                    out.push(json!({"role": "user", "content": text_acc}));
                }
            }
            _ => {
                let text = h.plain_text();
                if !text.is_empty() {
                    out.push(json!({"role": h.role, "content": text}));
                }
            }
        }
    }
    out
}

/// One-line summary of a user tool_result turn for DB `content` (used only when row happens to
/// be visible in UI — intermediate rounds are hidden, but we still store a readable fallback).
fn tool_result_summary(blocks: &[Value]) -> String {
    let mut names: Vec<String> = Vec::new();
    for b in blocks {
        if b.get("type").and_then(Value::as_str) == Some("tool_result") {
            if let Some(id) = b.get("tool_use_id").and_then(Value::as_str) {
                names.push(id.to_string());
            }
        }
    }
    if names.is_empty() {
        "[tool results]".into()
    } else {
        format!("[tool results: {}]", names.join(", "))
    }
}
use crate::tools::{
    enabled_tool_ids, execute_agent_tool, log_tool, parse_tool_code_block,
    truncate_chars, WorkspaceScope,
};

/// Check if a stop has been requested for this session (and clear the flag).
/// Recovers from a poisoned lock so a panic elsewhere can't wedge the stop signal.
fn check_stop_requested(state: &AppState, session_id: &str) -> bool {
    let mut stops = state
        .stop_requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    stops.remove(session_id)
}

// --- Embeddings & Semantic Retrieval ---

fn f32_vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_f32_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Call OpenAI embeddings API to get a vector for the given text.
pub(crate) async fn embed_text(state: &AppState, text: &str) -> Result<Vec<f32>, String> {
    let api_key = first_non_empty_setting(state, &["llm/openai_api_key"])
        .ok()
        .flatten()
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .ok_or_else(|| "OpenAI API key required for embeddings".to_string())?;

    let body = json!({
        "model": "text-embedding-3-small",
        "input": truncate_chars(text, 8000)
    });

    let resp = state
        .http
        .post("https://api.openai.com/v1/embeddings")
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Embeddings request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Embeddings API error: {}", resp.status()));
    }

    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    let embedding = json
        .get("data")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|d| d.get("embedding"))
        .and_then(Value::as_array)
        .ok_or_else(|| "Invalid embeddings response".to_string())?;

    Ok(embedding
        .iter()
        .filter_map(|v| v.as_f64().map(|f| f as f32))
        .collect())
}

/// Store an embedding for an entity or episode.
pub(crate) fn store_embedding(state: &AppState, source_type: &str, source_id: i64, content: &str, embedding: &[f32]) {
    let blob = f32_vec_to_blob(embedding);
    let _ = with_db(state, |db| {
        db.execute(
            r#"INSERT INTO embeddings (source_type, source_id, content, embedding, updated_at)
               VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP)
               ON CONFLICT(source_type, source_id) DO UPDATE SET
                 content = excluded.content,
                 embedding = excluded.embedding,
                 updated_at = CURRENT_TIMESTAMP"#,
            params![source_type, source_id, content, blob],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    });
}

/// Cosine similarity against a query whose magnitude is already known. Lets the
/// retrieval loop avoid recomputing the (large, fixed) query magnitude once per
/// candidate entity. Returns 0 on dimension mismatch.
fn cosine_with_query_mag(query: &[f32], query_mag: f32, b: &[f32]) -> f32 {
    if query_mag == 0.0 || query.len() != b.len() || b.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut mag_b_sq = 0.0f32;
    for (x, y) in query.iter().zip(b) {
        dot += x * y;
        mag_b_sq += y * y;
    }
    let mag_b = mag_b_sq.sqrt();
    if mag_b == 0.0 { 0.0 } else { dot / (query_mag * mag_b) }
}

/// Retrieve top-k entity IDs most relevant to the query embedding.
///
/// SQLite has no native vector index, so we still scan the `entity` embeddings
/// (the `UNIQUE(source_type, source_id)` constraint indexes the `source_type`
/// filter). The hot path is kept tight: the query magnitude is computed once
/// rather than per row, decoding/scoring happens in a single pass, and we select
/// the top-k with an O(n) partial sort instead of a full O(n log n) sort.
pub(crate) fn retrieve_relevant_entity_ids(state: &AppState, query_embedding: &[f32], top_k: usize) -> Vec<(i64, f32)> {
    if query_embedding.is_empty() || top_k == 0 {
        return Vec::new();
    }
    let query_mag: f32 = query_embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    if query_mag == 0.0 {
        return Vec::new();
    }

    let mut scored: Vec<(i64, f32)> = with_db(state, |db| {
        let mut stmt = db
            .prepare("SELECT source_id, embedding FROM embeddings WHERE source_type = 'entity'")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows.flatten() {
            let (id, blob) = row;
            let emb = blob_to_f32_vec(&blob);
            out.push((id, cosine_with_query_mag(query_embedding, query_mag, &emb)));
        }
        Ok(out)
    })
    .unwrap_or_default();

    if scored.len() > top_k {
        scored.select_nth_unstable_by(top_k - 1, |a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(top_k);
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// Retrieve relevant entities for a user message. Returns formatted context string.
pub(crate) async fn retrieve_relevant_memory(state: &AppState, user_message: &str) -> Option<Vec<(i64, f32)>> {
    // Check if we have any embeddings at all
    let count: i64 = with_db(state, |db| {
        db.query_row("SELECT COUNT(*) FROM embeddings WHERE source_type = 'entity'", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    })
    .unwrap_or(0);

    if count == 0 {
        return None;
    }

    let query_embedding = embed_text(state, user_message).await.ok()?;
    let relevant = retrieve_relevant_entity_ids(state, &query_embedding, 15);

    if relevant.is_empty() {
        None
    } else {
        Some(relevant)
    }
}

/// Resolved configuration for a one-shot (non-streaming) helper LLM call.
pub(crate) struct LlmCallConfig {
    #[allow(dead_code)]
    pub provider: String,
    pub model: String,
    pub is_anthropic: bool,
    pub api_key: String,
    pub endpoint: String,
}

/// Resolve provider / model / API key / endpoint for the one-shot helper LLM calls
/// (entity-summary compaction, feed-article generation, memory extraction). These three
/// call sites previously duplicated this block verbatim. The interactive chat + agent
/// paths use `Provider::openai` / `Provider::minimax_anthropic` instead and are unaffected.
///
/// Routing is provider-driven (`llm/provider` setting): `minimax`/`anthropic` use the
/// Anthropic messages wire format; every other provider uses the OpenAI chat-completions
/// format with a per-provider endpoint. Returns `Err` if no API key is configured.
pub(crate) fn resolve_llm_config(state: &AppState) -> Result<LlmCallConfig, String> {
    let provider = get_app_setting_value(state, "llm/provider")
        .ok()
        .flatten()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "minimax".to_string());

    let model = get_app_setting_value(state, "llm/model")
        .ok()
        .flatten()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| {
            if provider == "minimax" {
                "MiniMax-M2.7".to_string()
            } else {
                "gpt-4.1-mini".to_string()
            }
        });

    let (api_key_opt, endpoint, is_anthropic) = if provider == "minimax" {
        let key = first_non_empty_setting(state, &["llm/minimax_api_key", "llm/anthropic_api_key"])
            .ok()
            .flatten()
            .or_else(|| std::env::var("MINIMAX_API_KEY").ok())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());
        let base = std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimax.io/anthropic".to_string());
        let ep = if base.ends_with("/v1/messages") {
            base
        } else {
            format!("{}/v1/messages", base.trim_end_matches('/'))
        };
        (key, ep, true)
    } else if provider == "anthropic" {
        let key = first_non_empty_setting(state, &["llm/anthropic_api_key"])
            .ok()
            .flatten()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());
        (key, "https://api.anthropic.com/v1/messages".to_string(), true)
    } else {
        let key_setting = format!("llm/{}_api_key", provider);
        let key = first_non_empty_setting(state, &[&key_setting, "llm/openai_api_key"])
            .ok()
            .flatten()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok());
        let ep = match provider.as_str() {
            "openai" => "https://api.openai.com/v1/chat/completions".to_string(),
            "together" => "https://api.together.xyz/v1/chat/completions".to_string(),
            "groq" => "https://api.groq.com/openai/v1/chat/completions".to_string(),
            "deepseek" => "https://api.deepseek.com/chat/completions".to_string(),
            "xai" => "https://api.x.ai/v1/chat/completions".to_string(),
            "google" => "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions".to_string(),
            "mistral" => "https://api.mistral.ai/v1/chat/completions".to_string(),
            "cohere" => "https://api.cohere.com/compatibility/v1/chat/completions".to_string(),
            "fireworks" => "https://api.fireworks.ai/inference/v1/chat/completions".to_string(),
            "openrouter" => "https://openrouter.ai/api/v1/chat/completions".to_string(),
            _ => "https://api.openai.com/v1/chat/completions".to_string(),
        };
        (key, ep, false)
    };

    let api_key = api_key_opt
        .filter(|k| !k.is_empty())
        .ok_or_else(|| "No API key configured".to_string())?;

    Ok(LlmCallConfig {
        provider,
        model,
        is_anthropic,
        api_key,
        endpoint,
    })
}

/// Compact an entity summary that has grown too long.
pub(crate) async fn compact_entity_summary(
    state: &AppState,
    entity_id: i64,
    entity_name: &str,
    current_summary: &str,
) -> Option<String> {
    // Only compact if summary is long enough to warrant it
    if current_summary.len() < 500 {
        return None;
    }

    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state).ok()?;

    let prompt = format!(
        "This entity has accumulated history over time. Rewrite it as a concise current-state summary (2-4 sentences), \
        keeping only what's still relevant and important. Preserve key dates for major milestones.\n\n\
        Entity: {entity_name}\nHistory:\n{current_summary}\n\n\
        Respond with ONLY the compacted summary text, no explanation."
    );

    let request_body = if is_anthropic {
        json!({
            "model": model,
            "max_tokens": 512,
            "stream": false,
            "system": "You are a concise summarizer. Respond with only the summary text.",
            "messages": [{"role": "user", "content": prompt}]
        })
    } else {
        json!({
            "model": model,
            "max_tokens": 512,
            "stream": false,
            "messages": [
                {"role": "system", "content": "You are a concise summarizer. Respond with only the summary text."},
                {"role": "user", "content": prompt}
            ]
        })
    };

    let resp = if is_anthropic {
        state.http.post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send().await.ok()?
    } else {
        state.http.post(&endpoint)
            .bearer_auth(&api_key)
            .json(&request_body)
            .send().await.ok()?
    };

    if !resp.status().is_success() {
        return None;
    }

    let body: Value = resp.json().await.ok()?;
    let text = if is_anthropic {
        body.get("content")
            .and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)?
            .to_string()
    } else {
        body.get("choices")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)?
            .to_string()
    };

    // Update the entity in DB
    let compacted = text.trim().to_string();
    let eid = entity_id;
    let _ = with_db(state, |db| {
        db.execute(
            "UPDATE entities SET summary = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
            params![compacted, eid],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    });

    Some(compacted)
}

/// One-shot, non-streaming LLM call that expands a short feed summary into a fuller
/// markdown overview/article. Grounded in `source_text` (the fetched original page) when
/// available. Returns the article text or an error string.
pub(crate) async fn generate_feed_article_text(
    state: &AppState,
    title: &str,
    summary: &str,
    url: &str,
    source_name: &str,
    source_text: &str,
) -> Result<String, String> {
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state)?;

    // Cap fetched source so we stay within a sane token budget.
    let source_excerpt: String = source_text.chars().take(8000).collect();
    let source_block = if source_excerpt.trim().is_empty() {
        "(The original page could not be fetched — write from the headline and summary, and clearly avoid inventing specific facts, numbers, or quotes.)".to_string()
    } else {
        format!("Original article content (may be truncated):\n{source_excerpt}")
    };

    let prompt = format!(
        "Write a clear, well-structured overview article for this news/feed item.\n\n\
        Headline: {title}\n\
        Source: {source_name}\n\
        URL: {url}\n\
        Existing short summary: {summary}\n\n\
        {source_block}\n\n\
        Guidelines:\n\
        - Write 3-6 short paragraphs (roughly 250-500 words) in plain, engaging prose.\n\
        - Use Markdown. You may add a couple of `##` subheadings or a short bullet list if it genuinely helps.\n\
        - Lead with what happened and why it matters, then add context and key details.\n\
        - Ground everything in the provided content. Do NOT fabricate specifics. If something is unknown, say so or leave it out.\n\
        - Do not restate the headline as a title, and do not add a 'Source:' footer (the UI shows that separately).\n\
        Respond with ONLY the article in Markdown."
    );

    let system = "You are a sharp, concise editorial writer who turns source material into a faithful, readable overview. Never invent facts.";

    let request_body = if is_anthropic {
        json!({
            "model": model,
            "max_tokens": 1600,
            "stream": false,
            "system": system,
            "messages": [{"role": "user", "content": prompt}]
        })
    } else {
        json!({
            "model": model,
            "max_tokens": 1600,
            "stream": false,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt}
            ]
        })
    };

    let resp = if is_anthropic {
        state.http.post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send().await.map_err(|e| e.to_string())?
    } else {
        state.http.post(&endpoint)
            .bearer_auth(&api_key)
            .json(&request_body)
            .send().await.map_err(|e| e.to_string())?
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("LLM request failed ({status}): {body}"));
    }

    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let text = if is_anthropic {
        body.get("content")
            .and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
            .map(|s| s.to_string())
    } else {
        body.get("choices")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .map(|s| s.to_string())
    }
    .ok_or_else(|| "LLM returned no article text".to_string())?;

    let article = text.trim().to_string();
    if article.is_empty() {
        return Err("LLM returned an empty article".to_string());
    }
    Ok(article)
}

/// Backfill embeddings for entities that don't have one yet.
/// Called once on startup to handle entities created before embeddings were added.
pub(crate) async fn backfill_embeddings(state: &AppState) {
    let unembedded: Vec<(i64, String, String)> = with_db(state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT e.id, e.name, e.summary FROM entities e \
                 WHERE e.id NOT IN (SELECT source_id FROM embeddings WHERE source_type = 'entity')"
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(r) = row { out.push(r); }
        }
        Ok(out)
    })
    .unwrap_or_default();

    if unembedded.is_empty() {
        return;
    }

    eprintln!("[Memory] Backfilling embeddings for {} entities", unembedded.len());
    for (id, name, summary) in &unembedded {
        let content = format!("{}: {}", name, summary);
        match embed_text(state, &content).await {
            Ok(emb) => store_embedding(state, "entity", *id, &content, &emb),
            Err(e) => {
                eprintln!("[Memory] Backfill embed failed for '{}': {}", name, e);
                break; // Stop on first error (likely API key issue)
            }
        }
    }
}

/// Build system prompt. If `relevant_entity_ids` is Some, only include those entities.
/// If None, falls back to loading all entities (legacy behavior for agents, etc.).
/// Used by the custom-agent path, which is always user-invoked → interactive = true.
pub(crate) fn build_system_prompt(state: &AppState) -> String {
    build_system_prompt_with_context(state, None, true)
}

pub(crate) fn build_system_prompt_with_context(
    state: &AppState,
    relevant_entity_ids: Option<&[(i64, f32)]>,
    interactive: bool,
) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let home = std::env::var("HOME").unwrap_or_default();
    let workspace = state
        .workspace_root
        .lock()
        .map(|w| w.display().to_string())
        .unwrap_or_default();

    let mut prompt = format!(
        "You are Muninn, a helpful and intelligent AI assistant running locally on the user's Mac. \
Today's date is {today}.\n\n\
## Response Style\n\
Respond naturally and conversationally, like a knowledgeable friend. \
Keep responses brief unless asked for more detail. \
Only use markdown formatting when writing structured content (code, lists, docs). \
For casual conversation, just talk normally.\n\n\
## Capabilities\n\
You have access to the user's local file system and tools. \
Workspace root: {workspace}\n\
Home directory: {home}\n\
Proactively use your tools — don't describe what you *could* do, just do it."
    );

    // Add capability descriptions based on enabled tools
    let enabled = enabled_tool_ids(state).unwrap_or_default();
    let mut caps = Vec::new();
    if enabled.contains("run_command") {
        caps.push("Run shell commands (run_command)");
    }
    if enabled.contains("read_file") {
        caps.push("Read files (read_file)");
    }
    if enabled.contains("write_file") {
        caps.push("Write/create files (write_file)");
    }
    if enabled.contains("list_files") {
        caps.push("List directory contents (list_files)");
    }
    if enabled.contains("search_files") {
        caps.push("Search for text in files (search_files)");
    }
    if enabled.contains("web_search") {
        caps.push("Search the web (web_search)");
    }
    if enabled.contains("web_fetch") {
        caps.push("Fetch web page content (web_fetch)");
    }
    if enabled.contains("generate_image") {
        caps.push("Generate images from text descriptions (generate_image) — images display automatically in the chat after generation, do NOT say you can't show images");
    }
    if enabled.contains("search_places") {
        caps.push("Show a specific named place on the map (search_places) — Use this to locate and display a KNOWN place by name, like 'Arriba's Mexican Grill' or 'LAX airport'. This is a pin-drop tool, not a discovery tool. For discovering new places use search_nearby, for reviews/recommendations use web_search.");
        caps.push("Browse nearby places by CATEGORY (search_nearby) — PREFERRED tool for 'find X near me' or 'what restaurants are around here' queries. Use category IDs like 'restaurant', 'mexican_restaurant', 'coffee', 'gas_station', 'grocery', 'pharmacy', 'hotel', 'bar', 'gym', 'parking', 'fast_food', 'pizza'. Always try search_nearby FIRST for general food/service discovery. Falls back to user's saved location automatically. For reviews, ratings, or 'best of' recommendations, combine with web_search for richer results.");
    }
    if enabled.contains("get_directions") {
        caps.push("Get directions between locations — maps display automatically, ALWAYS use the tool instead of text-only directions. Supports driving, walking, and cycling modes. (get_directions)");
    }
    if enabled.contains("list_inbox") {
        caps.push("Read the user's Apple Mail inbox (list_inbox) — recent messages, optionally unread-only.");
    }
    if enabled.contains("search_mail") {
        caps.push("Search mail by subject or sender (search_mail) — subject+sender only, not body content.");
    }
    if enabled.contains("read_mail") {
        caps.push("Read the full body of a specific message by Message-ID (read_mail).");
    }
    if enabled.contains("write_todos") {
        caps.push("Maintain a plan / checklist for this turn (write_todos) — rewrite the full list each call.");
    }
    if enabled.contains("ask_user") && interactive {
        caps.push("Ask the user a clarifying question and wait for their answer (ask_user).");
    }
    if !caps.is_empty() {
        prompt.push_str("\n\nAvailable tools:\n");
        for cap in &caps {
            prompt.push_str(&format!("- {}\n", cap));
        }
    }

    // Mode-dependent framing. In interactive mode the agent can ask the user
    // questions; in non-interactive (scheduled) mode it cannot and must make
    // judgment calls on its own.
    if !interactive {
        prompt.push_str(
            "\n\n## Autonomous mode\n\
This is a scheduled or background run — there is no user present to answer \
questions. Make your best decision with the context you already have. If the \
request is genuinely ambiguous, note the assumption you made in your output \
rather than stalling.",
        );
    }

    // Email tools need a short section too — explain the workflow and the read-only boundary.
    // Keep in sync with build_project_system_prompt.
    let any_mail_tool = ["list_inbox", "search_mail", "read_mail"]
        .iter()
        .any(|t| enabled.contains(*t));
    if any_mail_tool {
        prompt.push_str(
            "\n\n## Email (Apple Mail)\n\
You can read the user's email through Apple Mail on this Mac. Access is \
READ-ONLY — you cannot send, reply, delete, mark read, or modify anything. \
The tools read Mail's local cache across all configured accounts (iCloud, \
Gmail, work IMAP, etc.).\n\n\
Typical flow: use list_inbox for \"what's new\" questions, search_mail when \
the user names a person or topic, then read_mail on a specific message id \
to quote or summarize content. The `id` returned by list/search is an \
opaque handle — treat it as a black box and pass it verbatim to read_mail. \
Handles are session-local to Mail.app (if Mail restarts, they become stale \
and you need to re-list to get fresh ones; read_mail returns a clear error \
when that happens).\n\n\
Dates are ISO-8601 local time (YYYY-MM-DDTHH:MM:SS). For \"today's mail\" \
or \"emails from last week\", push the date range into the `since` / `until` \
parameters rather than listing everything and filtering mentally — Mail \
handles the filter server-side, which is both faster and lets you see \
matches further back than the recent-N limit. Use today's date from the \
system prompt as your anchor. `until` is exclusive, so for \"today only\" \
set `since` to today and `until` to tomorrow.\n\n\
search_mail matches Subject + From only, not body. If the user asks \
something that needs body content (\"what did Bob say about the budget?\"), \
narrow with search_mail first, then read_mail each candidate.\n\n\
First use of any mail tool will trigger a one-time macOS permission prompt \
(\"Muninn wants to control Mail\"). If it fails with an authorization \
error, tell the user to grant Automation access in System Settings.",
        );
    }

    // Tasks need more context than a single bullet — append a dedicated section
    // whenever any task tool is enabled. Keep this in sync with
    // build_project_system_prompt.
    let any_task_tool = ["create_task", "list_tasks", "update_task", "delete_task"]
        .iter()
        .any(|t| enabled.contains(*t));
    if any_task_tool {
        prompt.push_str(
            "\n\n## Scheduled automations (tasks)\n\
Tasks are scheduled LLM runs — when a task fires, the scheduler starts a fresh \
Muninn session and sends the task's `description` as the user message. You then \
have your full toolset and act autonomously; no human is in the loop.\n\n\
Write `description` as a direct instruction to future-you, imperative voice \
(\"Summarize the top 5 HN posts and save to notes/hn.md\"), not as a memo \
(\"HN summary\"). Be specific enough that future-you can act without clarification.\n\n\
Tasks are automations, not to-dos. Don't create one for \"remind me to call mom\" \
(that's a reminder). Do create one for recurring work the user wants Muninn to \
handle on its own — daily digests, periodic checks, cron-style jobs. Schedule \
strings: 'every 30m', 'daily at 09:00', 'every monday at 10:00', 'hourly'; \
omit schedule for a one-shot delayed run. Call list_tasks before edit/delete \
to get the id.",
        );
    }

    if enabled.contains("write_todos") {
        prompt.push_str(
            "\n\n## Planning (write_todos)\n\
For any user request that will take more than ~3 steps, call `write_todos` \
BEFORE starting work to lay out the plan. Keep the list short (3–8 items), \
each item a single concrete action. Rewrite the full list each time — there \
is no partial-update tool.\n\n\
Status discipline: exactly one item is `in_progress` at any moment (the one \
you're actively working on); others are `pending` or `completed`. Update the \
list after finishing each step so the user can see progress. Don't bother \
with write_todos for trivial 1–2 step requests — it's noise.",
        );
    }

    if enabled.contains("ask_user") && interactive {
        prompt.push_str(
            "\n\n## Asking clarifying questions (ask_user)\n\
Use `ask_user` when you GENUINELY can't proceed without information only the \
user can give — ambiguous references (\"which Alice?\"), destructive action \
confirmations (\"delete all three files?\"), or required inputs the tools \
can't look up.\n\n\
Do NOT use ask_user to:\n\
- Ask permission for every small step (just do reasonable things)\n\
- Get information you could find by calling another tool first (read the \
file, search mail, run `ls` — then ask only if still ambiguous)\n\
- Stall on easy judgment calls — pick something and proceed\n\n\
One question at a time. The call blocks until the user answers, times out \
after 10 minutes, or is cancelled. If `choices` is supplied, the UI shows \
buttons; otherwise it's free-text.",
        );
    }

    // User profile context
    let profile_ctx = with_db(state, |db| {
        db.query_row(
            "SELECT user_name, user_about, profile_text FROM memory_profile WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())
    })
    .ok()
    .flatten();

    if let Some((name, about, text)) = &profile_ctx {
        let has_content = !name.is_empty() || !about.is_empty() || !text.is_empty();
        if has_content {
            prompt.push_str("\n## User Profile");
            if !name.is_empty() {
                prompt.push_str(&format!("\nName: {}", name));
            }
            if !about.is_empty() {
                prompt.push_str(&format!("\nAbout: {}", about));
            }
            if !text.is_empty() {
                prompt.push_str(&format!("\n{}", text));
            }
        }
    }

    // Known entities from memory — filtered by relevance if embeddings available
    let entities_ctx: Vec<(String, String, String, String)> = if let Some(relevant) = relevant_entity_ids {
        // Load only the relevant entities by ID
        let ids: Vec<i64> = relevant.iter().map(|(id, _)| *id).collect();
        with_db(state, |db| {
            let mut out = Vec::new();
            for id in &ids {
                if let Ok(row) = db.query_row(
                    "SELECT kind, name, summary, aliases_json FROM entities WHERE id = ?1",
                    params![id],
                    |row| Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    )),
                ) {
                    out.push(row);
                }
            }
            Ok(out)
        })
        .unwrap_or_default()
    } else {
        // Fallback: load all entities (for agents or when no embeddings exist)
        with_db(state, |db| {
            let mut stmt = db
                .prepare("SELECT kind, name, summary, aliases_json FROM entities")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            for row in rows {
                if let Ok(r) = row {
                    out.push(r);
                }
            }
            Ok(out)
        })
        .unwrap_or_default()
    };

    if !entities_ctx.is_empty() {
        prompt.push_str("\n\n## Known People, Places & Things");
        for (kind, name, summary, aliases_json) in &entities_ctx {
            let aliases: Vec<String> = serde_json::from_str(aliases_json).unwrap_or_default();
            let alias_str = if aliases.is_empty() {
                String::new()
            } else {
                format!(" [aka {}]", aliases.join(", "))
            };
            if summary.is_empty() {
                prompt.push_str(&format!("\n- {} ({}){}", name, kind, alias_str));
            } else {
                prompt.push_str(&format!("\n- {} ({}){}: {}", name, kind, alias_str, summary));
            }
        }
    }

    // Add structured profile fields
    let profile_extra: Option<(String, String, String, String, String, String)> = with_db(state, |db| {
        db.query_row(
            "SELECT identity, personality, goals, preferences, current_state, patterns FROM memory_profile WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .optional()
        .map_err(|e| e.to_string())
    })
    .ok()
    .flatten();

    if let Some((identity, personality, goals, preferences, current_state, patterns)) = &profile_extra {
        let fields = [
            ("Identity", identity), ("Personality", personality),
            ("Goals", goals), ("Preferences", preferences),
            ("Current focus", current_state), ("Patterns", patterns),
        ];
        let has_extra = fields.iter().any(|(_, v)| !v.is_empty());
        if has_extra {
            for (label, value) in &fields {
                if !value.is_empty() {
                    prompt.push_str(&format!("\n{}: {}", label, value));
                }
            }
        }
    }

    // Registered projects summaries
    let projects: Vec<(String, String, String)> = with_db(state, |db| {
        let mut stmt = db
            .prepare("SELECT path, name, agents_md FROM projects ORDER BY last_opened_at DESC LIMIT 20")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(r) = row {
                out.push(r);
            }
        }
        Ok(out)
    })
    .unwrap_or_default();

    if !projects.is_empty() {
        prompt.push_str("\n\n## Registered Projects");
        for (path, name, agents_md) in &projects {
            let display_name = if name.is_empty() { path.as_str() } else { name.as_str() };
            let preview: String = agents_md.lines().take(3).collect::<Vec<_>>().join(" ");
            if preview.is_empty() {
                prompt.push_str(&format!("\n- {} ({})", display_name, path));
            } else {
                prompt.push_str(&format!("\n- {} ({}): {}", display_name, path, preview));
            }
        }
    }

    prompt
}

pub(crate) fn build_project_system_prompt(
    state: &AppState,
    project_path: &str,
    interactive: bool,
) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Read AGENTS.md directly from the workspace directory (fall back to legacy spino.md)
    let agents_md = if !project_path.is_empty() {
        let agents_path = std::path::Path::new(project_path).join("AGENTS.md");
        let legacy_path = std::path::Path::new(project_path).join("spino.md");
        std::fs::read_to_string(&agents_path)
            .or_else(|_| std::fs::read_to_string(&legacy_path))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let mut prompt = format!(
        "You are Muninn, working within the project at {project_path}. \
Today's date is {today}.\n\n\
## Response Style\n\
Respond naturally and conversationally. \
Keep responses brief unless asked for more detail. \
Only use markdown for structured content.\n\n\
## Capabilities\n\
You have access to file tools scoped to: {project_path}\n\
Proactively use your tools — don't describe what you *could* do, just do it."
    );

    let enabled = enabled_tool_ids(state).unwrap_or_default();
    let mut caps = Vec::new();
    if enabled.contains("run_command") { caps.push("Run shell commands (run_command)"); }
    if enabled.contains("read_file") { caps.push("Read files (read_file)"); }
    if enabled.contains("write_file") { caps.push("Write/create files (write_file)"); }
    if enabled.contains("list_files") { caps.push("List directory contents (list_files)"); }
    if enabled.contains("search_files") { caps.push("Search for text in files (search_files)"); }
    if enabled.contains("web_search") { caps.push("Search the web (web_search)"); }
    if enabled.contains("web_fetch") { caps.push("Fetch web page content (web_fetch)"); }
    if enabled.contains("list_inbox") { caps.push("Read the user's Apple Mail inbox (list_inbox)"); }
    if enabled.contains("search_mail") { caps.push("Search mail by subject or sender (search_mail)"); }
    if enabled.contains("read_mail") { caps.push("Read a message by Message-ID (read_mail)"); }
    if enabled.contains("write_todos") { caps.push("Maintain a checklist plan (write_todos)"); }
    if enabled.contains("ask_user") && interactive { caps.push("Ask the user a clarifying question (ask_user)"); }
    if !caps.is_empty() {
        prompt.push_str("\n\nAvailable tools:\n");
        for cap in &caps {
            prompt.push_str(&format!("- {}\n", cap));
        }
    }

    if !interactive {
        prompt.push_str(
            "\n\n## Autonomous mode\nScheduled/background run — no user present. \
Make your best decision with the context you have and note any assumptions \
in your output.",
        );
    }

    // Keep this block in sync with build_system_prompt_with_context.
    let any_mail_tool = ["list_inbox", "search_mail", "read_mail"]
        .iter()
        .any(|t| enabled.contains(*t));
    if any_mail_tool {
        prompt.push_str(
            "\n\n## Email (Apple Mail)\n\
Read-only access to the user's Apple Mail across all configured accounts. \
Cannot send, reply, delete, or modify. Workflow: list_inbox for recent mail, \
search_mail for a person/topic (Subject+From only, not body), read_mail with \
the opaque handle from list/search. Handles are session-local (Mail restart \
invalidates them — re-list to refresh). First call may trigger a macOS \
Automation prompt.",
        );
    }

    // Keep this block in sync with build_system_prompt_with_context.
    let any_task_tool = ["create_task", "list_tasks", "update_task", "delete_task"]
        .iter()
        .any(|t| enabled.contains(*t));
    if any_task_tool {
        prompt.push_str(
            "\n\n## Scheduled automations (tasks)\n\
Tasks are scheduled LLM runs — when a task fires, the scheduler starts a fresh \
Muninn session and sends the task's `description` as the user message. You then \
have your full toolset and act autonomously; no human is in the loop.\n\n\
Write `description` as a direct instruction to future-you, imperative voice \
(\"Summarize the top 5 HN posts and save to notes/hn.md\"), not as a memo \
(\"HN summary\"). Be specific enough that future-you can act without clarification.\n\n\
Tasks are automations, not to-dos. Don't create one for \"remind me to call mom\" \
(that's a reminder). Do create one for recurring work the user wants Muninn to \
handle on its own — daily digests, periodic checks, cron-style jobs. Schedule \
strings: 'every 30m', 'daily at 09:00', 'every monday at 10:00', 'hourly'; \
omit schedule for a one-shot delayed run. Call list_tasks before edit/delete \
to get the id.",
        );
    }

    if enabled.contains("write_todos") {
        prompt.push_str(
            "\n\n## Planning (write_todos)\nFor requests requiring more than ~3 steps, \
call `write_todos` first to lay out the plan. Rewrite the full list each call \
(no partial updates). Exactly one item is `in_progress` at a time. Skip this \
tool for trivial 1–2 step requests.",
        );
    }

    if enabled.contains("ask_user") && interactive {
        prompt.push_str(
            "\n\n## Asking the user (ask_user)\nUse `ask_user` only when you can't \
proceed without user-only info: ambiguous references, destructive-action \
confirmations, required inputs no tool can look up. Don't use it to ask \
permission for every step or to avoid easy judgment calls. One pending \
question per session; call blocks until answered, 10-minute timeout.",
        );
    }

    if !agents_md.is_empty() {
        prompt.push_str(&format!("\n\n## Project Description (AGENTS.md)\n{}", agents_md));
    }

    prompt
}

/// Single source of truth for the agent tool catalog. Each entry is
/// `(name, description, JSON-schema parameters)`. `openai_tools_json` and
/// `anthropic_tools_json` wrap these in their provider-specific envelopes, so a tool
/// defined here is automatically exposed to both providers. (When adding a tool,
/// also add the dispatch arm in `tools::execute_agent_tool`.)
fn tool_defs(enabled: &HashSet<String>, interactive: bool) -> Vec<(&'static str, &'static str, Value)> {
    let mut defs: Vec<(&'static str, &'static str, Value)> = Vec::new();
    if enabled.contains("run_command") {
        defs.push((
            "run_command",
            "Run a shell command in the local workspace and return text output. Commands flagged as potentially dangerous pause and ask the user for approval before running.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string"}
                },
                "required": ["command"]
            }),
        ));
    }
    if enabled.contains("read_file") {
        defs.push((
            "read_file",
            "Read a file within workspace.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        ));
    }
    if enabled.contains("write_file") {
        defs.push((
            "write_file",
            "Write content to a file within workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        ));
    }
    if enabled.contains("list_files") {
        defs.push((
            "list_files",
            "List files in a workspace directory.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        ));
    }
    if enabled.contains("search_files") {
        defs.push((
            "search_files",
            "Search text recursively for a regex pattern in workspace files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "pattern": {"type": "string"}
                },
                "required": ["path", "pattern"]
            }),
        ));
    }
    if enabled.contains("web_search") {
        defs.push((
            "web_search",
            "Search the web using Brave Search API. Returns titles, URLs, and snippets.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"},
                    "count": {"type": "integer", "description": "Number of results (default 5, max 20)"}
                },
                "required": ["query"]
            }),
        ));
    }
    if enabled.contains("web_fetch") {
        defs.push((
            "web_fetch",
            "Fetch the text content of a URL. Returns the page body as plain text.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"}
                },
                "required": ["url"]
            }),
        ));
    }
    if enabled.contains("generate_image") {
        defs.push((
            "generate_image",
            "Generate an image from a text description. Use when the user wants to create, draw, or visualize something.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Detailed description of the image to generate"},
                    "style": {"type": "string", "description": "Art style hint: realistic, artistic, cartoon, abstract"}
                },
                "required": ["prompt"]
            }),
        ));
    }
    if enabled.contains("search_places") {
        defs.push((
            "search_places",
            "Pin-drop tool: locate and display a KNOWN place by name on the map (e.g., 'Arriba's Mexican Grill', 'LAX airport'). NOT for discovering new places — use search_nearby for that, or web_search for reviews.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search for a specific place or business by name. Include city/area for best results (e.g., 'Starbucks in Hemet CA', 'Arriba's Mexican Grill Idyllwild')."},
                    "limit": {"type": "integer", "description": "Max results (default 5, max 10)"}
                },
                "required": ["query"]
            }),
        ));
        defs.push((
            "search_nearby",
            "Browse nearby points of interest by category. Uses the user's saved location. Best for 'find X near me' queries. Returns results sorted by distance.",
            json!({
                "type": "object",
                "properties": {
                    "category": {"type": "string", "description": "Mapbox category ID. Common: restaurant, mexican_restaurant, chinese_restaurant, italian_restaurant, pizza, coffee, cafe, bar, fast_food, gas_station, grocery, supermarket, pharmacy, hospital, hotel, gym, parking, bank, atm, laundry, post_office, library, park, beach, movie_theater, shopping_mall, clothing_store, hardware, auto_repair, car_wash, dentist, veterinarian, hair_salon"},
                    "near": {"type": "string", "description": "Optional location to search near (e.g., 'Hemet, CA'). Uses saved user location if omitted."},
                    "limit": {"type": "integer", "description": "Max results (default 10, max 25)"}
                },
                "required": ["category"]
            }),
        ));
    }
    if enabled.contains("get_directions") {
        defs.push((
            "get_directions",
            "Get directions between two locations. Results display on an interactive map with the route.",
            json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "Starting address or place"},
                    "to": {"type": "string", "description": "Destination address or place"},
                    "mode": {"type": "string", "description": "Transport mode: driving, walking, cycling (default driving)"}
                },
                "required": ["from", "to"]
            }),
        ));
    }
    if enabled.contains("create_task") {
        defs.push((
            "create_task",
            "Schedule a future autonomous Muninn run. When the scheduled time arrives, a fresh session starts with `description` as the user prompt and your full toolset available. Use for recurring automations or one-off delayed actions, not for user reminders.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Short label for the UI / feed (not sent to the LLM at run time)."},
                    "description": {"type": "string", "description": "The prompt sent to future-Muninn when this task fires. Imperative voice, specific enough to act on without clarification — e.g. 'Fetch today's weather for Cupertino and post a one-line summary to the feed', not 'weather check'."},
                    "schedule": {"type": "string", "description": "Recurrence string: 'every 30m', 'daily at 09:00', 'every friday at 17:00', 'hourly'. Omit for a one-shot run."},
                    "next_run": {"type": "string", "description": "ISO timestamp for first run (YYYY-MM-DDTHH:MM:SS). Optional; computed from schedule if omitted."}
                },
                "required": ["name", "description"]
            }),
        ));
    }
    if enabled.contains("list_tasks") {
        defs.push((
            "list_tasks",
            "List tasks with id, name, description, schedule, next_run, status, and last result. Use before updating or deleting so you have the task id.",
            json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "description": "Optional filter: scheduled, running, or completed"}
                }
            }),
        ));
    }
    if enabled.contains("update_task") {
        defs.push((
            "update_task",
            "Edit an existing task. Supply task id and any fields to change. Omitted fields are left as-is.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Task id (from list_tasks)"},
                    "name": {"type": "string"},
                    "description": {"type": "string"},
                    "schedule": {"type": "string"},
                    "next_run": {"type": "string"},
                    "status": {"type": "string", "description": "scheduled, running, or completed"}
                },
                "required": ["id"]
            }),
        ));
    }
    if enabled.contains("delete_task") {
        defs.push((
            "delete_task",
            "Delete a task by id. Call list_tasks first if you don't know the id.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Task id"}
                },
                "required": ["id"]
            }),
        ));
    }
    if enabled.contains("list_inbox") {
        defs.push((
            "list_inbox",
            "Read messages from the user's Apple Mail unified inbox (all configured accounts combined). Returns JSON: {count, messages: [{id, date, from, subject, status}, ...]}, newest first. Dates are ISO-8601 local time (YYYY-MM-DDTHH:MM:SS, no timezone). `id` is an opaque handle — pass it to read_mail for the full body. Use for 'what's in my inbox', 'any new mail', or as a starting point before reading a specific message. Supports optional date-range filtering via `since`/`until`. Reads Apple Mail's local cache; does not hit the server. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "How many messages to return (1–50, default 20)."},
                    "unread_only": {"type": "boolean", "description": "If true, skip messages already marked read. Default false."},
                    "since": {"type": "string", "description": "Optional ISO date/datetime (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS) — only messages received at or after this moment. Today is a good reference: combine with `until` to filter to a single day. Local time."},
                    "until": {"type": "string", "description": "Optional ISO date/datetime — only messages received strictly before this moment (half-open, so `until=2026-04-22` excludes anything on the 22nd)."}
                }
            }),
        ));
    }
    if enabled.contains("search_mail") {
        defs.push((
            "search_mail",
            "Search the user's Apple Mail inbox by subject line OR sender, optionally bounded by date. Returns the same JSON shape as list_inbox. Use when the user names a person, topic, or subject keyword ('did Alice email me?', 'find the AWS receipt from last week'). IMPORTANT: `query` matches Subject and From only, NOT body — if you need body-text search, narrow candidates here then read_mail each. Case-insensitive substring match. Searches the unified inbox (not Sent/Drafts/Trash). At least one of `query`, `since`, `until` must be supplied.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Substring to match against subject or sender. Plain text — no search operators or regex. Optional if you're doing a pure date-range search."},
                    "limit": {"type": "integer", "description": "Max results (1–50, default 10)."},
                    "since": {"type": "string", "description": "Optional ISO date/datetime (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS) — only messages received at or after this moment. Local time."},
                    "until": {"type": "string", "description": "Optional ISO date/datetime — only messages received strictly before this moment (half-open)."}
                }
            }),
        ));
    }
    if enabled.contains("read_mail") {
        defs.push((
            "read_mail",
            "Fetch the full plain-text body and headers (From / To / Subject / Date) of one message. Pass the opaque `id` handle that list_inbox or search_mail returned — it encodes the account+mailbox+local message id needed for a direct O(1) lookup. Handles are session-local to Mail.app: if Mail restarts, re-run list/search to get fresh ones. Body is truncated to ~20k chars if longer.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Opaque handle string as returned by list_inbox or search_mail. Treat as a black box — copy it verbatim. Do NOT construct one yourself or use a raw Message-ID here."}
                },
                "required": ["id"]
            }),
        ));
    }
    if enabled.contains("write_todos") {
        defs.push((
            "write_todos",
            "Record or update your plan for this chat turn as a visible checklist. Rewrite the FULL list each call — there is no partial update. Use for any multi-step task (3+ steps) BEFORE starting work. Exactly one item may be `in_progress` at a time; the rest are `pending` or `completed`. After finishing each step, call write_todos again with that item marked `completed` and the next one `in_progress`. The list renders in the UI so the user sees progress.",
            json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "The full ordered list. Replaces any previous list for this session.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "description": "Short imperative phrase describing the step, e.g. 'Read src/main.rs'."},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                                "activeForm": {"type": "string", "description": "Optional present-participle form shown while the item is the active one, e.g. 'Reading src/main.rs'."}
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["items"]
            }),
        ));
    }
    if interactive && enabled.contains("ask_user") {
        defs.push((
            "ask_user",
            "Pause and ask the user a clarifying question. The call blocks until the user responds (10-minute timeout). Returns the user's answer as a string. Use ONLY when you genuinely cannot proceed: ambiguous references ('which Alice?'), destructive-action confirmations, or required inputs no other tool can supply. Do NOT use to ask permission for every step, or when another tool would give you the answer.",
            json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "The question to show the user. Phrase it conversationally, as if asking a colleague."},
                    "choices": {
                        "type": "array",
                        "description": "Optional — if supplied, the UI renders buttons for each choice. Omit or pass an empty array for free-text input.",
                        "items": {"type": "string"}
                    }
                },
                "required": ["question"]
            }),
        ));
    }
    defs
}

pub(crate) fn openai_tools_json(enabled: &HashSet<String>, interactive: bool) -> Vec<Value> {
    tool_defs(enabled, interactive)
        .into_iter()
        .map(|(name, description, parameters)| {
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters
                }
            })
        })
        .collect()
}

pub(crate) fn anthropic_tools_json(enabled: &HashSet<String>, interactive: bool) -> Vec<Value> {
    tool_defs(enabled, interactive)
        .into_iter()
        .map(|(name, description, parameters)| {
            json!({
                "name": name,
                "description": description,
                "input_schema": parameters
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Provider abstraction + shared chat/agent tool loop
// ---------------------------------------------------------------------------

/// One accumulated tool call from a streamed response.
struct ToolCall {
    id: String,
    name: String,
    /// Raw JSON argument text as accumulated from the stream.
    args: String,
}

/// Everything a single streamed turn produced.
struct StreamOutcome {
    text: String,
    tool_calls: Vec<ToolCall>,
    usage_input: u64,
    usage_output: u64,
}

/// Provider-specific wire format. Everything the OpenAI chat/completions and the
/// Anthropic messages APIs disagree on (auth, request body, SSE framing, tool-round
/// message shapes) lives behind this enum; the shared loop is `run_tool_loop`.
enum Provider {
    OpenAi { endpoint: String, api_key: String },
    Anthropic { endpoint: String, api_key: String },
}

impl Provider {
    fn openai(state: &AppState) -> Result<Self, String> {
        let api_key = if let Some(value) = first_non_empty_setting(state, &["llm/openai_api_key"])? {
            value
        } else {
            std::env::var("OPENAI_API_KEY")
                .map_err(|_| "OpenAI API key missing (set OPENAI_API_KEY or save it in Settings).".to_string())?
        };
        Ok(Self::OpenAi {
            endpoint: "https://api.openai.com/v1/chat/completions".to_string(),
            api_key,
        })
    }

    fn minimax_anthropic(state: &AppState) -> Result<Self, String> {
        let api_key = if let Some(value) =
            first_non_empty_setting(state, &["llm/minimax_api_key", "llm/anthropic_api_key"])?
        {
            value
        } else {
            std::env::var("MINIMAX_API_KEY")
                .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
                .map_err(|_| "MiniMax API key missing (set env var or save it in Settings).".to_string())?
        };
        let base = std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimax.io/anthropic".to_string());
        let endpoint = if base.ends_with("/v1/messages") {
            base
        } else {
            format!("{}/v1/messages", base.trim_end_matches('/'))
        };
        Ok(Self::Anthropic { endpoint, api_key })
    }

    fn label(&self) -> &'static str {
        match self {
            Self::OpenAi { .. } => "OpenAI",
            Self::Anthropic { .. } => "MiniMax Anthropic",
        }
    }

    fn tools_json(&self, enabled: &HashSet<String>, interactive: bool) -> Vec<Value> {
        match self {
            Self::OpenAi { .. } => openai_tools_json(enabled, interactive),
            Self::Anthropic { .. } => anthropic_tools_json(enabled, interactive),
        }
    }

    /// Shape MCP-discovered tools into this provider's tool-definition format.
    fn mcp_tools_json(&self, defs: &[crate::mcp::McpToolDef]) -> Vec<Value> {
        defs.iter()
            .map(|t| match self {
                Self::OpenAi { .. } => json!({
                    "type": "function",
                    "function": {
                        "name": t.full_name,
                        "description": t.description,
                        "parameters": t.schema,
                    }
                }),
                Self::Anthropic { .. } => json!({
                    "name": t.full_name,
                    "description": t.description,
                    "input_schema": t.schema,
                }),
            })
            .collect()
    }

    /// Provider-shaped starting message list. OpenAI carries the system prompt as the
    /// first message; Anthropic passes it as a top-level request field instead.
    fn initial_messages(&self, system_prompt: &str, history: &[HistoryMessage]) -> Vec<Value> {
        match self {
            Self::OpenAi { .. } => {
                let mut messages = vec![json!({"role": "system", "content": system_prompt})];
                messages.extend(history_to_openai(history));
                messages
            }
            Self::Anthropic { .. } => history_to_anthropic(history),
        }
    }

    fn build_request(&self, model: &str, system_prompt: &str, messages: &[Value], tools: &[Value]) -> Value {
        match self {
            Self::OpenAi { .. } => json!({
                "model": model,
                "messages": messages,
                "tools": tools,
                "tool_choice": "auto",
                "stream": true
            }),
            Self::Anthropic { .. } => json!({
                "model": model,
                "max_tokens": 4096,
                "system": system_prompt,
                "messages": messages,
                "tools": tools,
                "stream": true
            }),
        }
    }

    async fn send(&self, http: &reqwest::Client, body: &Value) -> Result<reqwest::Response, String> {
        let request = match self {
            Self::OpenAi { endpoint, api_key } => http.post(endpoint).bearer_auth(api_key),
            Self::Anthropic { endpoint, api_key } => http
                .post(endpoint)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01"),
        };
        let response = request.json(body).send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            return Err(format!("{} API error {}: {}", self.label(), status, body));
        }
        Ok(response)
    }

    /// Drain the SSE stream, invoking `on_token` for each text delta.
    async fn parse_stream(
        &self,
        mut response: reqwest::Response,
        mut on_token: impl FnMut(&str),
    ) -> Result<StreamOutcome, String> {
        let mut text = String::new();
        let mut usage_input: u64 = 0;
        let mut usage_output: u64 = 0;
        let mut line_buf = String::new();

        // OpenAI keys partial tool calls by stream index; Anthropic appends in order.
        let mut openai_calls: HashMap<u64, ToolCall> = HashMap::new();
        let mut anthropic_calls: Vec<ToolCall> = Vec::new();
        let mut current_event_type = String::new();

        while let Some(bytes) = response.chunk().await.map_err(|e| e.to_string())? {
            line_buf.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim().to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if matches!(self, Self::Anthropic { .. }) && line.starts_with("event: ") {
                    current_event_type = line[7..].to_string();
                    continue;
                }
                if line.is_empty() || line == "data: [DONE]" || !line.starts_with("data: ") {
                    continue;
                }
                let data: Value = match serde_json::from_str(&line[6..]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match self {
                    Self::OpenAi { .. } => {
                        if let Some(usage) = data.get("usage") {
                            if let Some(v) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                                usage_input = v;
                            }
                            if let Some(v) = usage.get("completion_tokens").and_then(Value::as_u64) {
                                usage_output = v;
                            }
                        }
                        let delta = match data
                            .get("choices")
                            .and_then(Value::as_array)
                            .and_then(|arr| arr.first())
                            .and_then(|c| c.get("delta"))
                        {
                            Some(d) => d,
                            None => continue,
                        };
                        if let Some(t) = delta.get("content").and_then(Value::as_str) {
                            if !t.is_empty() {
                                text.push_str(t);
                                on_token(t);
                            }
                        }
                        if let Some(tc_arr) = delta.get("tool_calls").and_then(Value::as_array) {
                            for tc in tc_arr {
                                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                                let entry = openai_calls.entry(idx).or_insert_with(|| ToolCall {
                                    id: tc.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                                    name: tc
                                        .get("function")
                                        .and_then(|f| f.get("name"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    args: String::new(),
                                });
                                if let Some(args_chunk) = tc
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(Value::as_str)
                                {
                                    entry.args.push_str(args_chunk);
                                }
                            }
                        }
                    }
                    Self::Anthropic { .. } => match current_event_type.as_str() {
                        "message_start" => {
                            if let Some(usage) = data.get("message").and_then(|m| m.get("usage")) {
                                usage_input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                            }
                        }
                        "message_delta" => {
                            if let Some(usage) = data.get("usage") {
                                usage_output = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                            }
                        }
                        "content_block_start" => {
                            if let Some(block) = data.get("content_block") {
                                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                                    anthropic_calls.push(ToolCall {
                                        id: block.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                                        name: block.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                                        args: String::new(),
                                    });
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = data.get("delta") {
                                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                                    "text_delta" => {
                                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                            if !t.is_empty() {
                                                text.push_str(t);
                                                on_token(t);
                                            }
                                        }
                                    }
                                    "input_json_delta" => {
                                        if let Some(chunk) =
                                            delta.get("partial_json").and_then(Value::as_str)
                                        {
                                            if let Some(last) = anthropic_calls.last_mut() {
                                                last.args.push_str(chunk);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    },
                }
            }
        }

        let tool_calls = match self {
            Self::OpenAi { .. } => {
                let mut sorted: Vec<(u64, ToolCall)> = openai_calls.into_iter().collect();
                sorted.sort_by_key(|(idx, _)| *idx);
                sorted.into_iter().map(|(_, c)| c).collect()
            }
            Self::Anthropic { .. } => anthropic_calls,
        };

        Ok(StreamOutcome {
            text,
            tool_calls,
            usage_input,
            usage_output,
        })
    }

    /// Append the assistant turn carrying tool calls in this provider's message shape.
    fn push_assistant_tool_turn(&self, messages: &mut Vec<Value>, text: &str, calls: &[ToolCall]) {
        match self {
            Self::OpenAi { .. } => {
                let tool_calls_json: Vec<Value> = calls
                    .iter()
                    .map(|c| {
                        json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.args}})
                    })
                    .collect();
                let mut msg = json!({"role": "assistant"});
                msg["content"] = if text.is_empty() { Value::Null } else { json!(text) };
                msg["tool_calls"] = json!(tool_calls_json);
                messages.push(msg);
            }
            Self::Anthropic { .. } => {
                let mut blocks: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for c in calls {
                    let input: Value = serde_json::from_str(&c.args).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({"type": "tool_use", "id": c.id, "name": c.name, "input": input}));
                }
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
        }
    }

    /// Append one tool result in this provider's message shape.
    fn push_tool_result(&self, messages: &mut Vec<Value>, call_id: &str, output: &str) {
        match self {
            Self::OpenAi { .. } => {
                messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
            Self::Anthropic { .. } => {
                messages.push(json!({
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": call_id, "content": output}]
                }));
            }
        }
    }

    /// Append a plain assistant-text + user-text round (used by the fenced-code-block
    /// tool fallback, which has no tool_use ids).
    fn push_text_round(&self, messages: &mut Vec<Value>, assistant_text: &str, user_text: &str) {
        match self {
            Self::OpenAi { .. } => {
                messages.push(json!({"role": "assistant", "content": assistant_text}));
                messages.push(json!({"role": "user", "content": user_text}));
            }
            Self::Anthropic { .. } => {
                messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": assistant_text}]}));
                messages.push(json!({"role": "user", "content": [{"type": "text", "text": user_text}]}));
            }
        }
    }
}

/// System prompt for a regular chat turn: the project prompt when scoped, otherwise
/// the memory-augmented prompt built from the latest user message.
async fn resolve_chat_system_prompt(
    state: &AppState,
    history: &[HistoryMessage],
    scope: Option<&WorkspaceScope>,
    interactive: bool,
) -> String {
    if let Some(s) = scope {
        build_project_system_prompt(state, &s.root.display().to_string(), interactive)
    } else {
        let user_msg = history.last().map(|h| h.plain_text()).unwrap_or_default();
        let relevant = retrieve_relevant_memory(state, &user_msg).await;
        build_system_prompt_with_context(state, relevant.as_deref(), interactive)
    }
}

/// The shared streaming chat/agent loop: request a turn, stream tokens to the UI,
/// execute any tool calls, persist hidden tool-round turns, and repeat until the model
/// produces a plain text reply. Used by both providers, for both regular chat and
/// custom agents (agents pass `agent_name` to drive the agent-iteration UI event).
#[allow(clippy::too_many_arguments)]
/// Emit the lifecycle events for a single tool call, execute it, log the result, and
/// return the string to feed back to the model. On error the returned string is
/// prefixed with `Tool error:`. Shared by the native-tool-call and the
/// fenced-```tool```-code-block paths in `run_tool_loop`, which were previously
/// identical ~30-line blocks.
#[allow(clippy::too_many_arguments)]
async fn execute_and_log_tool_call(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    exec_tools: &HashSet<String>,
    tool_name: &str,
    args_json: &Value,
    args_text: &str,
    scope: Option<&WorkspaceScope>,
    interactive: bool,
) -> String {
    let _ = app.emit("tool-executed", json!({
        "session_id": session_id,
        "tool": tool_name,
        "args": args_text,
        "status": "running"
    }));
    let result = execute_agent_tool(
        app, state, session_id, exec_tools, tool_name, args_json, scope, interactive,
    )
    .await;
    match result {
        Ok(output) => {
            log_tool(state, session_id, tool_name, args_text, &output, "ok");
            let _ = app.emit("tool-executed", json!({
                "session_id": session_id,
                "tool": tool_name,
                "args": args_text,
                "status": "ok"
            }));
            output
        }
        Err(error) => {
            log_tool(state, session_id, tool_name, args_text, &error, "error");
            let _ = app.emit("tool-executed", json!({
                "session_id": session_id,
                "tool": tool_name,
                "args": args_text,
                "status": "error"
            }));
            format!("Tool error: {}", error)
        }
    }
}

/// Periodically nudge the UI with a progress note during long tool-calling runs. The
/// agentic loop is intentionally unbounded, so this is purely informational and offers
/// the `/stop` affordance.
fn emit_tool_round_progress(app: &AppHandle, session_id: &str, tool_round: u32) {
    if tool_round % 5 == 0 {
        let _ = app.emit("chat-token", json!({
            "session_id": session_id,
            "token": format!("\n\n*[Progress: {} tool rounds completed so far. Type /stop to cancel.]*\n\n", tool_round)
        }));
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_tool_loop(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    provider: &Provider,
    model: &str,
    system_prompt: &str,
    history: &[HistoryMessage],
    exec_tools: &HashSet<String>,
    scope: Option<&WorkspaceScope>,
    interactive: bool,
    agent_name: Option<&str>,
) -> Result<(String, Vec<Value>), String> {
    let mut tools = provider.tools_json(exec_tools, interactive);
    // Merge in tools from configured MCP servers (lazy-connects on first turn;
    // resilient — a broken server contributes nothing rather than failing the turn).
    tools.extend(provider.mcp_tools_json(&state.mcp.connected_tools(state).await));
    let mut messages = provider.initial_messages(system_prompt, history);
    let mut tool_round: u32 = 0;
    let mut iteration: i64 = 0;

    loop {
        if check_stop_requested(state, session_id) {
            let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.to_string() });
            let msg = "[Generation stopped by user]".to_string();
            return Ok((msg.clone(), vec![json!({"type": "text", "text": msg})]));
        }

        if let Some(name) = agent_name {
            iteration += 1;
            let _ = app.emit("agent-iteration", AgentIterationEvent {
                session_id: session_id.to_string(),
                agent_name: name.to_string(),
                iteration,
                max_iterations: 0,
            });
        }

        let request_body = provider.build_request(model, system_prompt, &messages, &tools);
        let response = provider.send(&state.http, &request_body).await?;
        let outcome = provider
            .parse_stream(response, |t| {
                let _ = app.emit("chat-token", ChatTokenEvent {
                    session_id: session_id.to_string(),
                    token: t.to_string(),
                });
            })
            .await?;

        if !outcome.tool_calls.is_empty() {
            provider.push_assistant_tool_turn(&mut messages, &outcome.text, &outcome.tool_calls);

            // Persist the assistant tool_use turn as Anthropic-style blocks (hidden=1) so
            // reloads can replay it regardless of which provider produced it.
            let mut assistant_blocks: Vec<Value> = Vec::new();
            if !outcome.text.is_empty() {
                assistant_blocks.push(json!({"type": "text", "text": outcome.text}));
            }
            for call in &outcome.tool_calls {
                let input: Value = serde_json::from_str(&call.args).unwrap_or_else(|_| json!({}));
                assistant_blocks.push(json!({"type": "tool_use", "id": call.id, "name": call.name, "input": input}));
            }
            let _ = insert_message(state, session_id, "assistant", &outcome.text, &assistant_blocks, true);

            let mut tool_result_blocks: Vec<Value> = Vec::new();
            for call in &outcome.tool_calls {
                let args_json: Value = serde_json::from_str(&call.args).unwrap_or_else(|_| json!({}));
                let output = execute_and_log_tool_call(
                    app, state, session_id, exec_tools, &call.name, &args_json, &call.args, scope, interactive,
                )
                .await;
                provider.push_tool_result(&mut messages, &call.id, &output);
                tool_result_blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call.id,
                    "content": output,
                }));
            }
            let _ = insert_message(
                state,
                session_id,
                "user",
                &tool_result_summary(&tool_result_blocks),
                &tool_result_blocks,
                true,
            );

            tool_round += 1;
            emit_tool_round_progress(app, session_id, tool_round);
            continue;
        }

        let text_content = outcome.text;

        // Fallback: some models emit tool calls as fenced ```tool code blocks instead of
        // native tool_use. Execute it and feed the result back as plain text (no ids).
        if let Some((tool_name, args_str)) = parse_tool_code_block(&text_content) {
            let args_json: Value = serde_json::from_str(&args_str).unwrap_or_else(|_| json!({}));
            let args_text = args_str.to_string();

            let output = execute_and_log_tool_call(
                app, state, session_id, exec_tools, &tool_name, &args_json, &args_text, scope, interactive,
            )
            .await;

            provider.push_text_round(&mut messages, &text_content, &format!("Tool result: {}", output));

            let _ = insert_message(
                state,
                session_id,
                "assistant",
                &text_content,
                &[json!({"type": "text", "text": text_content})],
                true,
            );
            let tool_report = format!("Tool result ({}): {}", tool_name, output);
            let _ = insert_message(
                state,
                session_id,
                "user",
                &tool_report,
                &[json!({"type": "text", "text": tool_report})],
                true,
            );

            tool_round += 1;
            emit_tool_round_progress(app, session_id, tool_round);
            continue;
        }

        if outcome.usage_input > 0 || outcome.usage_output > 0 {
            let _ = app.emit("chat-usage", ChatUsageEvent {
                session_id: session_id.to_string(),
                input_tokens: outcome.usage_input,
                output_tokens: outcome.usage_output,
            });
        }
        let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.to_string() });
        let final_blocks = vec![json!({"type": "text", "text": text_content.clone()})];
        return Ok((text_content, final_blocks));
    }
}

pub(crate) async fn chat_with_openai(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    model: &str,
    history: &[HistoryMessage],
    scope: Option<&WorkspaceScope>,
    interactive: bool,
) -> Result<(String, Vec<Value>), String> {
    let provider = Provider::openai(state)?;
    let enabled_tools = enabled_tool_ids(state)?;
    let system_prompt = resolve_chat_system_prompt(state, history, scope, interactive).await;
    run_tool_loop(
        app, state, session_id, &provider, model, &system_prompt,
        history, &enabled_tools, scope, interactive, None,
    )
    .await
}

pub(crate) async fn chat_with_minimax_anthropic(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    model: &str,
    history: &[HistoryMessage],
    scope: Option<&WorkspaceScope>,
    interactive: bool,
) -> Result<(String, Vec<Value>), String> {
    let provider = Provider::minimax_anthropic(state)?;
    let enabled_tools = enabled_tool_ids(state)?;
    let system_prompt = resolve_chat_system_prompt(state, history, scope, interactive).await;
    run_tool_loop(
        app, state, session_id, &provider, model, &system_prompt,
        history, &enabled_tools, scope, interactive, None,
    )
    .await
}

pub(crate) fn load_agent_by_id(state: &AppState, agent_id: &str) -> Result<crate::models::AgentRecord, String> {
    use rusqlite::params;
    with_db(state, |db| {
        db.query_row(
            "SELECT agent_id, name, description, slash_command, system_prompt, max_iterations, enabled, allowed_tools_json, created_at, updated_at FROM agents WHERE agent_id = ?1",
            params![agent_id],
            |row| {
                Ok(crate::models::AgentRecord {
                    agent_id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    slash_command: row.get(3)?,
                    system_prompt: row.get(4)?,
                    max_iterations: row.get(5)?,
                    enabled: row.get::<_, i64>(6)? == 1,
                    allowed_tools_json: row.get(7)?,
                    created_at: row.get(8)?,
                    updated_at: row.get(9)?,
                })
            },
        )
        .map_err(|e| format!("agent not found: {}", e))
    })
}

pub(crate) async fn run_agent(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    agent_id: &str,
    history: &[HistoryMessage],
) -> Result<(String, Vec<Value>), String> {
    let agent = load_agent_by_id(state, agent_id)?;
    if !agent.enabled {
        return Err(format!("Agent '{}' is disabled", agent.name));
    }

    // Build agent-specific system prompt: agent's own prompt + memory context
    let memory_ctx = build_system_prompt(state);
    let agent_system = if agent.system_prompt.is_empty() {
        memory_ctx
    } else {
        format!("{}\n\n{}", agent.system_prompt, memory_ctx)
    };

    // Parse allowed tools from agent config
    let allowed_tools: HashSet<String> = {
        let parsed: Vec<String> = serde_json::from_str(&agent.allowed_tools_json).unwrap_or_default();
        if parsed.is_empty() {
            // If no tools specified, use all enabled tools
            enabled_tool_ids(state)?
        } else {
            let enabled = enabled_tool_ids(state)?;
            parsed.into_iter().filter(|t| enabled.contains(t)).collect()
        }
    };

    // Determine provider/model
    let provider_name =
        get_app_setting_value(state, "llm/provider")?.unwrap_or_else(|| "minimax".to_string());
    let model = get_app_setting_value(state, "llm/model")?
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| {
            if provider_name == "minimax" {
                "MiniMax-M2.7".to_string()
            } else {
                "gpt-4.1-mini".to_string()
            }
        });

    let is_anthropic =
        provider_name == "minimax" || provider_name == "anthropic" || model.starts_with("MiniMax-");
    let provider = if is_anthropic {
        Provider::minimax_anthropic(state)?
    } else {
        Provider::openai(state)?
    };

    // Custom agents are user-invoked via slash commands — always interactive, and not
    // project-scoped (scope: None matches the previous per-provider agent runners).
    run_tool_loop(
        app, state, session_id, &provider, &model, &agent_system,
        history, &allowed_tools, None, true, Some(&agent.name),
    )
    .await
}

pub(crate) async fn run_extraction(app: &AppHandle, state: &AppState, user_msg: &str, assistant_msg: &str) {
    if user_msg.len() + assistant_msg.len() < 50 {
        return;
    }

    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = match resolve_llm_config(state) {
        Ok(cfg) => cfg,
        Err(_) => return,
    };

    let entities: Vec<(i64, String, String, String, String)> = with_db(state, |db| {
        let mut stmt = db
            .prepare("SELECT id, kind, name, summary, aliases_json FROM entities")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(r) = row {
                out.push(r);
            }
        }
        Ok(out)
    })
    .unwrap_or_default();

    let profile: Option<(String, String, String, String, String, String, String, String, String)> = with_db(state, |db| {
        db.query_row(
            "SELECT user_name, user_about, profile_text, identity, personality, goals, preferences, current_state, patterns FROM memory_profile WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())
    })
    .ok()
    .flatten();

    let mut existing_context = String::new();
    if !entities.is_empty() {
        existing_context.push_str("Known entities:\n");
        for (_id, kind, name, summary, aliases_json) in &entities {
            let aliases: Vec<String> = serde_json::from_str(aliases_json).unwrap_or_default();
            if aliases.is_empty() {
                existing_context.push_str(&format!("- {} ({}): {}\n", name, kind, summary));
            } else {
                existing_context.push_str(&format!("- {} [aka {}] ({}): {}\n", name, aliases.join(", "), kind, summary));
            }
        }
        existing_context.push('\n');
    }

    if let Some((name, about, text, identity, personality, goals, preferences, current_state, patterns)) = &profile {
        let fields = [
            ("Name", name), ("About", about), ("Identity", identity),
            ("Personality", personality), ("Goals", goals), ("Preferences", preferences),
            ("Current state", current_state), ("Patterns", patterns), ("Notes", text),
        ];
        let has_content = fields.iter().any(|(_, v)| !v.is_empty());
        if has_content {
            existing_context.push_str("Current user profile:\n");
            for (label, value) in &fields {
                if !value.is_empty() {
                    existing_context.push_str(&format!("{}: {}\n", label, value));
                }
            }
            existing_context.push('\n');
        }
    }

    let user_trunc = truncate_chars(user_msg, 2000);
    let assistant_trunc = truncate_chars(assistant_msg, 2000);

    let today = chrono::Local::now().format("%Y-%m").to_string();

    let extraction_prompt = format!(
        "Analyze this conversation exchange and extract structured information.\n\n\
{existing_context}--- EXCHANGE ---\n\
User: {user_trunc}\n\n\
Assistant: {assistant_trunc}\n\
--- END EXCHANGE ---\n\n\
Return a JSON object with these optional fields (omit any that have no updates):\n\
{{\n\
  \"profile_updates\": {{\n\
    \"identity\": {{ \"value\": \"who the user is\", \"mode\": \"append|replace\" }},\n\
    \"personality\": {{ \"value\": \"...\", \"mode\": \"append|replace\" }},\n\
    \"goals\": {{ \"value\": \"...\", \"mode\": \"append|replace\" }},\n\
    \"preferences\": {{ \"value\": \"...\", \"mode\": \"append|replace\" }},\n\
    \"current_state\": {{ \"value\": \"...\", \"mode\": \"append|replace\" }},\n\
    \"patterns\": {{ \"value\": \"...\", \"mode\": \"append|replace\" }}\n\
  }},\n\
  \"entities\": [\n\
    {{\n\
      \"name\": \"entity name\",\n\
      \"type\": \"person|place|project|organization|concept|thing\",\n\
      \"summary\": \"what changed or new info (will be appended to history)\",\n\
      \"aliases\": [\"nickname\", \"abbreviation\"]\n\
    }}\n\
  ],\n\
  \"merge_entities\": [\n\
    {{ \"old_name\": \"duplicate entity name\", \"into\": \"canonical entity name\" }}\n\
  ],\n\
  \"delete_entities\": [\"entity name to remove if no longer relevant\"],\n\
  \"relationships\": [\n\
    {{\n\
      \"from\": \"entity name\",\n\
      \"to\": \"entity name\",\n\
      \"label\": \"relationship type (e.g. works_at, uses, part_of)\"\n\
    }}\n\
  ],\n\
  \"episode\": {{\n\
    \"description\": \"what was discussed\",\n\
    \"entity_names\": [\"entities involved\"]\n\
  }}\n\
}}\n\n\
Rules:\n\
- Only extract genuinely meaningful information, not small talk\n\
- For existing entities, use their exact name from the known entities list\n\
- Entity summaries are APPENDED as dated history — write what's NEW or CHANGED, not the full summary\n\
- Use merge_entities when you notice two entities that refer to the same thing (e.g. \"JS\" and \"JavaScript\")\n\
- Use delete_entities only for entities that are clearly wrong, trivial, or no longer relevant\n\
- Profile update mode: use \"replace\" when the new value supersedes the old (e.g. new job, new focus), use \"append\" when adding additional info\n\
- Omit profile fields that haven't changed\n\
- If nothing meaningful to extract, return {{}}\n\
- Respond with ONLY valid JSON, no markdown fences, no explanation"
    );

    let request_body = if is_anthropic {
        json!({
            "model": model,
            "max_tokens": 1024,
            "stream": false,
            "system": "You are an entity extraction system. Always respond with valid JSON only. No markdown.",
            "messages": [{"role": "user", "content": extraction_prompt}]
        })
    } else {
        json!({
            "model": model,
            "max_tokens": 1024,
            "temperature": 0.1,
            "stream": false,
            "messages": [
                {"role": "system", "content": "You are an entity extraction system. Always respond with valid JSON only. No markdown."},
                {"role": "user", "content": extraction_prompt}
            ]
        })
    };

    let response = if is_anthropic {
        state
            .http
            .post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send()
            .await
    } else {
        state
            .http
            .post(&endpoint)
            .bearer_auth(&api_key)
            .json(&request_body)
            .send()
            .await
    };

    let response = match response {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[Memory] Extraction request failed: {}", e);
            return;
        }
    };

    if !response.status().is_success() {
        eprintln!("[Memory] Extraction API error: {}", response.status());
        return;
    }

    let body: Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[Memory] Failed to parse extraction response: {}", e);
            return;
        }
    };

    let content = if is_anthropic {
        body.get("content")
            .and_then(Value::as_array)
            .and_then(|arr| {
                arr.iter()
                    .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("")
            .to_string()
    } else {
        body.get("choices")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    if content.is_empty() {
        return;
    }

    let mut cleaned = content.trim().to_string();
    if let Some(pos) = cleaned.find("</think>") {
        cleaned = cleaned[pos + 8..].trim().to_string();
    } else if cleaned.starts_with("<think>") {
        if let Some(pos) = cleaned.find('{') {
            cleaned = cleaned[pos..].to_string();
        }
    }
    if cleaned.starts_with("```") {
        if let Some(nl) = cleaned.find('\n') {
            if let Some(last_fence) = cleaned.rfind("```") {
                if last_fence > nl {
                    cleaned = cleaned[nl + 1..last_fence].trim().to_string();
                }
            }
        }
    }

    let parsed: Value = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(_) => return,
    };

    if !parsed.is_object() || parsed.as_object().map_or(true, |o| o.is_empty()) {
        return;
    }

    // Apply profile updates (structured fields with append/replace mode)
    if let Some(profile_obj) = parsed.get("profile_updates") {
        // Support legacy string format
        if let Some(profile_str) = profile_obj.as_str() {
            if !profile_str.trim().is_empty() {
                let _ = with_db(state, |db| {
                    let existing: String = db
                        .query_row("SELECT profile_text FROM memory_profile WHERE id = 1", [], |row| row.get(0))
                        .unwrap_or_default();
                    let new_text = if existing.is_empty() { profile_str.to_string() } else { format!("{}\n{}", existing, profile_str) };
                    db.execute("UPDATE memory_profile SET profile_text = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = 1", params![new_text])
                        .map_err(|e| e.to_string())?;
                    Ok(())
                });
            }
        } else if let Some(obj) = profile_obj.as_object() {
            let fields = ["identity", "personality", "goals", "preferences", "current_state", "patterns"];
            for field_name in &fields {
                if let Some(field_val) = obj.get(*field_name) {
                    // Support both plain string (legacy) and {value, mode} object
                    let (val, mode) = if let Some(s) = field_val.as_str() {
                        (s.to_string(), "append".to_string())
                    } else if let Some(o) = field_val.as_object() {
                        let v = o.get("value").and_then(Value::as_str).unwrap_or("").to_string();
                        let m = o.get("mode").and_then(Value::as_str).unwrap_or("append").to_string();
                        (v, m)
                    } else {
                        continue;
                    };

                    if val.trim().is_empty() {
                        continue;
                    }

                    let _ = with_db(state, |db| {
                        let new_val = if mode == "replace" {
                            val.clone()
                        } else {
                            let existing: String = db
                                .query_row(
                                    &format!("SELECT {} FROM memory_profile WHERE id = 1", field_name),
                                    [],
                                    |row| row.get(0),
                                )
                                .unwrap_or_default();
                            if existing.is_empty() {
                                val.clone()
                            } else {
                                format!("{}\n{}", existing, val)
                            }
                        };
                        db.execute(
                            &format!("UPDATE memory_profile SET {} = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = 1", field_name),
                            params![new_val],
                        )
                        .map_err(|e| e.to_string())?;
                        Ok(())
                    });
                }
            }
        }
    }

    // Apply entities with aliases
    if let Some(ents) = parsed.get("entities").and_then(Value::as_array) {
        for ent in ents {
            let name = ent.get("name").and_then(Value::as_str).unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            let kind = ent.get("type").and_then(Value::as_str).unwrap_or("concept");
            let summary = ent.get("summary").and_then(Value::as_str).unwrap_or("");
            let new_aliases: Vec<String> = ent
                .get("aliases")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();

            let _ = with_db(state, |db| {
                // Try to find by name or alias
                let existing: Option<(i64, String)> = db
                    .query_row(
                        "SELECT id, aliases_json FROM entities WHERE name = ?1 COLLATE NOCASE",
                        params![name],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;

                // If not found by name, search aliases
                let existing = if existing.is_some() {
                    existing
                } else {
                    let mut stmt = db.prepare("SELECT id, aliases_json FROM entities").map_err(|e| e.to_string())?;
                    let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
                    let mut found = None;
                    let name_lower = name.to_lowercase();
                    for row in rows {
                        if let Ok((id, aj)) = row {
                            let aliases: Vec<String> = serde_json::from_str(&aj).unwrap_or_default();
                            if aliases.iter().any(|a| a.to_lowercase() == name_lower) {
                                found = Some((id, aj));
                                break;
                            }
                        }
                    }
                    found
                };

                if let Some((id, existing_aliases_json)) = existing {
                    // Merge aliases
                    let mut existing_aliases: Vec<String> = serde_json::from_str(&existing_aliases_json).unwrap_or_default();
                    for alias in &new_aliases {
                        let lower = alias.to_lowercase();
                        if !existing_aliases.iter().any(|a| a.to_lowercase() == lower) {
                            existing_aliases.push(alias.clone());
                        }
                    }
                    let merged_json = serde_json::to_string(&existing_aliases).unwrap_or_else(|_| "[]".to_string());
                    if !summary.is_empty() {
                        // Append summary with date prefix to build entity history
                        let existing_summary: String = db
                            .query_row("SELECT summary FROM entities WHERE id = ?1", params![id], |row| row.get(0))
                            .unwrap_or_default();
                        let dated_entry = format!("{}: {}", today, summary);
                        let new_summary = if existing_summary.is_empty() {
                            dated_entry
                        } else {
                            format!("{}\n{}", existing_summary, dated_entry)
                        };
                        db.execute(
                            "UPDATE entities SET summary = ?1, aliases_json = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?3",
                            params![new_summary, merged_json, id],
                        ).map_err(|e| e.to_string())?;
                    } else {
                        db.execute(
                            "UPDATE entities SET aliases_json = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
                            params![merged_json, id],
                        ).map_err(|e| e.to_string())?;
                    }
                } else {
                    let aliases_json = serde_json::to_string(&new_aliases).unwrap_or_else(|_| "[]".to_string());
                    db.execute(
                        "INSERT INTO entities (kind, name, summary, aliases_json) VALUES (?1, ?2, ?3, ?4)",
                        params![kind, name, summary, aliases_json],
                    ).map_err(|e| e.to_string())?;
                }
                Ok(())
            });
        }
    }

    // Apply relationships
    if let Some(rels) = parsed.get("relationships").and_then(Value::as_array) {
        for rel in rels {
            let from_name = rel.get("from").and_then(Value::as_str).unwrap_or("").trim();
            let to_name = rel.get("to").and_then(Value::as_str).unwrap_or("").trim();
            let label = rel.get("label").and_then(Value::as_str).unwrap_or("").trim();
            if from_name.is_empty() || to_name.is_empty() || label.is_empty() {
                continue;
            }
            let _ = with_db(state, |db| {
                let from_id: Option<i64> = db
                    .query_row("SELECT id FROM entities WHERE name = ?1 COLLATE NOCASE", params![from_name], |row| row.get(0))
                    .optional().map_err(|e| e.to_string())?;
                let to_id: Option<i64> = db
                    .query_row("SELECT id FROM entities WHERE name = ?1 COLLATE NOCASE", params![to_name], |row| row.get(0))
                    .optional().map_err(|e| e.to_string())?;
                if let (Some(fid), Some(tid)) = (from_id, to_id) {
                    let _ = db.execute(
                        "INSERT OR IGNORE INTO relationships (from_entity_id, to_entity_id, label) VALUES (?1, ?2, ?3)",
                        params![fid, tid, label],
                    );
                }
                Ok(())
            });
        }
    }

    // Apply merge_entities: merge duplicates into canonical entity
    if let Some(merges) = parsed.get("merge_entities").and_then(Value::as_array) {
        for merge in merges {
            let old_name = merge.get("old_name").and_then(Value::as_str).unwrap_or("").trim();
            let into_name = merge.get("into").and_then(Value::as_str).unwrap_or("").trim();
            if old_name.is_empty() || into_name.is_empty() || old_name.eq_ignore_ascii_case(into_name) {
                continue;
            }
            let _ = with_db(state, |db| {
                let old: Option<(i64, String, String)> = db
                    .query_row(
                        "SELECT id, summary, aliases_json FROM entities WHERE name = ?1 COLLATE NOCASE",
                        params![old_name],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional().map_err(|e| e.to_string())?;
                let target: Option<(i64, String, String)> = db
                    .query_row(
                        "SELECT id, summary, aliases_json FROM entities WHERE name = ?1 COLLATE NOCASE",
                        params![into_name],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional().map_err(|e| e.to_string())?;

                if let (Some((old_id, old_summary, old_aliases_json)), Some((target_id, target_summary, target_aliases_json))) = (old, target) {
                    // Merge summaries
                    let merged_summary = if old_summary.is_empty() {
                        target_summary
                    } else if target_summary.is_empty() {
                        old_summary
                    } else {
                        format!("{}\n{}", target_summary, old_summary)
                    };

                    // Merge aliases (add old name as alias)
                    let mut target_aliases: Vec<String> = serde_json::from_str(&target_aliases_json).unwrap_or_default();
                    let old_aliases: Vec<String> = serde_json::from_str(&old_aliases_json).unwrap_or_default();
                    // Add old entity name as alias
                    let old_lower = old_name.to_lowercase();
                    if !target_aliases.iter().any(|a| a.to_lowercase() == old_lower) {
                        target_aliases.push(old_name.to_string());
                    }
                    for alias in old_aliases {
                        let lower = alias.to_lowercase();
                        if !target_aliases.iter().any(|a| a.to_lowercase() == lower) {
                            target_aliases.push(alias);
                        }
                    }
                    let merged_aliases_json = serde_json::to_string(&target_aliases).unwrap_or_else(|_| "[]".to_string());

                    // Update target entity
                    let _ = db.execute(
                        "UPDATE entities SET summary = ?1, aliases_json = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?3",
                        params![merged_summary, merged_aliases_json, target_id],
                    );

                    // Reassign relationships from old to target
                    let _ = db.execute("UPDATE OR IGNORE relationships SET from_entity_id = ?1 WHERE from_entity_id = ?2", params![target_id, old_id]);
                    let _ = db.execute("UPDATE OR IGNORE relationships SET to_entity_id = ?1 WHERE to_entity_id = ?2", params![target_id, old_id]);
                    let _ = db.execute("UPDATE OR IGNORE episode_entities SET entity_id = ?1 WHERE entity_id = ?2", params![target_id, old_id]);

                    // Delete old entity and its embedding
                    let _ = db.execute("DELETE FROM entities WHERE id = ?1", params![old_id]);
                    let _ = db.execute("DELETE FROM embeddings WHERE source_type = 'entity' AND source_id = ?1", params![old_id]);
                }
                Ok(())
            });
        }
    }

    // Apply delete_entities: remove stale/wrong entities
    if let Some(deletes) = parsed.get("delete_entities").and_then(Value::as_array) {
        for del in deletes {
            if let Some(name) = del.as_str() {
                let name = name.trim();
                if name.is_empty() { continue; }
                let _ = with_db(state, |db| {
                    if let Some(id) = db
                        .query_row("SELECT id FROM entities WHERE name = ?1 COLLATE NOCASE", params![name], |row| row.get::<_, i64>(0))
                        .optional().map_err(|e| e.to_string())?
                    {
                        let _ = db.execute("DELETE FROM entities WHERE id = ?1", params![id]);
                        let _ = db.execute("DELETE FROM embeddings WHERE source_type = 'entity' AND source_id = ?1", params![id]);
                        let _ = db.execute("DELETE FROM relationships WHERE from_entity_id = ?1 OR to_entity_id = ?1", params![id]);
                        let _ = db.execute("DELETE FROM episode_entities WHERE entity_id = ?1", params![id]);
                    }
                    Ok(())
                });
            }
        }
    }

    // Apply episode with entity linking
    if let Some(ep) = parsed.get("episode").and_then(Value::as_object) {
        if let Some(desc) = ep.get("description").and_then(Value::as_str) {
            if !desc.trim().is_empty() {
                let _ = with_db(state, |db| {
                    db.execute(
                        "INSERT INTO episodes (description, timestamp) VALUES (?1, datetime('now'))",
                        params![desc],
                    )
                    .map_err(|e| e.to_string())?;
                    let episode_id = db.last_insert_rowid();

                    // Link entities by name
                    if let Some(names) = ep.get("entity_names").and_then(Value::as_array) {
                        for name_val in names {
                            if let Some(name) = name_val.as_str() {
                                let entity_id: Option<i64> = db
                                    .query_row("SELECT id FROM entities WHERE name = ?1 COLLATE NOCASE", params![name.trim()], |row| row.get(0))
                                    .optional().map_err(|e| e.to_string())?;
                                if let Some(eid) = entity_id {
                                    let _ = db.execute("INSERT OR IGNORE INTO episode_entities (episode_id, entity_id) VALUES (?1, ?2)", params![episode_id, eid]);
                                }
                            }
                        }
                    }
                    Ok(())
                });

                // Embed the episode
                let episode_id: Option<i64> = with_db(state, |db| {
                    db.query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
                        .map_err(|e| e.to_string())
                }).ok();
                if let Some(eid) = episode_id {
                    if let Ok(emb) = embed_text(state, desc).await {
                        store_embedding(state, "episode", eid, desc, &emb);
                    }
                }
            }
        }
    }

    // Embed updated/new entities and run compaction if needed
    let entities_to_embed: Vec<(i64, String, String)> = with_db(state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT e.id, e.name, e.summary FROM entities e \
                 WHERE e.updated_at >= datetime('now', '-10 seconds')"
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(r) = row { out.push(r); }
        }
        Ok(out)
    })
    .unwrap_or_default();

    for (entity_id, entity_name, summary) in &entities_to_embed {
        // Embed the entity
        let embed_text_content = format!("{}: {}", entity_name, summary);
        if let Ok(emb) = embed_text(state, &embed_text_content).await {
            store_embedding(state, "entity", *entity_id, &embed_text_content, &emb);
        }

        // Compact if summary is too long
        if summary.len() > 500 {
            compact_entity_summary(state, *entity_id, entity_name, summary).await;
            // Re-embed after compaction
            let compacted: Option<String> = with_db(state, |db| {
                db.query_row("SELECT summary FROM entities WHERE id = ?1", params![entity_id], |row| row.get(0))
                    .optional()
                    .map_err(|e| e.to_string())
            }).ok().flatten();
            if let Some(cs) = compacted {
                let embed_text_content = format!("{}: {}", entity_name, cs);
                if let Ok(emb) = embed_text(state, &embed_text_content).await {
                    store_embedding(state, "entity", *entity_id, &embed_text_content, &emb);
                }
            }
        }
    }

    let _ = app.emit("memory-updated", json!({}));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mag(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[test]
    fn cosine_identical_vectors_is_one() {
        let q = [3.0f32, 4.0];
        let s = cosine_with_query_mag(&q, mag(&q), &[3.0, 4.0]);
        assert!((s - 1.0).abs() < 1e-6, "got {}", s);
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let q = [1.0f32, 0.0];
        let s = cosine_with_query_mag(&q, mag(&q), &[0.0, 1.0]);
        assert!(s.abs() < 1e-6, "got {}", s);
    }

    #[test]
    fn cosine_opposite_vectors_is_negative_one() {
        let q = [1.0f32, 0.0];
        let s = cosine_with_query_mag(&q, mag(&q), &[-1.0, 0.0]);
        assert!((s + 1.0).abs() < 1e-6, "got {}", s);
    }

    #[test]
    fn cosine_handles_degenerate_inputs() {
        let q = [1.0f32, 2.0];
        // Dimension mismatch → 0.
        assert_eq!(cosine_with_query_mag(&q, mag(&q), &[1.0, 2.0, 3.0]), 0.0);
        // Empty candidate → 0.
        assert_eq!(cosine_with_query_mag(&q, mag(&q), &[]), 0.0);
        // Zero query magnitude → 0.
        assert_eq!(cosine_with_query_mag(&[0.0, 0.0], 0.0, &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn blob_roundtrip_preserves_vector() {
        let v = vec![0.5f32, -1.25, 3.0, 0.0];
        let restored = blob_to_f32_vec(&f32_vec_to_blob(&v));
        assert_eq!(v, restored);
    }
}
