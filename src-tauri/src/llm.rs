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

/// Reply text `run_tool_loop` returns when the user stops generation mid-turn. The goal
/// auto-continue loop compares against this so a Stop click also halts the loop instead of
/// being evaluated as an ordinary (unmet) reply and overridden by a re-prompt.
pub(crate) const STOPPED_BY_USER_SENTINEL: &str = "[Generation stopped by user]";

/// Check if a stop has been requested for this session (and clear the flag).
/// Recovers from a poisoned lock so a panic elsewhere can't wedge the stop signal.
pub(crate) fn check_stop_requested(state: &AppState, session_id: &str) -> bool {
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
                "MiniMax-M3".to_string()
            } else {
                "gpt-4.1-mini".to_string()
            }
        });

    let (is_anthropic, endpoint, api_key) = resolve_provider_wire(state, &provider)?;

    Ok(LlmCallConfig {
        provider,
        model,
        is_anthropic,
        api_key,
        endpoint,
    })
}

/// The single source of truth mapping a provider name → (is_anthropic wire format,
/// endpoint URL, API key). Shared by the one-shot helper path (`resolve_llm_config`) and
/// the streaming chat/agent path (`Provider::from_provider_name`), so adding a provider in
/// one place covers both. `minimax`/`anthropic` use the Anthropic messages wire format;
/// every other provider uses the OpenAI chat-completions format. Returns `Err` if the
/// matching API key is not configured.
pub(crate) fn resolve_provider_wire(state: &AppState, provider: &str) -> Result<(bool, String, String), String> {
    let api_key = resolve_provider_key(state, provider)?;

    let (endpoint, is_anthropic) = if provider == "minimax" {
        let base = std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimax.io/anthropic".to_string());
        let ep = if base.ends_with("/v1/messages") {
            base
        } else {
            format!("{}/v1/messages", base.trim_end_matches('/'))
        };
        (ep, true)
    } else if provider == "anthropic" {
        ("https://api.anthropic.com/v1/messages".to_string(), true)
    } else if provider == "custom" {
        // Local/self-hosted OpenAI-compatible server. The base URL is required; the
        // key was already resolved (falls back to the "local" dummy when unset).
        let base = get_app_setting_value(state, "llm/custom_base_url")?
            .filter(|b| !b.trim().is_empty())
            .ok_or_else(|| "No custom base URL configured".to_string())?;
        (format!("{}/chat/completions", base.trim_end_matches('/')), false)
    } else {
        let ep = match provider {
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
            // Meta Model API (OpenAI-compatible). Key comes from llm/meta_api_key via the
            // generic branch in resolve_provider_key.
            "meta" => "https://api.meta.ai/v1/chat/completions".to_string(),
            _ => "https://api.openai.com/v1/chat/completions".to_string(),
        };
        (ep, false)
    };

    Ok((is_anthropic, endpoint, api_key))
}

/// Resolve just the API key for a provider, using the same setting-key precedence and
/// env fallbacks as `resolve_provider_wire`. Used by the models-listing command (which
/// hits a different endpoint than chat) and shared by `resolve_provider_wire`. The
/// `custom` provider falls back to the literal dummy key "local" so local servers
/// (Ollama/LM Studio) that need no auth still work; every other provider errors when no
/// key is configured. Never returns the key to the webview — callers use it only to sign
/// outbound requests.
pub(crate) fn resolve_provider_key(state: &AppState, provider: &str) -> Result<String, String> {
    let key_opt = if provider == "minimax" {
        first_non_empty_setting(state, &["llm/minimax_api_key", "llm/anthropic_api_key"])?
            .or_else(|| std::env::var("MINIMAX_API_KEY").ok())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
    } else if provider == "anthropic" {
        first_non_empty_setting(state, &["llm/anthropic_api_key"])?
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
    } else if provider == "custom" {
        let key = first_non_empty_setting(state, &["llm/custom_api_key"])?
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| "local".to_string());
        return Ok(key);
    } else {
        let key_setting = format!("llm/{}_api_key", provider);
        // llm/key_fallback (default on) controls whether a generic provider without its
        // own key may fall back to the OpenAI key. That fallback is usually wrong (the
        // request just 401s with a confusing error), so users can disable it to get an
        // honest "no key configured" instead.
        let allow_openai_fallback =
            get_app_setting_value(state, "llm/key_fallback")?.as_deref() != Some("0");
        if allow_openai_fallback {
            first_non_empty_setting(state, &[&key_setting, "llm/openai_api_key"])?
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        } else {
            first_non_empty_setting(state, &[&key_setting])?
        }
    };

    key_opt
        .filter(|k| !k.is_empty())
        .ok_or_else(|| format!("No API key configured for provider '{}'", provider))
}

/// Resolve which cloud speech-to-text provider to use, honoring the `stt/provider`
/// setting: "auto" (default) prefers Groq's whisper-large-v3-turbo when a Groq key is
/// configured and otherwise falls back to OpenAI whisper-1; "groq"/"openai" force the
/// choice (erroring if that provider's key is missing). Returns `(api_key, model,
/// transcriptions_endpoint)`. Shared by `transcribe_wav` and the video cloud fallback so
/// both paths agree.
pub(crate) fn resolve_stt(
    state: &AppState,
) -> Result<(String, &'static str, &'static str), String> {
    const GROQ: (&str, &str) = (
        "whisper-large-v3-turbo",
        "https://api.groq.com/openai/v1/audio/transcriptions",
    );
    const OPENAI: (&str, &str) = (
        "whisper-1",
        "https://api.openai.com/v1/audio/transcriptions",
    );

    let pref = get_app_setting_value(state, "stt/provider")?
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "auto".to_string());

    let groq_key = first_non_empty_setting(state, &["llm/groq_api_key"])?
        .or_else(|| std::env::var("GROQ_API_KEY").ok())
        .filter(|k| !k.is_empty());
    let openai_key = get_app_setting_value(state, "llm/openai_api_key")?
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .filter(|k| !k.is_empty());

    match pref.as_str() {
        "groq" => {
            let key = groq_key.ok_or_else(|| {
                "stt/provider is 'groq' but no Groq API key is configured (set llm/groq_api_key).".to_string()
            })?;
            Ok((key, GROQ.0, GROQ.1))
        }
        "openai" => {
            let key = openai_key.ok_or_else(|| {
                "stt/provider is 'openai' but no OpenAI API key is configured (set llm/openai_api_key).".to_string()
            })?;
            Ok((key, OPENAI.0, OPENAI.1))
        }
        // "auto" (and any unrecognized value): prefer Groq, else OpenAI.
        _ => {
            if let Some(key) = groq_key {
                Ok((key, GROQ.0, GROQ.1))
            } else if let Some(key) = openai_key {
                Ok((key, OPENAI.0, OPENAI.1))
            } else {
                Err("No transcription API key configured. Set llm/groq_api_key (or llm/openai_api_key) in settings.".to_string())
            }
        }
    }
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

/// Evaluate a chained task's natural-language gate condition against the upstream task's
/// output. Returns `(should_proceed, reason)`. One-shot, non-streaming, deliberately cheap.
///
/// Fails *closed*: any API/parse error returns `(false, ...)` so an ambiguous result never
/// fires a downstream action (which could send mail, write files, etc.). A missed run is
/// recoverable; a spurious one may not be.
pub(crate) async fn evaluate_chain_condition(
    state: &AppState,
    condition: &str,
    upstream_output: &str,
) -> (bool, String) {
    let cfg = match resolve_llm_config(state) {
        Ok(c) => c,
        Err(e) => return (false, format!("no LLM configured: {e}")),
    };
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = cfg;

    // Cap the data we send — gate decisions don't need the full payload.
    let data = crate::tools::truncate_chars(upstream_output, 6000);
    let prompt = format!(
        "A scheduled automation produced the output below. Decide whether this condition is satisfied:\n\n\
        CONDITION: {condition}\n\n\
        OUTPUT:\n{data}\n\n\
        Answer with exactly YES or NO on the first line, then one short sentence explaining why."
    );
    let system = "You are a strict gate that decides whether a condition is met by some data. \
        Answer YES only when the condition is clearly satisfied; answer NO if it is not satisfied or you are unsure. \
        First line must be exactly YES or NO.";

    let request_body = if is_anthropic {
        json!({
            "model": model, "max_tokens": 150, "stream": false,
            "system": system,
            "messages": [{"role": "user", "content": prompt}]
        })
    } else {
        json!({
            "model": model, "max_tokens": 150, "stream": false,
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
            .json(&request_body).send().await
    } else {
        state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await
    };
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => return (false, format!("eval call failed: HTTP {}", r.status())),
        Err(e) => return (false, format!("eval call error: {e}")),
    };
    let body: Value = match resp.json().await {
        Ok(b) => b,
        Err(e) => return (false, format!("eval parse error: {e}")),
    };
    let text = if is_anthropic {
        body.get("content").and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text")).and_then(Value::as_str).unwrap_or("")
    } else {
        body.get("choices").and_then(Value::as_array).and_then(|a| a.first())
            .and_then(|c| c.get("message")).and_then(|m| m.get("content"))
            .and_then(Value::as_str).unwrap_or("")
    };
    parse_condition_verdict(&crate::feeds::strip_think_tags(text))
}

/// Evaluate whether a session's goal has been FULLY achieved, judging by the assistant's
/// latest report. Returns `(met, reason)` where `reason` is the judge's one-sentence rationale.
///
/// Unlike `evaluate_chain_condition`, this does NOT fail closed: any API/parse error returns
/// `Err(...)` so the goal loop can surface the failure and pause (rather than silently deciding
/// the goal is or isn't met). Conservative on the affirmative side: partial progress → NO.
pub(crate) async fn evaluate_goal_met(
    state: &AppState,
    goal: &str,
    last_reply: &str,
) -> Result<(bool, String), String> {
    let cfg = resolve_llm_config(state).map_err(|e| format!("no LLM configured: {e}"))?;
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = cfg;

    let data = crate::tools::truncate_chars(last_reply, 6000);
    let prompt = format!(
        "An autonomous assistant is working toward a goal. Below is the goal and the assistant's \
        latest report. Decide whether the goal has been FULLY achieved.\n\n\
        GOAL: {goal}\n\n\
        ASSISTANT'S LATEST REPORT:\n{data}\n\n\
        Answer with exactly YES or NO on the first line, then one short sentence explaining why."
    );
    let system = "You are a strict judge deciding whether an objective has been FULLY achieved \
        based on the assistant's latest report. The first line must be exactly YES (the goal is \
        completely and verifiably met) or NO (it is not). Be conservative: if the work is only \
        partially done, still in progress, uncertain, or the assistant is blocked, answer NO. \
        Then add one short sentence explaining why.";

    let request_body = if is_anthropic {
        json!({
            "model": model, "max_tokens": 150, "stream": false,
            "system": system,
            "messages": [{"role": "user", "content": prompt}]
        })
    } else {
        json!({
            "model": model, "max_tokens": 150, "stream": false,
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
            .json(&request_body).send().await
    } else {
        state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await
    };
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => return Err(format!("eval call failed: HTTP {}", r.status())),
        Err(e) => return Err(format!("eval call error: {e}")),
    };
    let body: Value = resp.json().await.map_err(|e| format!("eval parse error: {e}"))?;
    let text = if is_anthropic {
        body.get("content").and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text")).and_then(Value::as_str).unwrap_or("")
    } else {
        body.get("choices").and_then(Value::as_array).and_then(|a| a.first())
            .and_then(|c| c.get("message")).and_then(|m| m.get("content"))
            .and_then(Value::as_str).unwrap_or("")
    };
    Ok(parse_condition_verdict(&crate::feeds::strip_think_tags(text)))
}

/// Generate a short human title for a chat session from its first exchange. One-shot,
/// non-streaming, deliberately cheap. Errors are returned (the caller treats titling as
/// best-effort and just leaves the session untitled).
pub(crate) async fn generate_session_title(
    state: &AppState,
    user_msg: &str,
    assistant_reply: &str,
) -> Result<String, String> {
    let cfg = resolve_llm_config(state).map_err(|e| format!("no LLM configured: {e}"))?;
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = cfg;

    let user_part = crate::tools::truncate_chars(user_msg, 1500);
    let reply_part = crate::tools::truncate_chars(assistant_reply, 1500);
    let prompt = format!(
        "Name this chat conversation.\n\nUSER'S FIRST MESSAGE:\n{user_part}\n\n\
        ASSISTANT'S REPLY (excerpt):\n{reply_part}\n\n\
        Reply with ONLY the title — 2 to 6 words, no quotes, no trailing punctuation."
    );
    let system = "You name chat conversations. Reply with only a concise 2-6 word title \
        describing the topic — no quotes, no punctuation at the end, no explanations.";

    let request_body = if is_anthropic {
        json!({
            "model": model, "max_tokens": 40, "stream": false,
            "system": system,
            "messages": [{"role": "user", "content": prompt}]
        })
    } else {
        json!({
            "model": model, "max_tokens": 40, "stream": false,
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
            .json(&request_body).send().await
    } else {
        state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await
    };
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => return Err(format!("title call failed: HTTP {}", r.status())),
        Err(e) => return Err(format!("title call error: {e}")),
    };
    let body: Value = resp.json().await.map_err(|e| format!("title parse error: {e}"))?;
    let text = if is_anthropic {
        body.get("content").and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text")).and_then(Value::as_str).unwrap_or("")
    } else {
        body.get("choices").and_then(Value::as_array).and_then(|a| a.first())
            .and_then(|c| c.get("message")).and_then(|m| m.get("content"))
            .and_then(Value::as_str).unwrap_or("")
    };
    let title = sanitize_session_title(&crate::feeds::strip_think_tags(text));
    if title.is_empty() {
        return Err("title model returned nothing usable".to_string());
    }
    Ok(title)
}

/// Normalize a model-proposed session title: first non-empty line, quotes/backticks and
/// trailing punctuation stripped, whitespace collapsed, capped at 60 chars on a char boundary.
fn sanitize_session_title(text: &str) -> String {
    let first_line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let stripped = first_line
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '“' | '”'))
        .trim_end_matches(['.', '!', ':', ';', ','])
        .trim();
    let collapsed: String = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(60).collect::<String>().trim_end().to_string()
}

/// Interpret a gate model's reply: met only on an explicit affirmative first token, so blank
/// or garbled output fails closed (no downstream action). Returns `(met, reason)`.
fn parse_condition_verdict(text: &str) -> (bool, String) {
    let trimmed = text.trim();
    let met = trimmed.to_uppercase().starts_with("YES");
    let reason = trimmed
        .lines()
        .map(str::trim)
        .rfind(|s| !s.is_empty())
        .unwrap_or(if met { "condition met" } else { "condition not met" });
    (met, reason.to_string())
}

/// Verdict from the auto-verify safety reviewer for a dangerous command. The `String`
/// carries the reviewer's one-sentence rationale (or the error, when failing closed).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CommandVerdict {
    /// Safe to run in context despite matching a broad danger pattern.
    Allow(String),
    /// Clearly harmful — do not run; report the reason back to the model.
    Deny(String),
    /// Uncertain / a genuine user judgment call — ask the user (or defer).
    Escalate(String),
}

/// LLM security review of a dangerous shell command before we interrupt the user.
///
/// One-shot, non-streaming, modeled on `evaluate_chain_condition`. **Fails closed**: any
/// API/parse error or unrecognized verdict returns `Escalate` (with the error as the note)
/// so an ambiguous result always routes to human judgment rather than silently allowing.
pub(crate) async fn verify_dangerous_command(
    state: &AppState,
    command: &str,
    cwd: Option<&str>,
    workspace_root: &str,
    reasons: &[String],
) -> CommandVerdict {
    let cfg = match resolve_llm_config(state) {
        Ok(c) => c,
        Err(e) => return CommandVerdict::Escalate(format!("no LLM configured: {e}")),
    };
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = cfg;

    let cwd_str = cwd.unwrap_or("(workspace root)");
    let reasons_str = if reasons.is_empty() {
        "(none provided)".to_string()
    } else {
        reasons.join("; ")
    };
    let cmd = crate::tools::truncate_chars(command, 2000);

    let system = "You are a strict security reviewer for an autonomous agent that runs shell \
        commands inside a workspace directory the user has already approved. Your job is to \
        decide whether a command flagged as potentially dangerous should run, be blocked, or be \
        escalated to the user. Your first line MUST be exactly one of: ALLOW, DENY, or ESCALATE. \
        Then, on the next line, give one short sentence explaining why. \
        Guidance: ALLOW only when the command is clearly safe in this context despite matching a \
        broad pattern (for example, `rm -rf` of a path that is clearly inside the workspace build \
        or cache directory). DENY when the command is clearly harmful, irreversible outside the \
        workspace, or system-damaging (for example, deleting home/system paths, disabling \
        security, exfiltrating secrets, or piping remote scripts to a shell). ESCALATE when you \
        are uncertain or the decision is genuinely the user's to make. When in doubt, ESCALATE.";

    let prompt = format!(
        "Review this command flagged as potentially dangerous.\n\n\
        COMMAND:\n{cmd}\n\n\
        WORKING DIRECTORY: {cwd_str}\n\
        WORKSPACE ROOT: {workspace_root}\n\
        FLAGGED BECAUSE: {reasons_str}\n\n\
        First line exactly ALLOW, DENY, or ESCALATE. Then one short sentence explaining why."
    );

    let request_body = if is_anthropic {
        json!({
            "model": model, "max_tokens": 150, "stream": false,
            "system": system,
            "messages": [{"role": "user", "content": prompt}]
        })
    } else {
        json!({
            "model": model, "max_tokens": 150, "stream": false,
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
            .json(&request_body).send().await
    } else {
        state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await
    };
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => return CommandVerdict::Escalate(format!("verifier call failed: HTTP {}", r.status())),
        Err(e) => return CommandVerdict::Escalate(format!("verifier call error: {e}")),
    };
    let body: Value = match resp.json().await {
        Ok(b) => b,
        Err(e) => return CommandVerdict::Escalate(format!("verifier parse error: {e}")),
    };
    let text = if is_anthropic {
        body.get("content").and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text")).and_then(Value::as_str).unwrap_or("")
    } else {
        body.get("choices").and_then(Value::as_array).and_then(|a| a.first())
            .and_then(|c| c.get("message")).and_then(|m| m.get("content"))
            .and_then(Value::as_str).unwrap_or("")
    };
    parse_command_verdict(&crate::feeds::strip_think_tags(text))
}

/// Interpret the verifier model's reply. The first non-empty line must start with ALLOW,
/// DENY, or ESCALATE; anything else fails closed to `Escalate`. The note is the reviewer's
/// following sentence (or a default when the model gave none).
fn parse_command_verdict(text: &str) -> CommandVerdict {
    let trimmed = text.trim();
    let mut lines = trimmed.lines().map(str::trim).filter(|s| !s.is_empty());
    let first = lines.next().unwrap_or("");
    let upper = first.to_uppercase();
    // The note is the remaining text after the verdict line; fall back to any trailing
    // words on the verdict line itself, then to a generic default.
    let rest: String = lines.collect::<Vec<_>>().join(" ");
    let note = if !rest.is_empty() {
        rest
    } else {
        // Strip a leading verdict word from the first line, if the model put the reason inline.
        let inline = first
            .trim_start_matches(|c: char| c.is_alphabetic())
            .trim_start_matches([':', '-', '.', ' '])
            .trim();
        inline.to_string()
    };

    // Match the first WORD exactly (allowing trailing punctuation), not a prefix — a reply
    // like "ALLOWING this would be harmful" must fail closed, not parse as Allow.
    let first_word = upper
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches([':', '.', ',', '!', '-']);
    match first_word {
        "ALLOW" => CommandVerdict::Allow(if note.is_empty() { "verifier allowed the command".to_string() } else { note }),
        "DENY" => CommandVerdict::Deny(if note.is_empty() { "verifier denied the command".to_string() } else { note }),
        "ESCALATE" => CommandVerdict::Escalate(if note.is_empty() { "verifier escalated for user judgment".to_string() } else { note }),
        _ => CommandVerdict::Escalate(format!("unrecognized verifier verdict: {}", crate::tools::truncate_chars(trimmed, 200))),
    }
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

/// Per-platform copywriting guidance baked into the social-copy prompt.
fn platform_guidance(platform: &str) -> &'static str {
    match platform.to_lowercase().replace(['/', ' '], "").as_str() {
        "x" | "twitter" | "xtwitter" => "≤280 characters, punchy, a strong hook in the first words, 0–2 hashtags, no filler.",
        "linkedin" => "Professional but human. A scroll-stopping first line, 2–4 short value-packed paragraphs, a clear takeaway, 0–3 hashtags.",
        "instagram" => "An engaging caption with a hook and light emoji, a call-to-action, then 5–10 relevant hashtags on a new line.",
        "tiktok" => "Very short, hook-driven caption in a trend-aware voice, 2–4 hashtags.",
        "youtube" => "A compelling video title, then a 2–3 sentence description with searchable keywords. Keep Shorts snappy.",
        _ => "Platform-appropriate copy with a strong hook and no generic filler.",
    }
}

/// Pull a JSON array out of a model response, tolerating code fences / stray prose.
fn extract_json_array(text: &str) -> Option<Vec<Value>> {
    if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(text.trim()) {
        return Some(a);
    }
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&text[start..=end])
        .ok()
        .and_then(|v| v.as_array().cloned())
}

/// Pull a JSON object out of a model response, tolerating code fences / stray prose.
fn extract_json_object(text: &str) -> Option<Value> {
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(text.trim()) {
        return Some(v);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&text[start..=end]).ok().filter(Value::is_object)
}

/// Generate tailored social-media post copy (3 variants per platform) in one LLM call.
/// Returns structured per-platform variants for the Studio tile.
pub(crate) async fn generate_social_copy(
    state: &AppState,
    topic: &str,
    platforms: &[String],
    tone: Option<&str>,
) -> Result<Vec<crate::models::PlatformCopy>, String> {
    if platforms.is_empty() {
        return Err("Select at least one platform.".to_string());
    }
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state)?;

    // Shared brand voice (persisted in Settings via the Studio header) + the per-request tone.
    let brand_voice = get_app_setting_value(state, "studio/brand_voice").ok().flatten().unwrap_or_default();
    let mut tone_line = String::new();
    if !brand_voice.trim().is_empty() {
        tone_line.push_str(&format!("Brand voice (apply to all copy): {}\n", brand_voice.trim()));
    }
    if let Some(t) = tone.map(str::trim).filter(|t| !t.is_empty()) {
        tone_line.push_str(&format!("Tone for this batch: {}\n", t));
    }
    let mut platform_specs = String::new();
    for p in platforms {
        platform_specs.push_str(&format!("- {}: {}\n", p, platform_guidance(p)));
    }

    let prompt = format!(
        "Write social media post copy about this topic.\n\nTopic: {topic}\n{tone_line}\n\
        Generate copy for these platforms, each tailored to its norms:\n{platform_specs}\n\
        For EACH platform write 3 distinct variants that take different angles or hooks.\n\n\
        Return ONLY valid JSON — an array where each element is \
        {{\"platform\": \"<platform name exactly as given above>\", \"variants\": [\"variant 1\", \"variant 2\", \"variant 3\"]}}. \
        No markdown, no code fences, no commentary — output the JSON array and nothing else."
    );
    let system = "You are an expert social media copywriter. You tailor copy precisely to each platform's format and audience, write scroll-stopping hooks, and never use generic filler. You output strict JSON only.";

    let request_body = if is_anthropic {
        json!({
            "model": model, "max_tokens": 4000, "stream": false, "system": system,
            "messages": [{"role": "user", "content": prompt}]
        })
    } else {
        json!({
            "model": model, "max_tokens": 4000, "stream": false,
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
        return Err(format!("Copy generation failed ({status}): {body}"));
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
    .ok_or_else(|| "Copy generation returned no text".to_string())?;

    let parsed = extract_json_array(&text)
        .ok_or_else(|| format!("Could not parse copy as JSON. Model said:\n{}", text.trim()))?;
    let mut out = Vec::new();
    for el in parsed {
        let platform = el.get("platform").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let variants: Vec<String> = el
            .get("variants")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        if !platform.is_empty() && !variants.is_empty() {
            out.push(crate::models::PlatformCopy { platform, variants });
        }
    }
    if out.is_empty() {
        return Err(format!("No usable copy was generated. Model said:\n{}", text.trim()));
    }
    Ok(out)
}

/// Writing-section assistant: apply `instruction` to `text` (or, when `text` is empty,
/// treat the instruction as a "write this" request). Returns the revised/written prose,
/// honoring the persisted brand voice. One-shot, non-streaming.
pub(crate) async fn writing_assist(
    state: &AppState,
    text: &str,
    instruction: &str,
) -> Result<String, String> {
    if instruction.trim().is_empty() {
        return Err("Tell me what to do (e.g. improve, shorten, continue, or write…).".to_string());
    }
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state)?;

    let brand_voice = get_app_setting_value(state, "studio/brand_voice").ok().flatten().unwrap_or_default();
    let voice_line = if brand_voice.trim().is_empty() {
        String::new()
    } else {
        format!("Write in this brand voice: {}\n\n", brand_voice.trim())
    };

    let body_block = if text.trim().is_empty() {
        "(No existing draft — produce the text from scratch per the instruction.)".to_string()
    } else {
        format!("Here is the current draft:\n\"\"\"\n{}\n\"\"\"", text.trim())
    };

    let prompt = format!(
        "{voice_line}{body_block}\n\nInstruction: {instruction}\n\n\
        Return ONLY the resulting text (Markdown is fine). No preamble, no explanation, no quotes around it."
    );
    let system = "You are a sharp, versatile writing assistant and editor. You improve clarity, flow, and impact while preserving the author's intent. You return only the requested text.";

    let max_tokens = resolve_max_output_tokens(state);
    let request_body = if is_anthropic {
        json!({"model": model, "max_tokens": max_tokens, "stream": false, "system": system,
               "messages": [{"role": "user", "content": prompt}]})
    } else {
        json!({"model": model, "max_tokens": max_tokens, "stream": false,
               "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}]})
    };

    let resp = if is_anthropic {
        state.http.post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send().await.map_err(|e| e.to_string())?
    } else {
        state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await.map_err(|e| e.to_string())?
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Writing request failed ({status}): {body}"));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let out = if is_anthropic {
        body.get("content").and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text")).and_then(Value::as_str).map(|s| s.to_string())
    } else {
        body.get("choices").and_then(Value::as_array).and_then(|arr| arr.first())
            .and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(Value::as_str).map(|s| s.to_string())
    }
    .ok_or_else(|| "Writing assistant returned no text".to_string())?;
    let out = out.trim().to_string();
    if out.is_empty() {
        return Err("Writing assistant returned empty text".to_string());
    }
    Ok(out)
}

/// Generate a social carousel/graphic as inline-styled HTML slides (one string per slide).
/// Inline styles only — no `<style>` blocks (which would leak into the app) and no external
/// resources — so the frontend can rasterize each slide with html2canvas reliably.
pub(crate) async fn generate_carousel(
    state: &AppState,
    topic: &str,
    slides: u32,
    width: u32,
    height: u32,
) -> Result<Vec<String>, String> {
    if topic.trim().is_empty() {
        return Err("Describe what the graphic should say.".to_string());
    }
    let n = slides.clamp(1, 10);
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state)?;
    let brand_voice = get_app_setting_value(state, "studio/brand_voice").ok().flatten().unwrap_or_default();
    let voice_line = if brand_voice.trim().is_empty() {
        String::new()
    } else {
        format!("Brand voice / visual style to reflect: {}\n", brand_voice.trim())
    };

    let prompt = format!(
        "Design a {n}-slide social media {kind} about:\n{topic}\n\n{voice_line}\n\
        Return ONLY a JSON array of {n} strings — each string is the HTML for one slide.\n\
        STRICT requirements for every slide:\n\
        - Use ONLY inline styles (the `style` attribute). NO <style> blocks, NO class names, NO <script>, NO external images or web fonts.\n\
        - The outermost element MUST be a single <div> sized exactly `width:{width}px;height:{height}px` with `box-sizing:border-box;overflow:hidden;` a background (a solid color or CSS gradient) and generous padding.\n\
        - Use only system-safe fonts: -apple-system, 'Helvetica Neue', Helvetica, Arial, Georgia, sans-serif.\n\
        - Strong visual hierarchy: a big bold headline and supporting text; high contrast; lay out with inline flexbox (style=\"display:flex;flex-direction:column;justify-content:center;...\").\n\
        - Slide 1 is a striking cover/hook. Keep text on each slide short and punchy.\n\
        No commentary, no code fences — just the JSON array of HTML strings.",
        kind = if n > 1 { "carousel" } else { "graphic" }
    );
    let system = "You are a world-class social media graphic designer who builds striking, self-contained, inline-styled HTML cards. You never use <style> blocks, classes, scripts, or external resources. You return only a JSON array of HTML strings.";

    let max_tokens = resolve_max_output_tokens(state);
    let request_body = if is_anthropic {
        json!({"model": model, "max_tokens": max_tokens, "stream": false, "system": system,
               "messages": [{"role": "user", "content": prompt}]})
    } else {
        json!({"model": model, "max_tokens": max_tokens, "stream": false,
               "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}]})
    };

    let resp = if is_anthropic {
        state.http.post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send().await.map_err(|e| e.to_string())?
    } else {
        state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await.map_err(|e| e.to_string())?
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Carousel generation failed ({status}): {body}"));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let text = if is_anthropic {
        body.get("content").and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text")).and_then(Value::as_str).map(|s| s.to_string())
    } else {
        body.get("choices").and_then(Value::as_array).and_then(|arr| arr.first())
            .and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(Value::as_str).map(|s| s.to_string())
    }
    .ok_or_else(|| "Carousel generation returned no text".to_string())?;

    let arr = extract_json_array(&text)
        .ok_or_else(|| format!("Could not parse slides as JSON. Model said:\n{}", text.trim()))?;
    let slides_html: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
        .collect();
    if slides_html.is_empty() {
        return Err(format!("No usable slides were generated. Model said:\n{}", text.trim()));
    }
    Ok(slides_html)
}

/// Strip a leading/trailing Markdown code fence (```html … ```) the model may wrap output in.
fn strip_code_fences(text: &str) -> String {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // Drop the optional language tag on the first line, then the trailing fence.
        let after_lang = rest.splitn(2, '\n').nth(1).unwrap_or("");
        return after_lang.trim_end().trim_end_matches("```").trim().to_string();
    }
    t.to_string()
}

/// Generate a complete, self-contained HTML document (slide deck or print-ready doc) for the
/// Studio Docs section. Honors the persisted brand voice. One-shot, non-streaming.
pub(crate) async fn generate_doc_html(
    state: &AppState,
    doc_type: &str,
    topic: &str,
    slides: u32,
) -> Result<String, String> {
    if topic.trim().is_empty() {
        return Err("Describe what the document should cover.".to_string());
    }
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state)?;
    let brand_voice = get_app_setting_value(state, "studio/brand_voice").ok().flatten().unwrap_or_default();
    let voice_line = if brand_voice.trim().is_empty() {
        String::new()
    } else {
        format!("Brand voice / style to reflect: {}\n", brand_voice.trim())
    };

    let prompt = if doc_type == "slides" {
        let n = slides.clamp(3, 20);
        format!(
            "Create a COMPLETE, self-contained HTML slide deck about:\n{topic}\n\n{voice_line}\n\
            Produce about {n} slides. Requirements:\n\
            - ONE html document with an embedded <style> block. NO external resources (no CDN links, no web fonts, no <script>).\n\
            - Each slide is `<section class=\"slide\">…</section>`. First slide is a title slide.\n\
            - Clean, modern, high-contrast design: large readable type, generous spacing, a consistent color theme, a subtle accent.\n\
            - Include print CSS so each slide prints on its own landscape page: `@media print {{ .slide {{ page-break-after: always; }} }}` and size slides ~16:9.\n\
            Return ONLY the HTML, starting with <!DOCTYPE html>. No commentary, no code fences."
        )
    } else {
        format!(
            "Create a COMPLETE, self-contained, print-ready HTML document about:\n{topic}\n\n{voice_line}\n\
            Requirements:\n\
            - ONE html file with an embedded <style> block. NO external resources (no CDN links, no web fonts, no <script>).\n\
            - Clean typographic design: clear hierarchy of headings, readable body text, sensible margins, a title block.\n\
            - Include print CSS with good page margins.\n\
            Return ONLY the HTML, starting with <!DOCTYPE html>. No commentary, no code fences."
        )
    };
    let system = "You are an expert document and presentation designer who writes clean, self-contained, print-ready HTML/CSS. You never use external resources or scripts. You return only the HTML.";

    let max_tokens = resolve_max_output_tokens(state);
    let request_body = if is_anthropic {
        json!({"model": model, "max_tokens": max_tokens, "stream": false, "system": system,
               "messages": [{"role": "user", "content": prompt}]})
    } else {
        json!({"model": model, "max_tokens": max_tokens, "stream": false,
               "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}]})
    };

    let resp = if is_anthropic {
        state.http.post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send().await.map_err(|e| e.to_string())?
    } else {
        state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await.map_err(|e| e.to_string())?
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Doc generation failed ({status}): {body}"));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let text = if is_anthropic {
        body.get("content").and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text")).and_then(Value::as_str).map(|s| s.to_string())
    } else {
        body.get("choices").and_then(Value::as_array).and_then(|arr| arr.first())
            .and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(Value::as_str).map(|s| s.to_string())
    }
    .ok_or_else(|| "Doc generation returned no text".to_string())?;

    let html = strip_code_fences(&text);
    if !html.to_lowercase().contains("<html") && !html.to_lowercase().contains("<!doctype") {
        return Err(format!("Model did not return HTML. It said:\n{}", html.chars().take(400).collect::<String>()));
    }
    Ok(html)
}

/// Turn a natural-language edit instruction + the video's analysis (probe, transcript,
/// silences) into a structured `EditPlan` for the Studio Video tab. One-shot JSON call,
/// validated by deserializing into the plan struct.
///
/// When `current_plan` is `Some`, this is a *refinement*: the existing plan's JSON is
/// included in the prompt with instructions to mutate only what the new instruction asks
/// for and preserve every other field verbatim (so "make the captions karaoke" doesn't
/// silently drop segments/filters/texts from a prior AI or manual edit).
pub(crate) async fn generate_video_edit_plan(
    state: &AppState,
    instruction: &str,
    probe: &crate::video::VideoProbe,
    transcript: Option<&crate::video::Transcript>,
    silences: &[crate::video::SilenceRange],
    filler_ranges: &[crate::video::SilenceRange],
    current_plan: Option<&crate::video::EditPlan>,
) -> Result<crate::video::EditPlan, String> {
    if instruction.trim().is_empty() {
        return Err("Describe the edit you want.".to_string());
    }
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state)?;

    let presets = crate::video::FILTER_PRESETS
        .iter()
        .map(|(k, label, _)| format!("\"{k}\" ({label})"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut context = format!(
        "Video: {:.1}s, {}x{}, {:.0} fps, audio: {}.\n",
        probe.duration_s, probe.width, probe.height, probe.fps,
        if probe.has_audio { "yes" } else { "no" }
    );
    if !silences.is_empty() {
        context.push_str("Detected silences (source seconds):\n");
        for s in silences.iter().take(60) {
            context.push_str(&format!("  {:.2}–{:.2}\n", s.start, s.end));
        }
    }
    if !filler_ranges.is_empty() {
        context.push_str(&format!(
            "Detected filler words/stutters ({} found — \"um\", \"uh\", repeated words, etc., source seconds):\n",
            filler_ranges.len()
        ));
        for f in filler_ranges.iter().take(60) {
            context.push_str(&format!("  {:.2}–{:.2}\n", f.start, f.end));
        }
    }
    if let Some(t) = transcript {
        if !t.segments.is_empty() {
            context.push_str("Transcript (source seconds):\n");
            let mut budget = 0usize;
            for seg in &t.segments {
                let line = format!("  [{:.1}–{:.1}] {}\n", seg.start, seg.end, seg.text);
                budget += line.len();
                if budget > 24_000 {
                    context.push_str("  … (transcript truncated)\n");
                    break;
                }
                context.push_str(&line);
            }
        }
    }

    let refinement_note = if let Some(existing) = current_plan {
        let existing_json = serde_json::to_string(existing)
            .map_err(|e| format!("could not serialize current plan: {e}"))?;
        context.push_str(&format!(
            "\nThe user already has an edit plan in progress (JSON below). This is a REFINEMENT \
            request: apply ONLY what the new instruction below asks for, and copy every other \
            field from this existing plan UNCHANGED (segments, filter, texts, aspect, smart_reframe, \
            speed, mute, captions/caption_style, normalize_audio, denoise, fade, output_name). Do not \
            reset or drop fields the instruction doesn't mention.\n\
            Current plan JSON:\n{existing_json}\n"
        ));
        "Treat the instruction as an incremental change to the current plan above, not a fresh edit — \
        preserve every field it doesn't ask you to change. "
    } else {
        ""
    };

    let prompt = format!(
        "You are planning a video edit. Produce an edit plan as JSON.\n\n{context}\n\
        User's instruction: {instruction}\n\n\
        {refinement_note}Return ONLY a JSON object with these fields (all optional unless noted):\n\
        - \"segments\": array of {{\"start\": s, \"end\": s, \"source\": optional path}} — ranges in final playback order. For a MAIN-video segment omit \"source\": start/end are SOURCE-timeline seconds to keep. Omit or [] to keep everything. Use the transcript/silences to pick what matters; cut dead air and filler. When the user asks to tighten things up, remove the \"ums\"/filler, or clean up stutters, exclude the detected filler-word/stutter ranges above from the kept segments. To splice in OTHER media, set \"source\" to a file path: a VIDEO file inserts a b-roll clip (start/end trim THAT file's own timeline); a still IMAGE (.png/.jpg/.jpeg/.webp) is a silent title/intro card shown for end-start seconds (use start=0, duration 0.2–30s). Only add inserts when the user asks for b-roll, a title card, or an intro/outro slide.\n\
        - \"filter\": one of {presets}\n\
        - \"texts\": array of {{\"text\", \"start\", \"end\", \"position\" (\"top\"|\"center\"|\"bottom\"), \"size\"}} — overlay times are on the OUTPUT timeline (after cuts and speed), so compute them from the kept segments' cumulative durations.\n\
        - \"aspect\": \"9:16\", \"1:1\", \"4:5\", or \"16:9\" (center-crop) — only if the user asks for a platform/orientation.\n\
        - \"smart_reframe\": true to keep faces in frame when converting to a narrower/vertical aspect (subject-aware crop instead of a static center-crop). Only meaningful with an \"aspect\" that narrows the video (e.g. 16:9 → 9:16); harmless otherwise. Prefer it when the user wants a vertical/TikTok/Reels/Shorts cut of talking-head footage.\n\
        - \"speed\": playback speed 0.25–4.0.\n\
        - \"mute\": true to drop audio.\n\
        - \"captions\": true to burn in word-timed subtitles from the transcript. Use when the user mentions captions/subtitles, or wants a social/silent-viewing cut (TikTok, Reels, Shorts).\n\
        - \"caption_style\": \"boxed\" (default — white text on a black box), \"outline\" (white with a heavy outline), or \"karaoke\" (word-by-word highlight). Only set when captions is true.\n\
        - \"normalize_audio\": true to loudness-normalize the mix to a publishable level (EBU R128, -14 LUFS). Use when the user wants the audio ready to publish/upload, or mentions it sounding too quiet/loud/inconsistent.\n\
        - \"denoise\": true to reduce background hiss/hum/fan noise from the recording. Use when the user mentions noisy audio, background noise, or a bad-sounding mic.\n\
        - \"fade\": overall fade-in/fade-out duration in seconds (0.1–5.0), applied to both video and audio at the very start and end of the clip. Use for a polished open/close; omit for a hard cut in/out.\n\
        - \"output_name\": short kebab-case stem for the output file.\n\n\
        Keep cuts conservative: pad speech boundaries ~0.2s so words aren't clipped. \
        No markdown, no code fences, no commentary — the JSON object only."
    );
    let system = "You are a precise video editor. You reason about timestamps carefully and output strict JSON only.";

    let request_body = if is_anthropic {
        json!({"model": model, "max_tokens": 4000, "stream": false, "system": system,
               "messages": [{"role": "user", "content": prompt}]})
    } else {
        json!({"model": model, "max_tokens": 4000, "stream": false,
               "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}]})
    };

    let resp = if is_anthropic {
        state.http.post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send().await.map_err(|e| e.to_string())?
    } else {
        state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await.map_err(|e| e.to_string())?
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Edit planning failed ({status}): {body}"));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let text = if is_anthropic {
        body.get("content").and_then(Value::as_array)
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
            .and_then(|b| b.get("text")).and_then(Value::as_str).map(|s| s.to_string())
    } else {
        body.get("choices").and_then(Value::as_array).and_then(|arr| arr.first())
            .and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(Value::as_str).map(|s| s.to_string())
    }
    .ok_or_else(|| "Edit planning returned no text".to_string())?;

    let obj = extract_json_object(&text)
        .ok_or_else(|| format!("Could not parse the edit plan as JSON. Model said:\n{}", text.trim()))?;
    let mut plan: crate::video::EditPlan =
        serde_json::from_value(obj).map_err(|e| format!("Edit plan had the wrong shape: {e}"))?;
    plan.validate(probe)?;
    Ok(plan)
}

/// Parse the model's shorts response into raw specs, tolerating a bare array, a `{"shorts": [...]}`
/// wrapper (or `specs`/`clips`/`highlights`), or a single bare spec object.
fn parse_shorts_specs(text: &str) -> Result<Vec<crate::video::ShortSpec>, String> {
    if let Some(obj) = extract_json_object(text) {
        for key in ["shorts", "specs", "clips", "highlights"] {
            if let Some(arr) = obj.get(key).and_then(Value::as_array) {
                return serde_json::from_value(Value::Array(arr.clone()))
                    .map_err(|e| format!("shorts had the wrong shape: {e}"));
            }
        }
        // A single unwrapped spec object is also acceptable.
        if obj.get("segments").is_some() {
            let spec: crate::video::ShortSpec =
                serde_json::from_value(obj).map_err(|e| format!("short had the wrong shape: {e}"))?;
            return Ok(vec![spec]);
        }
    }
    if let Some(arr) = extract_json_array(text) {
        return serde_json::from_value(Value::Array(arr))
            .map_err(|e| format!("shorts had the wrong shape: {e}"));
    }
    Err(format!("could not parse shorts JSON. Model said:\n{}", text.trim()))
}

/// Pick the best `count` highlight moments from a (possibly long) transcript for auto-shorts.
///
/// Feeds the LLM the SEGMENT-level transcript with timestamps (word-level timings are far too
/// many tokens for a 40-minute video; precise trims happen later at render), the video duration,
/// the target clip count + per-clip duration, and an optional user `brief`. Instructs the model
/// to pick self-contained moments with a strong hook up front, snapping boundaries to transcript
/// segments and never starting mid-sentence. Validates hard via `sanitize_short_specs` and
/// re-prompts once with the validation error before giving up.
pub(crate) async fn generate_shorts_specs(
    state: &AppState,
    transcript: &crate::video::Transcript,
    probe: &crate::video::VideoProbe,
    count: usize,
    duration_hint_s: Option<f64>,
    brief: Option<&str>,
) -> Result<Vec<crate::video::ShortSpec>, String> {
    let count = count.clamp(1, 8);
    let target_s = duration_hint_s
        .unwrap_or(crate::video::DEFAULT_SHORT_TARGET_S)
        .clamp(10.0, 180.0);
    if transcript.segments.is_empty() {
        return Err("no transcript segments to pick shorts from".to_string());
    }
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state)?;

    // Segment-level transcript with timestamps, budgeted so a long video still fits the context.
    let mut tr = String::new();
    let mut budget = 0usize;
    for seg in &transcript.segments {
        let line = format!("[{:.1}-{:.1}] {}\n", seg.start, seg.end, seg.text.trim());
        budget += line.len();
        if budget > 40_000 {
            tr.push_str("… (transcript truncated — pick from what's shown above)\n");
            break;
        }
        tr.push_str(&line);
    }

    let brief_line = match brief.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => format!("\nUser brief (prioritize this): {b}\n"),
        None => String::new(),
    };

    let base_prompt = format!(
        "You are an expert short-form video editor. From the transcript of a {dur:.0}s video, pick the \
        {count} BEST self-contained moments to cut into vertical short clips (~{target:.0}s each).\n\n\
        Transcript (segment start–end in source seconds):\n{tr}{brief}\n\
        Rules for every pick:\n\
        - Self-contained: it must stand on its own, with a strong HOOK in the first ~3 seconds and a clear payoff.\n\
        - Never start or end mid-sentence. Snap every start/end to the transcript segment boundaries shown above.\n\
        - Total kept duration per clip ≈ {target:.0}s (must be between {tmin:.0}s and {tmax:.0}s).\n\
        - A clip may stitch a few in-order, NON-overlapping segments if that makes a tighter moment.\n\
        - All timestamps are SOURCE seconds within [0, {dur:.0}]. Segment boundaries are precise enough — finer trims happen later.\n\n\
        Return ONLY a JSON object of this shape:\n\
        {{\"shorts\": [ {{\"title\": \"≤60 char title\", \"hook\": \"the opening line you're banking on\", \"rationale\": \"why this moment works as a short\", \"segments\": [{{\"start\": 12.0, \"end\": 55.0}}] }} ]}}\n\
        Order the array best-first. No markdown, no code fences, no commentary — the JSON object only.",
        dur = probe.duration_s,
        count = count,
        target = target_s,
        tr = tr,
        brief = brief_line,
        tmin = 0.5 * target_s,
        tmax = 2.0 * target_s,
    );
    let system = "You are a precise short-form video editor. You reason about timestamps carefully and output strict JSON only.";

    let mut attempt_prompt = base_prompt.clone();
    let mut last_err = String::new();
    for attempt in 0..2 {
        let request_body = if is_anthropic {
            json!({"model": model, "max_tokens": 4000, "stream": false, "system": system,
                   "messages": [{"role": "user", "content": attempt_prompt}]})
        } else {
            json!({"model": model, "max_tokens": 4000, "stream": false,
                   "messages": [{"role": "system", "content": system}, {"role": "user", "content": attempt_prompt}]})
        };
        let resp = if is_anthropic {
            state.http.post(&endpoint)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&request_body)
                .send().await.map_err(|e| e.to_string())?
        } else {
            state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await.map_err(|e| e.to_string())?
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Shorts planning failed ({status}): {body}"));
        }
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let text = if is_anthropic {
            body.get("content").and_then(Value::as_array)
                .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
                .and_then(|b| b.get("text")).and_then(Value::as_str).map(|s| s.to_string())
        } else {
            body.get("choices").and_then(Value::as_array).and_then(|arr| arr.first())
                .and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(Value::as_str).map(|s| s.to_string())
        }
        .unwrap_or_default();

        match parse_shorts_specs(&text)
            .and_then(|specs| crate::video::sanitize_short_specs(specs, probe, count, target_s))
        {
            Ok(specs) => return Ok(specs),
            Err(e) => {
                last_err = e;
                if attempt == 0 {
                    attempt_prompt = format!(
                        "{base_prompt}\n\nYour previous answer was rejected: {last_err}\nReturn corrected JSON only."
                    );
                }
            }
        }
    }
    Err(format!("Could not get a usable set of shorts from the model: {last_err}"))
}

/// Parse a compilation-spec JSON object from the model's reply (tolerating code fences and a
/// little surrounding prose). Accepts either a bare object or one wrapped under a common key.
fn parse_compilation_spec(text: &str) -> Result<crate::video::CompilationSpec, String> {
    if let Some(obj) = extract_json_object(text) {
        for key in ["compilation", "spec", "result", "plan"] {
            if let Some(inner) = obj.get(key) {
                if inner.is_object() {
                    return serde_json::from_value(inner.clone())
                        .map_err(|e| format!("compilation had the wrong shape: {e}"));
                }
            }
        }
        if obj.get("segments").is_some() {
            return serde_json::from_value(obj).map_err(|e| format!("compilation had the wrong shape: {e}"));
        }
    }
    Err(format!("could not parse compilation JSON. Model said:\n{}", text.trim()))
}

/// Plan a multi-clip compilation: pick which parts of which clips to keep and in what order,
/// detecting retakes ("takes" — same content re-recorded, keep the best take per section) vs a
/// montage ("montage" — distinct moments across clips, ordered for flow).
///
/// Feeds the LLM each clip numbered with its duration, segment-level transcript (budgeted so
/// several long clips still fit the context), and filler/silence stats. `mode` is "auto"
/// (detect + report in `mode_detected`), "takes", or "montage". Validates hard via
/// `sanitize_compilation_spec` and re-prompts once with the validation error before giving up.
pub(crate) async fn generate_compilation_specs(
    state: &AppState,
    clips: &[crate::video::ClipInfo],
    brief: Option<&str>,
    mode: &str,
    target_duration_s: Option<f64>,
) -> Result<crate::video::CompilationSpec, String> {
    if clips.len() < 2 {
        return Err("compilation needs at least 2 clips".to_string());
    }
    let mode = match mode.trim().to_lowercase().as_str() {
        "takes" => "takes",
        "montage" => "montage",
        _ => "auto",
    };
    let target_s = target_duration_s.filter(|t| *t > 0.0);
    let LlmCallConfig { model, is_anthropic, api_key, endpoint, .. } = resolve_llm_config(state)?;

    let clip_durations: Vec<(String, f64)> =
        clips.iter().map(|c| (c.path.clone(), c.duration_s)).collect();

    // Numbered clips: each with duration + filler/silence stats + its segment-level transcript,
    // sharing a global char budget so a set of long clips still fits the context.
    let mut clip_block = String::new();
    let mut budget = 0usize;
    let mut truncated = false;
    for (i, c) in clips.iter().enumerate() {
        let header = format!(
            "\n=== Clip {n} — source: {src}\nduration: {dur:.1}s | fillers: {fill} | silence: {sil:.0}%\ntranscript (source seconds):\n",
            n = i + 1,
            src = c.path,
            dur = c.duration_s,
            fill = c.filler_count,
            sil = c.silence_ratio * 100.0,
        );
        clip_block.push_str(&header);
        if c.transcript_segments.is_empty() {
            clip_block.push_str("(no transcript available for this clip — use its duration only)\n");
            continue;
        }
        for seg in &c.transcript_segments {
            let line = format!("[{:.1}-{:.1}] {}\n", seg.start, seg.end, seg.text.trim());
            budget += line.len();
            if budget > 48_000 {
                clip_block.push_str("… (transcript truncated — plan from what's shown)\n");
                truncated = true;
                break;
            }
            clip_block.push_str(&line);
        }
        if truncated {
            break;
        }
    }

    let brief_line = match brief.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => format!("\nUser brief (prioritize this): {b}\n"),
        None => String::new(),
    };
    let target_line = match target_s {
        Some(t) => format!(
            "Target total duration ≈ {t:.0}s (the final compilation must be between {lo:.0}s and {hi:.0}s).\n",
            t = t, lo = 0.5 * t, hi = 2.0 * t
        ),
        None => "No fixed target duration — keep it as long as the strong material warrants, no longer.\n".to_string(),
    };
    let mode_line = match mode {
        "takes" => "MODE: takes. Treat the clips as retakes of the same material.\n".to_string(),
        "montage" => "MODE: montage. Treat the clips as distinct content to weave together.\n".to_string(),
        _ => "MODE: auto — YOU decide. If the clips heavily overlap in transcript (the same lines re-recorded), it's \"takes\"; if they cover distinct content, it's \"montage\". Report which in mode_detected.\n".to_string(),
    };

    let base_prompt = format!(
        "You are an expert video editor assembling ONE finished video from {count} source clips.\n\
        {mode_line}{target_line}{brief}\n\
        The clips:\n{clips}\n\n\
        How to choose:\n\
        - TAKES: align the takes into sections (the same sentence/idea attempted more than once). For each \
        section pick the SINGLE best take — complete sentences, fewest fillers/stutters, no long pauses. \
        Assemble the chosen takes in natural narrative order. NEVER mix two takes mid-sentence.\n\
        - MONTAGE: pick the strongest self-contained moments from across the clips honoring the brief and the \
        target duration. Order them for flow — lead with the best hook.\n\
        Universal rules:\n\
        - Every segment MUST set \"source\" to one of the exact clip source paths shown above.\n\
        - Segment start/end are SOURCE seconds within THAT clip ([0, its duration]). Snap to transcript \
        segment boundaries; never start or end mid-sentence.\n\
        - Segments play in the order you list them. Minimum 0.5s each; at most 100 segments.\n\n\
        Return ONLY a JSON object of this shape:\n\
        {{\"title\": \"≤80 char title\", \"mode_detected\": \"takes\" or \"montage\", \"notes\": \"1–2 sentences on what you kept and why\", \"segments\": [ {{\"source\": \"<exact clip path>\", \"start\": 3.0, \"end\": 12.5}} ]}}\n\
        No markdown, no code fences, no commentary — the JSON object only.",
        count = clips.len(),
        mode_line = mode_line,
        target_line = target_line,
        brief = brief_line,
        clips = clip_block,
    );
    let system = "You are a precise video editor. You reason about timestamps carefully and output strict JSON only.";

    let mut attempt_prompt = base_prompt.clone();
    let mut last_err = String::new();
    for attempt in 0..2 {
        let request_body = if is_anthropic {
            json!({"model": model, "max_tokens": 4000, "stream": false, "system": system,
                   "messages": [{"role": "user", "content": attempt_prompt}]})
        } else {
            json!({"model": model, "max_tokens": 4000, "stream": false,
                   "messages": [{"role": "system", "content": system}, {"role": "user", "content": attempt_prompt}]})
        };
        let resp = if is_anthropic {
            state.http.post(&endpoint)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&request_body)
                .send().await.map_err(|e| e.to_string())?
        } else {
            state.http.post(&endpoint).bearer_auth(&api_key).json(&request_body).send().await.map_err(|e| e.to_string())?
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Compilation planning failed ({status}): {body}"));
        }
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let text = if is_anthropic {
            body.get("content").and_then(Value::as_array)
                .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
                .and_then(|b| b.get("text")).and_then(Value::as_str).map(|s| s.to_string())
        } else {
            body.get("choices").and_then(Value::as_array).and_then(|arr| arr.first())
                .and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(Value::as_str).map(|s| s.to_string())
        }
        .unwrap_or_default();

        match parse_compilation_spec(&text)
            .and_then(|spec| crate::video::sanitize_compilation_spec(spec, &clip_durations, target_s))
        {
            Ok(spec) => return Ok(spec),
            Err(e) => {
                last_err = e;
                if attempt == 0 {
                    attempt_prompt = format!(
                        "{base_prompt}\n\nYour previous answer was rejected: {last_err}\nReturn corrected JSON only."
                    );
                }
            }
        }
    }
    Err(format!("Could not get a usable compilation from the model: {last_err}"))
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
    if enabled.contains("edit_file") {
        caps.push("Edit part of a file by exact-string replacement (edit_file)");
    }
    if enabled.contains("append_file") {
        caps.push("Append or prepend content to a file (append_file)");
    }
    if enabled.contains("capture_screen") {
        caps.push("Take a screenshot of the screen or frontmost window to see it (capture_screen)");
    }
    if enabled.contains("open_app") {
        caps.push("Launch/focus a macOS app, optionally opening a URL (open_app)");
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
    if enabled.contains("compose_draft") {
        caps.push("Draft a new email (compose_draft) — saved to Drafts unsent for the user to review/send.");
    }
    if enabled.contains("reply_draft") {
        caps.push("Draft a reply to a message (reply_draft) — saved to Drafts unsent.");
    }
    if enabled.contains("create_event") {
        caps.push("Add an event to Apple Calendar (create_event).");
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
    let any_mail_tool = ["list_inbox", "search_mail", "read_mail", "compose_draft", "reply_draft"]
        .iter()
        .any(|t| enabled.contains(*t));
    if any_mail_tool {
        prompt.push_str(
            "\n\n## Email (Apple Mail)\n\
You can read the user's email through Apple Mail on this Mac, across all \
configured accounts (iCloud, Gmail, work IMAP, etc.). Writing is DRAFT-ONLY: \
compose_draft and reply_draft save an unsent message to Drafts for the user \
to review and send — you can NEVER send, delete, mark read, or otherwise \
modify mail. After drafting, tell the user a draft is ready in Mail; do not \
imply it was sent.\n\n\
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
    let any_task_tool = ["create_task", "list_tasks", "update_task", "delete_task", "run_task"]
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
to get the id.\n\n\
run_task fires a task immediately, regardless of its schedule. A task with no \
schedule and no next_run is manual/chain-only: it never fires on its own — it \
runs only via run_task or when chained from another task's chain_next. A failed \
one-shot (errored run with no schedule to rejoin) gets status 'failed'; re-fire \
it with run_task after fixing the cause.\n\n\
Set `project_path` (an existing absolute folder) on a task to scope its run's \
file/shell tools to that project; without it the task runs in the default \
workspace root.",
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
    if enabled.contains("edit_file") { caps.push("Edit part of a file by exact-string replacement (edit_file)"); }
    if enabled.contains("append_file") { caps.push("Append or prepend content to a file (append_file)"); }
    if enabled.contains("capture_screen") { caps.push("Take a screenshot of the screen or frontmost window (capture_screen)"); }
    if enabled.contains("open_app") { caps.push("Launch/focus a macOS app, optionally opening a URL (open_app)"); }
    if enabled.contains("list_files") { caps.push("List directory contents (list_files)"); }
    if enabled.contains("search_files") { caps.push("Search for text in files (search_files)"); }
    if enabled.contains("web_search") { caps.push("Search the web (web_search)"); }
    if enabled.contains("web_fetch") { caps.push("Fetch web page content (web_fetch)"); }
    if enabled.contains("list_inbox") { caps.push("Read the user's Apple Mail inbox (list_inbox)"); }
    if enabled.contains("search_mail") { caps.push("Search mail by subject or sender (search_mail)"); }
    if enabled.contains("read_mail") { caps.push("Read a message by Message-ID (read_mail)"); }
    if enabled.contains("compose_draft") { caps.push("Draft a new email, unsent (compose_draft)"); }
    if enabled.contains("reply_draft") { caps.push("Draft a reply, unsent (reply_draft)"); }
    if enabled.contains("create_event") { caps.push("Add an Apple Calendar event (create_event)"); }
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
    let any_mail_tool = ["list_inbox", "search_mail", "read_mail", "compose_draft", "reply_draft"]
        .iter()
        .any(|t| enabled.contains(*t));
    if any_mail_tool {
        prompt.push_str(
            "\n\n## Email (Apple Mail)\n\
Read + draft-only access to the user's Apple Mail across all configured accounts. \
Reading: list_inbox for recent mail, search_mail for a person/topic (Subject+From \
only, not body), read_mail with the opaque handle from list/search (handles are \
session-local — re-list if Mail restarted). Writing is DRAFT-ONLY: compose_draft \
and reply_draft save unsent messages to Drafts for the user to review and send — \
you can never send, delete, or modify mail. Don't imply a draft was sent. First \
call may trigger a macOS Automation prompt.",
        );
    }

    // Keep this block in sync with build_system_prompt_with_context.
    let any_task_tool = ["create_task", "list_tasks", "update_task", "delete_task", "run_task"]
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
to get the id.\n\n\
run_task fires a task immediately, regardless of its schedule. A task with no \
schedule and no next_run is manual/chain-only: it never fires on its own — it \
runs only via run_task or when chained from another task's chain_next. A failed \
one-shot (errored run with no schedule to rejoin) gets status 'failed'; re-fire \
it with run_task after fixing the cause.\n\n\
Set `project_path` (an existing absolute folder) on a task to scope its run's \
file/shell tools to that project; without it the task runs in the default \
workspace root.",
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
            "Read a file within the workspace. Optionally pass start_line and/or end_line (1-based, inclusive) to read only a slice of a large file; when a range is given each line is prefixed with its line number.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "description": "First line to read (1-based, inclusive). Omit to start at the top."},
                    "end_line": {"type": "integer", "description": "Last line to read (1-based, inclusive). Omit to read to the end."}
                },
                "required": ["path"]
            }),
        ));
    }
    if enabled.contains("write_file") {
        defs.push((
            "write_file",
            "Write content to a file within workspace (creates or fully overwrites it). To change part of an existing file, prefer edit_file — it is far more token-efficient than rewriting the whole file.",
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
    if enabled.contains("edit_file") {
        defs.push((
            "edit_file",
            "Edit an existing file by replacing an exact substring. `old_string` must match the file content exactly (including whitespace/indentation) and, unless `replace_all` is true, must be unique in the file — include enough surrounding context to disambiguate. Use this instead of write_file for changes to part of a file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to find. Must be unique unless replace_all is true."},
                    "new_string": {"type": "string", "description": "Replacement text. Empty string deletes the matched text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring a unique match. Defaults to false."}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ));
    }
    if enabled.contains("append_file") {
        defs.push((
            "append_file",
            "Append `content` to the end of a file (creates it if missing). Ideal for building a large file section-by-section without re-sending the whole thing. Set `prepend` true to add to the start instead. Content is inserted verbatim — include any leading/trailing newlines you need.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "prepend": {"type": "boolean", "description": "Insert at the start of the file instead of the end. Defaults to false (append)."}
                },
                "required": ["path", "content"]
            }),
        ));
    }
    if enabled.contains("capture_screen") {
        defs.push((
            "capture_screen",
            "Take a screenshot so you can SEE the user's screen — use this to debug how an app or web page looks. `mode` is \"screen\" (whole display, default) or \"window\" (the frontmost window). To inspect a specific app or URL, call open_app first to bring it to the front, then capture_screen with mode \"window\". The screenshot is returned to you as an image you can read directly.",
            json!({
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "enum": ["screen", "window"], "description": "\"screen\" = whole display (default); \"window\" = the frontmost window."}
                }
            }),
        ));
    }
    if enabled.contains("open_app") {
        defs.push((
            "open_app",
            "Launch or focus a macOS app and bring it to the front, optionally opening a URL or file in it. Use the app's exact name (e.g. \"Safari\", \"Google Chrome\", \"Preview\"). Pair with capture_screen mode \"window\" to look at what you opened.",
            json!({
                "type": "object",
                "properties": {
                    "app": {"type": "string", "description": "Exact app name, e.g. \"Safari\" or \"Google Chrome\"."},
                    "url": {"type": "string", "description": "Optional URL or file path to open in the app."}
                },
                "required": ["app"]
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
    if enabled.contains("probe_video") {
        defs.push((
            "probe_video",
            "Get a video file's metadata: duration, resolution, fps, codec, and whether it has audio. Use before analyzing or editing a video.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Video file path within the workspace"}
                },
                "required": ["path"]
            }),
        ));
    }
    if enabled.contains("analyze_video") {
        defs.push((
            "analyze_video",
            "Analyze a video for editing: metadata, detected silence ranges (dead air), detected filler-word/stutter ranges (\"um\", \"uh\", repeated words), suggested keep-segments (silences + fillers removed), and a timestamped speech transcript. Use the timestamps to pick precise cut points for edit_video.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Video file path within the workspace"},
                    "transcribe": {"type": "boolean", "description": "Include a timestamped transcript (default true; requires a Groq or OpenAI API key)"}
                },
                "required": ["path"]
            }),
        ));
    }
    if enabled.contains("edit_video") {
        defs.push((
            "edit_video",
            "Edit a video with ffmpeg — non-destructive, renders a new .mp4 into the Content library. Keep only chosen segments (cuts), splice in b-roll clips or still-image title/intro cards from other files (segments[].source), apply a color filter preset, overlay timed text, center-crop to an aspect ratio (with optional smart_reframe to keep faces in frame when going vertical), change speed, mute, or burn in word-timed captions/subtitles. Also supports audio polish: normalize_audio for publishable loudness, denoise for noisy recordings, and fade for a smooth fade-in/out. Cuts always get an automatic de-click micro-fade so concatenated segments never pop. Run analyze_video first to choose cut points. Set captions=true for social/silent-viewing cuts or when the user asks for subtitles (the video is auto-transcribed). To build a title/intro card image, render HTML→PNG first (the Studio 'renderSlideToPng' helper) and reference the PNG as an image insert.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Source (main) video path within the workspace"},
                    "segments": {
                        "type": "array",
                        "items": {"type": "object", "properties": {"start": {"type": "number"}, "end": {"type": "number"}, "source": {"type": "string", "description": "Optional. Omit for the MAIN video. A workspace path to another VIDEO file makes this an inserted b-roll clip (start..end trimmed from THAT file). A workspace path to a still IMAGE (.png/.jpg/.jpeg/.webp) makes this a silent title/intro card shown for end-start seconds (use start=0; duration must be 0.2–30s). Inserts are auto-scaled/cropped to the main video's frame."}}, "required": ["start", "end"]},
                        "description": "Ranges to keep/insert, in final playback order. For MAIN segments (no source) start/end are SOURCE-timeline seconds — pad speech boundaries ~0.2s. For inserts, start/end are on the inserted file's OWN timeline. Omit entirely to keep the whole main video."
                    },
                    "filter": {"type": "string", "enum": ["none", "cinematic", "warm", "cool", "vintage", "bw", "punchy"], "description": "Color filter preset"},
                    "texts": {
                        "type": "array",
                        "items": {"type": "object", "properties": {"text": {"type": "string"}, "start": {"type": "number"}, "end": {"type": "number"}, "position": {"type": "string", "enum": ["top", "center", "bottom"]}, "size": {"type": "integer"}}, "required": ["text", "start", "end"]},
                        "description": "Text overlays. Times are on the OUTPUT timeline (after cuts and speed change)."
                    },
                    "aspect": {"type": "string", "description": "Center-crop to an aspect like \"9:16\", \"1:1\", \"4:5\""},
                    "smart_reframe": {"type": "boolean", "description": "Keep faces in frame when converting to a narrower/vertical aspect (subject-aware crop instead of static center-crop). Only meaningful with an aspect that narrows the video (e.g. 16:9 → 9:16). Use for vertical/TikTok/Reels/Shorts cuts of talking-head footage."},
                    "speed": {"type": "number", "description": "Playback speed multiplier, 0.25–4.0"},
                    "mute": {"type": "boolean", "description": "Drop the audio track"},
                    "captions": {"type": "boolean", "description": "Burn in word-timed subtitles from the transcript (auto-transcribes). Use for social/silent-viewing cuts or when subtitles are requested."},
                    "caption_style": {"type": "string", "enum": ["boxed", "outline", "karaoke"], "description": "Caption look: boxed (default), outline, or karaoke word-by-word highlight. Only with captions=true."},
                    "normalize_audio": {"type": "boolean", "description": "Loudness-normalize the mix for publishable audio (EBU R128, -14 LUFS). Use when the audio should be upload-ready or sounds too quiet/loud."},
                    "denoise": {"type": "boolean", "description": "Reduce background hiss/hum/fan noise from the recording. Use for noisy audio or a bad-sounding mic."},
                    "fade": {"type": "number", "description": "Overall fade-in/fade-out duration in seconds (0.1-5.0), applied to both video and audio at the clip's start and end."},
                    "output_name": {"type": "string", "description": "Short kebab-case stem for the output file"}
                },
                "required": ["path"]
            }),
        ));
    }
    if enabled.contains("generate_shorts") {
        defs.push((
            "generate_shorts",
            "Auto-clip a long video into short vertical clips: transcribes it, uses AI to pick the best self-contained highlight moments (strong hook + payoff), and renders each as a captioned 9:16 short (smart-reframed to keep faces in frame, loudness-normalized) into the Content library. Ideal for scheduled/background tasks that auto-clip new recordings into ready-to-post Reels/Shorts/TikToks — it never prompts, so it works in unattended runs. Returns the titles and output paths.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Video file path within the workspace"},
                    "count": {"type": "integer", "description": "How many shorts to produce (default 3, max 8)"},
                    "duration_s": {"type": "number", "description": "Target seconds per clip (default ~45; typically 30–60)"},
                    "brief": {"type": "string", "description": "Optional focus, e.g. 'focus on the pricing discussion' or 'pick the funniest moments'"}
                },
                "required": ["path"]
            }),
        ));
    }
    if enabled.contains("compile_videos") {
        defs.push((
            "compile_videos",
            "Combine several video clips (2–10) into ONE finished video, keeping only the good parts, and render it into the Content library. Transcribes and analyzes every clip, then uses AI to either (retakes/\"takes\" mode) align re-recorded takes into sections and keep the single best take per section — cleanest delivery, fewest fillers/stutters, no long pauses — assembled in narrative order, or (\"montage\" mode) pick the strongest self-contained moments across the clips and order them for flow. Pass mode=\"auto\" (default) to let it detect retakes vs montage. Great for scheduled/background tasks that assemble a polished cut from raw takes or daily clips — it never prompts, so it works unattended. Returns the title, detected mode, and output path.",
            json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "2–10 clip file paths within the workspace, in any order (the planner decides the final ordering)."
                    },
                    "brief": {"type": "string", "description": "Optional focus, e.g. 'keep the best explanation of pricing' or 'make an upbeat highlight reel'."},
                    "mode": {"type": "string", "enum": ["auto", "takes", "montage"], "description": "auto (detect retakes vs montage — default), takes (retakes of the same material — keep the best take per section), or montage (distinct moments woven together)."},
                    "target_duration_s": {"type": "number", "description": "Optional target total duration in seconds. Mostly useful for montage mode."}
                },
                "required": ["paths"]
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
    if enabled.contains("git_status") {
        defs.push((
            "git_status",
            "Show the git status of the workspace: current branch, the last commit, and the list of changed/staged/untracked files (porcelain XY codes). Use to see what has changed before diffing or committing. Read-only.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ));
    }
    if enabled.contains("git_diff") {
        defs.push((
            "git_diff",
            "Show a git diff of the workspace. By default shows unstaged working-tree changes. Set staged=true for the staged diff, or pass commit (a ref or range like 'HEAD~3' or 'abc..def') to diff against/between commits. Restrict to one file with path. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Optional file (or directory) to limit the diff to."},
                    "staged": {"type": "boolean", "description": "Diff staged changes (git diff --staged) instead of the working tree. Default false."},
                    "commit": {"type": "string", "description": "Optional ref or range, e.g. 'HEAD~3', 'main', or 'abc123..def456'. Validated against a conservative character set."}
                }
            }),
        ));
    }
    if enabled.contains("git_log") {
        defs.push((
            "git_log",
            "Show recent commit history (one line per commit, with ref decorations). Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "How many commits to show (default 10, max 100)."},
                    "path": {"type": "string", "description": "Optional file/directory to show history for."}
                }
            }),
        ));
    }
    if enabled.contains("git_commit") {
        defs.push((
            "git_commit",
            "Commit changes in the workspace with the given message. If paths are supplied, only those files are staged and committed; otherwise all tracked modified files are committed (untracked files are never added). Never pushes. Use git_status/git_diff first to review what you're committing.",
            json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "Commit message."},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Optional specific files to stage and commit. Omit to commit all tracked changes."}
                },
                "required": ["message"]
            }),
        ));
    }
    if enabled.contains("code_outline") {
        defs.push((
            "code_outline",
            "List the top-level declarations (functions, classes, structs, enums, traits, types, etc.) with their line numbers for a source file, or for every source file in a directory. Fast way to understand a file's structure or navigate a codebase before reading in detail. Supports Rust, JS/TS, Python, and Go.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "A source file or a directory to scan."}
                },
                "required": ["path"]
            }),
        ));
    }
    if enabled.contains("find_references") {
        defs.push((
            "find_references",
            "Find where a symbol (function, variable, type name) is used across the workspace, as whole-word matches with 2 lines of surrounding context. Use to trace how something is used before changing it. The symbol is matched literally (not a regex).",
            json!({
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "description": "The exact identifier to search for."},
                    "path": {"type": "string", "description": "Optional subdirectory to limit the search to."}
                },
                "required": ["symbol"]
            }),
        ));
    }
    if enabled.contains("http_request") {
        defs.push((
            "http_request",
            "Make an HTTP request to a public URL and return the status, content-type, and response body (truncated). Supports GET/POST/PUT/PATCH/DELETE/HEAD, custom headers, and a request body — use for testing/debugging APIs. Only http(s) to public hosts is allowed (localhost, private, and metadata addresses are blocked).",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The http(s) URL to request."},
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"], "description": "HTTP method (default GET)."},
                    "headers": {"type": "object", "description": "Optional request headers as a string→string map.", "additionalProperties": {"type": "string"}},
                    "body": {"type": "string", "description": "Optional request body (sent verbatim)."}
                },
                "required": ["url"]
            }),
        ));
    }
    if enabled.contains("run_tests") {
        defs.push((
            "run_tests",
            "Run the project's test suite and return the output and exit status. Auto-detects the project type in the target directory: Cargo.toml → cargo test, package.json → npm test, pyproject.toml/pytest.ini/setup.py → pytest. Optionally filter to a subset of tests. 5-minute timeout.",
            json!({
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "description": "Optional test name filter (passed to cargo test <filter>, npm test -- <filter>, or pytest -k <filter>)."},
                    "path": {"type": "string", "description": "Optional subdirectory containing the project to test. Defaults to the workspace root."}
                }
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
                    "next_run": {"type": "string", "description": "ISO timestamp for first run (YYYY-MM-DDTHH:MM:SS). Optional; computed from schedule if omitted."},
                    "chain_next": {"type": "array", "items": {"type": "integer"}, "description": "Optional task ids to run after this one finishes successfully. Each downstream task fires on the next scheduler tick with this task's full output prepended to its prompt. Build workflows by creating the downstream tasks first (no schedule), then referencing their ids here."},
                    "conditions": {"type": "string", "description": "Optional natural-language gate. When this task is fired by an upstream task in a chain, it runs only if this condition holds for the upstream output — e.g. 'only if there are unread emails'. Ignored for the task's own scheduled runs."},
                    "project_path": {"type": "string", "description": "Optional absolute folder path; the task run's file/shell tools are scoped to this folder (its workspace). Must be an existing directory. Omit to run in the default workspace root."},
                    "agent_id": {"type": "string", "description": "Optional custom agent (id or unique name, from list_agents) to run this task through — the run uses that agent's system prompt and tool allowlist instead of the default Muninn persona. Must exist and be enabled."}
                },
                "required": ["name", "description"]
            }),
        ));
    }
    if enabled.contains("list_tasks") {
        defs.push((
            "list_tasks",
            "List tasks with id, name, description, schedule, next_run, status, and last result. Use before updating, deleting, or running so you have the task id.",
            json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "description": "Optional filter: scheduled, running, completed, paused, or failed"}
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
                    "chain_next": {"type": "array", "items": {"type": "integer"}, "description": "Task ids to run after this one succeeds (replaces the existing chain). Pass an empty array to clear."},
                    "conditions": {"type": "string", "description": "Natural-language gate for chain-triggered runs (replaces existing). Pass an empty string to clear."},
                    "project_path": {"type": "string", "description": "Absolute folder path to scope the task run's file/shell tools to (must be an existing directory). Pass an empty string to clear (run in the default workspace root)."},
                    "agent_id": {"type": "string", "description": "Custom agent (id or unique name) to run this task through. Pass an empty string to clear (run as default Muninn)."},
                    "status": {"type": "string", "description": "scheduled, completed, or paused (running/failed are engine-managed and can't be set)"}
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
    if enabled.contains("run_task") {
        defs.push((
            "run_task",
            "Fire a task immediately, regardless of its schedule. The run starts in the background and this call returns right away — it does NOT wait for the run to finish. Use to trigger a manual/chain-only task (no schedule) or to re-fire a 'failed' one-shot. Errors if the task doesn't exist or is already running.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Task id (from list_tasks)"}
                },
                "required": ["id"]
            }),
        ));
    }
    if enabled.contains("list_agents") {
        defs.push((
            "list_agents",
            "List the custom agents: id, name, description, enabled, tool allowlist, and a system-prompt preview. Use before call_agent, update_agent, delete_agent, or targeting an agent from a task.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ));
    }
    if enabled.contains("create_agent") {
        defs.push((
            "create_agent",
            "Create a reusable custom agent: a named persona with its own system prompt and tool allowlist. Use for specialist roles you (or scheduled tasks) will invoke repeatedly — e.g. a code reviewer, a news summarizer. After creating, invoke it with call_agent or attach it to a task via agent_id.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Unique agent name, e.g. 'Code Reviewer'."},
                    "system_prompt": {"type": "string", "description": "The agent's role and working instructions. Written as a system prompt: who it is, how it should approach tasks, what its output should look like."},
                    "description": {"type": "string", "description": "One-line summary shown in the Agents panel."},
                    "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool ids this agent may use (validated against the tools registry). Omit or pass [] to allow all enabled tools. Prefer a minimal list — e.g. a summarizer needs web_search/web_fetch, not run_command."},
                    "max_iterations": {"type": "integer", "description": "Iteration budget hint (1-100, default 5)."},
                    "slash_command": {"type": "string", "description": "Optional slash command (without the /) to invoke it from chat."}
                },
                "required": ["name", "system_prompt"]
            }),
        ));
    }
    if enabled.contains("update_agent") {
        defs.push((
            "update_agent",
            "Edit an existing custom agent. Supply the agent (id or name) and any fields to change; omitted fields are left as-is. Set enabled: false to disable without deleting.",
            json!({
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "description": "Agent id or unique name (from list_agents)."},
                    "name": {"type": "string"},
                    "system_prompt": {"type": "string"},
                    "description": {"type": "string"},
                    "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Replaces the allowlist. Pass [] to allow all enabled tools."},
                    "max_iterations": {"type": "integer"},
                    "slash_command": {"type": "string"},
                    "enabled": {"type": "boolean"}
                },
                "required": ["agent"]
            }),
        ));
    }
    if enabled.contains("delete_agent") {
        defs.push((
            "delete_agent",
            "Delete a custom agent by id or name. Tasks still targeting it will fail until reassigned — the result warns you if any exist.",
            json!({
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "description": "Agent id or unique name (from list_agents)."}
                },
                "required": ["agent"]
            }),
        ));
    }
    if enabled.contains("call_agent") {
        defs.push((
            "call_agent",
            "Delegate a sub-task to a custom agent and get its final answer back as this tool's result. The agent runs autonomously with its own system prompt and tool allowlist in the current workspace: it cannot ask the user questions, dangerous shell commands are blocked, and it cannot delegate further. The call waits for the agent to finish — use it for focused sub-tasks ('review this diff', 'research X and summarize'), not open-ended work. The prompt must be self-contained: the agent sees none of this conversation.",
            json!({
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "description": "Agent id or unique name (from list_agents)."},
                    "prompt": {"type": "string", "description": "The task for the agent. Self-contained and specific — include any context it needs, since it cannot see this chat."}
                },
                "required": ["agent", "prompt"]
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
    if enabled.contains("compose_draft") {
        defs.push((
            "compose_draft",
            "Create an UNSENT draft email in Apple Mail. The draft is saved to Drafts for the user to review and send manually — nothing is sent automatically. Use when the user asks you to write/draft an email or when a task should prepare a message for their approval. Do NOT claim the email was sent.",
            json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "Recipient email address(es), comma- or semicolon-separated."},
                    "subject": {"type": "string", "description": "Subject line."},
                    "body": {"type": "string", "description": "Plain-text body. Newlines are preserved."},
                    "cc": {"type": "string", "description": "Optional CC address(es), comma- or semicolon-separated."}
                },
                "required": ["to", "body"]
            }),
        ));
    }
    if enabled.contains("reply_draft") {
        defs.push((
            "reply_draft",
            "Create an UNSENT draft reply to a specific message, addressed to its original sender with a 'Re:' subject, saved to Drafts. Pass the opaque `id` handle from list_inbox/search_mail. Nothing is sent — the user reviews and sends. Read the original with read_mail first if you need its content to write a relevant reply.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Opaque message handle from list_inbox or search_mail."},
                    "body": {"type": "string", "description": "Plain-text reply body. Newlines preserved."}
                },
                "required": ["id", "body"]
            }),
        ));
    }
    if enabled.contains("create_event") {
        defs.push((
            "create_event",
            "Add an event to the user's Apple Calendar. `start`/`end` are ISO datetimes in local time (YYYY-MM-DDTHH:MM:SS). Use when the user asks to schedule something, or in a task chain to add a detected meeting. The event is created directly (reversible — the user can delete it).",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Event title / summary."},
                    "start": {"type": "string", "description": "Start datetime, ISO local time (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)."},
                    "end": {"type": "string", "description": "End datetime, ISO local time. Must be after start."},
                    "calendar": {"type": "string", "description": "Optional calendar name. Defaults to the first calendar if omitted."},
                    "location": {"type": "string", "description": "Optional location."},
                    "notes": {"type": "string", "description": "Optional notes/description."}
                },
                "required": ["title", "start", "end"]
            }),
        ));
    }
    if enabled.contains("get_settings") {
        defs.push((
            "get_settings",
            "Read the app's agent-configurable settings: current values, allowed values, and which provider API keys are usable. Secrets are never included. Call this before set_setting to see valid keys and current state.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ));
    }
    if enabled.contains("set_setting") {
        defs.push((
            "set_setting",
            "Change one app setting (validated against a fixed schema — see get_settings for keys and allowed values). Safety-related settings prompt the user for approval first. API keys and MCP server config cannot be changed with this tool. Changes take effect immediately unless the result notes otherwise.",
            json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Setting key, e.g. 'llm/model' or 'tts/voice_id'. Must be one of the keys listed by get_settings."},
                    "value": {"type": "string", "description": "New value. Booleans accept true/false/1/0; pass an empty string to reset defaults where allowed."}
                },
                "required": ["key", "value"]
            }),
        ));
    }
    if enabled.contains("set_theme") {
        defs.push((
            "set_theme",
            "Switch the app's UI theme. Applies immediately and persists.",
            json!({
                "type": "object",
                "properties": {
                    "theme": {"type": "string", "enum": ["dark", "light", "terminal", "abyss", "slate", "ember", "paper", "ocean", "system"], "description": "'system' follows the OS light/dark preference."}
                },
                "required": ["theme"]
            }),
        ));
    }
    if enabled.contains("list_models") {
        defs.push((
            "list_models",
            "List the model ids a chat provider actually offers (queried live from its /models API). Use before changing llm/model so you set a real id. MiniMax has no models API — its known model is 'MiniMax-M3'.",
            json!({
                "type": "object",
                "properties": {
                    "provider": {"type": "string", "description": "Provider to query (e.g. 'openai', 'groq', 'anthropic'). Defaults to the currently configured llm/provider."}
                }
            }),
        ));
    }
    if enabled.contains("test_llm_connection") {
        defs.push((
            "test_llm_connection",
            "Send a minimal test request to verify a provider/model pair works with the configured API key. Use after changing llm/provider or llm/model, or to diagnose chat failures. Returns success or the provider's HTTP error.",
            json!({
                "type": "object",
                "properties": {
                    "provider": {"type": "string", "description": "Provider to test. Defaults to the current llm/provider."},
                    "model": {"type": "string", "description": "Model id to test. Defaults to the configured model for that provider."}
                }
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
#[derive(Clone, PartialEq)]
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

    /// Build a Provider for any configured provider name, routing through the shared
    /// `resolve_provider_wire` table. `minimax`/`anthropic` → Anthropic wire format with
    /// their respective endpoints; everything else → OpenAI chat-completions format with
    /// the provider's endpoint + key. This is what makes a non-default `llm/provider`
    /// setting actually take effect on the streaming chat/agent path.
    fn from_provider_name(state: &AppState, provider: &str) -> Result<Self, String> {
        let (is_anthropic, endpoint, api_key) = resolve_provider_wire(state, provider)?;
        if is_anthropic {
            Ok(Self::Anthropic { endpoint, api_key })
        } else {
            Ok(Self::OpenAi { endpoint, api_key })
        }
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

    fn build_request(&self, model: &str, system_prompt: &str, messages: &[Value], tools: &[Value], max_tokens: u64) -> Value {
        match self {
            Self::OpenAi { .. } => json!({
                "model": model,
                "messages": messages,
                "tools": tools,
                "tool_choice": "auto",
                "max_tokens": max_tokens,
                "stream": true
            }),
            Self::Anthropic { .. } => json!({
                "model": model,
                "max_tokens": max_tokens,
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

    /// Like `push_tool_result`, but also attaches captured images so a vision model can see
    /// them. For Anthropic the images ride as sibling blocks in the same user turn as the
    /// tool_result (a well-supported shape); OpenAI tool messages are text-only, so the
    /// images are dropped there with the text result kept.
    fn push_tool_result_with_images(
        &self,
        messages: &mut Vec<Value>,
        call_id: &str,
        output: &str,
        images: &[(String, String)],
    ) {
        match self {
            Self::OpenAi { .. } => {
                messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
            Self::Anthropic { .. } => {
                let mut content = vec![json!({"type": "tool_result", "tool_use_id": call_id, "content": output})];
                for (media_type, data) in images {
                    content.push(json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": media_type, "data": data}
                    }));
                }
                messages.push(json!({"role": "user", "content": content}));
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

/// Per-response output-token ceiling for the streaming chat/agent loop. This caps the
/// *entire* assistant turn — visible text plus any tool-call argument JSON — so a value
/// that's too small silently truncates large `write_file` / `run_command` payloads
/// mid-tool-use (the stream stops with `max_tokens` and the accumulated argument JSON is
/// left unterminated). Defaults to MiniMax 3.0's max output; override per-deployment with
/// the `llm/max_tokens` app setting.
fn resolve_max_output_tokens(state: &AppState) -> u64 {
    get_app_setting_value(state, "llm/max_tokens")
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(40960)
}

/// Rough token estimate (~4 chars/token) for the exact request being sent: the system
/// prompt plus the serialized message array, which includes every hidden tool_use /
/// tool_result round. Used to drive the context bar when the provider's stream doesn't
/// report `usage` (MiniMax's Anthropic-compatible endpoint often omits it) — far closer
/// to the truth than the frontend's fallback, which counts only visible chat bubbles.
fn estimate_request_tokens(system_prompt: &str, messages: &[Value]) -> u64 {
    let messages_chars: usize = messages.iter().map(|m| m.to_string().len()).sum();
    ((system_prompt.len() + messages_chars) / 4) as u64
}

/// Take and clear any images `capture_screen` stashed for this session during the current
/// tool call (returns (media_type, base64) pairs). See `AppState::captured_images`.
fn drain_captured_images(state: &AppState, session_id: &str) -> Vec<(String, String)> {
    state
        .captured_images
        .lock()
        .ok()
        .and_then(|mut m| m.remove(session_id))
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
/// Resolve the configured chat failover target (llm/fallback_provider +
/// llm/fallback_model, with the per-provider model memory as a model fallback).
/// Returns None when unconfigured or misconfigured — failover must never introduce a
/// new failure mode, so any resolution problem silently means "no failover".
fn resolve_chat_fallback(state: &AppState) -> Option<(Provider, String, String)> {
    let provider_name = get_app_setting_value(state, "llm/fallback_provider")
        .ok()
        .flatten()?
        .trim()
        .to_string();
    if provider_name.is_empty() {
        return None;
    }
    let model = get_app_setting_value(state, "llm/fallback_model")
        .ok()
        .flatten()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| {
            get_app_setting_value(state, &format!("llm/model/{}", provider_name))
                .ok()
                .flatten()
                .filter(|m| !m.trim().is_empty())
        })?;
    let provider = Provider::from_provider_name(state, &provider_name).ok()?;
    Some((provider, model.trim().to_string(), provider_name))
}

/// Wrap a primary Provider construction result: when construction itself fails
/// (typically a missing API key), fall back to the configured failover target so e.g. a
/// local model still answers when no cloud key is set. Returns the provider plus the
/// model to use with it — a provider swap swaps the model too.
fn provider_or_fallback(
    state: &AppState,
    primary: Result<Provider, String>,
    model: &str,
) -> Result<(Provider, String), String> {
    match primary {
        Ok(p) => Ok((p, model.to_string())),
        Err(err) => match resolve_chat_fallback(state) {
            Some((fb, fb_model, fb_name)) => {
                crate::db::add_feed_item(
                    state,
                    "system",
                    "LLM failover",
                    &format!(
                        "Primary provider unavailable ({}). Using fallback {} / {}.",
                        err, fb_name, fb_model
                    ),
                    "error",
                    "",
                );
                Ok((fb, fb_model))
            }
            None => Err(err),
        },
    }
}

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
    // Failover can swap these mid-turn, so the loop works on owned copies.
    let mut provider = provider.clone();
    let mut model = model.to_string();
    let mut tools = provider.tools_json(exec_tools, interactive);
    // Merge in tools from configured MCP servers (lazy-connects on first turn;
    // resilient — a broken server contributes nothing rather than failing the turn).
    tools.extend(provider.mcp_tools_json(&state.mcp.connected_tools(state).await));
    let mut messages = provider.initial_messages(system_prompt, history);
    let max_tokens = resolve_max_output_tokens(state);
    let mut tool_round: u32 = 0;
    let mut iteration: i64 = 0;
    let mut fallback_tried = false;

    loop {
        if check_stop_requested(state, session_id) {
            let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.to_string() });
            let msg = STOPPED_BY_USER_SENTINEL.to_string();
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

        let request_body = provider.build_request(&model, system_prompt, &messages, &tools, max_tokens);
        let response = match provider.send(&state.http, &request_body).await {
            Ok(resp) => resp,
            Err(err) => {
                // Provider failover (llm/fallback_provider): only on the turn's FIRST
                // request — nothing has streamed or executed yet, so rebuilding the
                // provider-shaped tools/messages is lossless. Later rounds carry
                // wire-format-specific message state and executed tool side effects, so
                // a mid-turn provider swap is deliberately not attempted. `send` fails
                // before any SSE bytes arrive, so no tokens were emitted to the UI.
                if tool_round > 0 || fallback_tried {
                    return Err(err);
                }
                let Some((fb_provider, fb_model, fb_name)) = resolve_chat_fallback(state)
                else {
                    return Err(err);
                };
                if fb_provider == provider && fb_model == model {
                    return Err(err);
                }
                crate::db::add_feed_item(
                    state,
                    "system",
                    "LLM failover",
                    &format!(
                        "Primary model '{}' failed ({}). Retrying with fallback {} / {}.",
                        model, err, fb_name, fb_model
                    ),
                    "error",
                    session_id,
                );
                fallback_tried = true;
                provider = fb_provider;
                model = fb_model;
                tools = provider.tools_json(exec_tools, interactive);
                tools.extend(provider.mcp_tools_json(&state.mcp.connected_tools(state).await));
                messages = provider.initial_messages(system_prompt, history);
                continue;
            }
        };
        let outcome = provider
            .parse_stream(response, |t| {
                let _ = app.emit("chat-token", ChatTokenEvent {
                    session_id: session_id.to_string(),
                    token: t.to_string(),
                });
            })
            .await?;

        // Update the context bar every round so it tracks the live conversation size
        // (system prompt + full tool history), not just the final turn. Prefer the
        // provider's reported usage; fall back to an estimate of the exact request we
        // just sent when the stream omits `usage` (e.g. MiniMax's Anthropic endpoint).
        {
            let input_tokens = if outcome.usage_input > 0 {
                outcome.usage_input
            } else {
                estimate_request_tokens(system_prompt, &messages)
            };
            let output_tokens = if outcome.usage_output > 0 {
                outcome.usage_output
            } else {
                (outcome.text.len() / 4) as u64
            };
            let _ = app.emit("chat-usage", ChatUsageEvent {
                session_id: session_id.to_string(),
                input_tokens,
                output_tokens,
            });
        }

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
                let output = match serde_json::from_str::<Value>(&call.args) {
                    Ok(args_json) => {
                        execute_and_log_tool_call(
                            app, state, session_id, exec_tools, &call.name, &args_json, &call.args, scope, interactive,
                        )
                        .await
                    }
                    // Unparseable argument JSON almost always means the response hit the
                    // output-token ceiling mid tool_use, leaving the accumulated JSON
                    // unterminated. Feed back an honest, actionable error instead of
                    // silently calling the tool with `{}` (which surfaces as a misleading
                    // "missing path"/"missing command" and sends the model chasing a
                    // phantom parameter bug).
                    Err(e) => format!(
                        "ERROR: tool '{}' arguments were not valid JSON ({}). They were most \
                         likely truncated because the response reached the {}-token output \
                         limit. Retry with shorter content — e.g. write the file in smaller \
                         chunks, or raise the limit via the llm/max_tokens setting.",
                        call.name, e, max_tokens
                    ),
                };
                // A tool (capture_screen) may have produced images to show the model. Attach
                // them as sibling image blocks in the tool_result turn so the vision model
                // sees them this turn; persist only a text placeholder so reloaded history
                // doesn't carry the (large) base64 forever.
                let images = drain_captured_images(state, session_id);
                if images.is_empty() {
                    provider.push_tool_result(&mut messages, &call.id, &output);
                    tool_result_blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": call.id,
                        "content": output,
                    }));
                } else {
                    provider.push_tool_result_with_images(&mut messages, &call.id, &output, &images);
                    tool_result_blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": call.id,
                        "content": format!("{} [{} screenshot(s) shown to the model]", output, images.len()),
                    }));
                }
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

        // chat-usage for this final round was already emitted above, right after the
        // response was parsed — no need to repeat it here.
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
    let (provider, model) = provider_or_fallback(state, Provider::openai(state), model)?;
    let enabled_tools = enabled_tool_ids(state)?;
    let system_prompt = resolve_chat_system_prompt(state, history, scope, interactive).await;
    run_tool_loop(
        app, state, session_id, &provider, &model, &system_prompt,
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
    let (provider, model) =
        provider_or_fallback(state, Provider::minimax_anthropic(state), model)?;
    let enabled_tools = enabled_tool_ids(state)?;
    let system_prompt = resolve_chat_system_prompt(state, history, scope, interactive).await;
    run_tool_loop(
        app, state, session_id, &provider, &model, &system_prompt,
        history, &enabled_tools, scope, interactive, None,
    )
    .await
}

/// Chat via whatever provider `provider_name` resolves to (the configured `llm/provider`).
/// Used for any provider other than the MiniMax/OpenAI fast paths — routes through the
/// shared `resolve_provider_wire` table so groq/deepseek/anthropic/etc. actually work.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn chat_dynamic(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    provider_name: &str,
    model: &str,
    history: &[HistoryMessage],
    scope: Option<&WorkspaceScope>,
    interactive: bool,
) -> Result<(String, Vec<Value>), String> {
    let (provider, model) =
        provider_or_fallback(state, Provider::from_provider_name(state, provider_name), model)?;
    let enabled_tools = enabled_tool_ids(state)?;
    let system_prompt = resolve_chat_system_prompt(state, history, scope, interactive).await;
    run_tool_loop(
        app, state, session_id, &provider, &model, &system_prompt,
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
    scope: Option<&WorkspaceScope>,
    interactive: bool,
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
                "MiniMax-M3".to_string()
            } else {
                "gpt-4.1-mini".to_string()
            }
        });

    let primary = if provider_name == "minimax" || model.starts_with("MiniMax-") {
        Provider::minimax_anthropic(state)
    } else {
        Provider::from_provider_name(state, &provider_name)
    };
    let (provider, model) = provider_or_fallback(state, primary, &model)?;

    // Chat-invoked agents pass scope: None, interactive: true (the previous hardcoded
    // behavior). Scheduled task runs pass the task's workspace scope and
    // interactive: false so dangerous commands are hard-blocked with no user to ask.
    run_tool_loop(
        app, state, session_id, &provider, &model, &agent_system,
        history, &allowed_tools, scope, interactive, Some(&agent.name),
    )
    .await
}

/// The `/init` project analyzer: explores a project with read-only tools and returns the
/// generated AGENTS.md content. Runs through the shared `run_tool_loop` like chat/agents —
/// `session_id` is a throwaway id used only for streaming events (it has no `sessions` row,
/// so the loop's `insert_message` calls no-op under the foreign-key constraint).
pub(crate) async fn run_project_init(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    path: &str,
    scope: &WorkspaceScope,
) -> Result<String, String> {
    let system_prompt = format!(
        "You are Muninn, a project analyzer. You explore projects using tools and generate an AGENTS.md description file. \
        The project root is: {path}. All file paths are relative to this root. Be thorough but efficient — \
        explore the structure, read key files, then write a concise summary."
    );

    let prompt = format!(
        "You are analyzing the project at: {path}\n\n\
        Use the provided tools to explore the project thoroughly:\n\
        - Use list_files to understand the directory structure (check subdirectories too)\n\
        - Use read_file to read key files (README, config files, entry points, important source files)\n\
        - Use run_command to check git log, dependency info, etc.\n\
        - Use search_files to find patterns if needed\n\n\
        After exploring, generate a concise AGENTS.md file (under 100 lines, markdown) that describes:\n\
        1. What the project is (name, purpose)\n\
        2. Tech stack and key dependencies\n\
        3. Project structure overview\n\
        4. How to build/run/test\n\
        5. Key files and their purposes\n\n\
        Output the final AGENTS.md content as your last message, with no preamble like \"Here is the AGENTS.md\"."
    );

    // Read-only exploration tools only.
    let init_tools: HashSet<String> = ["read_file", "list_files", "search_files", "run_command"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let provider_name =
        get_app_setting_value(state, "llm/provider")?.unwrap_or_else(|| "minimax".to_string());
    let model = get_app_setting_value(state, "llm/model")?
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| {
            if provider_name == "minimax" {
                "MiniMax-M3".to_string()
            } else {
                "gpt-4.1-mini".to_string()
            }
        });

    let primary = if provider_name == "minimax" || model.starts_with("MiniMax-") {
        Provider::minimax_anthropic(state)
    } else {
        Provider::from_provider_name(state, &provider_name)
    };
    let (provider, model) = provider_or_fallback(state, primary, &model)?;

    let history = vec![HistoryMessage::text("user", &prompt)];

    let (text, _blocks) = run_tool_loop(
        app, state, session_id, &provider, &model, &system_prompt,
        &history, &init_tools, Some(scope), false, None,
    )
    .await?;

    Ok(text)
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
    fn condition_verdict_is_affirmative_only_and_fails_closed() {
        assert!(parse_condition_verdict("YES\nThere are 3 unread emails.").0);
        assert!(parse_condition_verdict("yes").0);
        assert!(!parse_condition_verdict("NO\nInbox is empty.").0);
        // Anything non-affirmative or empty must not fire.
        assert!(!parse_condition_verdict("").0);
        assert!(!parse_condition_verdict("I'm not sure").0);
        assert!(!parse_condition_verdict("Maybe, it depends").0);
        // Reason is the last non-empty line.
        assert_eq!(parse_condition_verdict("YES\nfound 2 items").1, "found 2 items");
        // Single-line replies use that line as the reason; blank input uses the fallback.
        assert_eq!(parse_condition_verdict("NO").1, "NO");
        assert_eq!(parse_condition_verdict("").1, "condition not met");
    }

    #[test]
    fn command_verdict_parses_each_first_line_and_note() {
        assert_eq!(
            parse_command_verdict("ALLOW\nThe path is inside the workspace build dir."),
            CommandVerdict::Allow("The path is inside the workspace build dir.".to_string())
        );
        assert_eq!(
            parse_command_verdict("DENY\nDeletes the user's home directory."),
            CommandVerdict::Deny("Deletes the user's home directory.".to_string())
        );
        assert_eq!(
            parse_command_verdict("ESCALATE\nThis is the user's call."),
            CommandVerdict::Escalate("This is the user's call.".to_string())
        );
        // Case-insensitive first token.
        assert!(matches!(parse_command_verdict("allow\nfine"), CommandVerdict::Allow(_)));
    }

    #[test]
    fn command_verdict_handles_inline_note_and_blank() {
        // Reason on the same line as the verdict word.
        assert_eq!(
            parse_command_verdict("DENY: removes system files"),
            CommandVerdict::Deny("removes system files".to_string())
        );
        // Bare verdict with no note gets a sensible default.
        assert_eq!(
            parse_command_verdict("ALLOW"),
            CommandVerdict::Allow("verifier allowed the command".to_string())
        );
    }

    #[test]
    fn command_verdict_fails_closed_on_garbage() {
        // Unrecognized first line → Escalate (fail closed).
        assert!(matches!(parse_command_verdict("maybe, it depends"), CommandVerdict::Escalate(_)));
        assert!(matches!(parse_command_verdict(""), CommandVerdict::Escalate(_)));
        assert!(matches!(parse_command_verdict("I think this is fine"), CommandVerdict::Escalate(_)));
    }

    #[test]
    fn session_title_sanitizer_strips_quotes_punctuation_and_caps_length() {
        assert_eq!(sanitize_session_title("\"Confession Times Lookup.\""), "Confession Times Lookup");
        assert_eq!(sanitize_session_title("  Fix   login   bug!  "), "Fix login bug");
        assert_eq!(sanitize_session_title("Title: here\nsecond line ignored"), "Title: here");
        assert_eq!(sanitize_session_title("\n\n  \n"), "");
        let long = "word ".repeat(30);
        assert!(sanitize_session_title(&long).chars().count() <= 60);
    }

    #[test]
    fn command_verdict_requires_exact_verdict_word_not_prefix() {
        // A verdict word as a mere prefix of a longer word must fail closed, not match.
        assert!(matches!(
            parse_command_verdict("ALLOWING this would be harmful"),
            CommandVerdict::Escalate(_)
        ));
        assert!(matches!(
            parse_command_verdict("DENYING it outright seems wrong"),
            CommandVerdict::Escalate(_)
        ));
        // Trailing punctuation on the exact word is still accepted.
        assert!(matches!(parse_command_verdict("ALLOW."), CommandVerdict::Allow(_)));
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
