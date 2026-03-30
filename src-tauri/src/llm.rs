use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Emitter};

use crate::AppState;
use crate::db::{first_non_empty_setting, get_app_setting_value, with_db};
use crate::models::*;
use crate::tools::{
    enabled_tool_ids, execute_agent_tool, log_tool, parse_tool_code_block,
    truncate_chars, WorkspaceScope,
};

/// Check if a stop has been requested for this session (and clear the flag).
fn check_stop_requested(state: &AppState, session_id: &str) -> bool {
    if let Ok(mut stops) = state.stop_requests.lock() {
        stops.remove(session_id)
    } else {
        false
    }
}

// --- Embeddings & Semantic Retrieval ---

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 { 0.0 } else { dot / (mag_a * mag_b) }
}

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

/// Retrieve top-k entity IDs most relevant to the query embedding.
pub(crate) fn retrieve_relevant_entity_ids(state: &AppState, query_embedding: &[f32], top_k: usize) -> Vec<(i64, f32)> {
    let rows: Vec<(i64, Vec<u8>)> = with_db(state, |db| {
        let mut stmt = db
            .prepare("SELECT source_id, embedding FROM embeddings WHERE source_type = 'entity'")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))
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

    let mut scored: Vec<(i64, f32)> = rows
        .iter()
        .map(|(id, blob)| {
            let emb = blob_to_f32_vec(blob);
            (*id, cosine_similarity(query_embedding, &emb))
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);
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

    let provider = get_app_setting_value(state, "llm/provider")
        .ok()
        .flatten()
        .unwrap_or_else(|| "minimax".to_string());

    let model = get_app_setting_value(state, "llm/model")
        .ok()
        .flatten()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| {
            if provider == "minimax" { "MiniMax-M2.7".to_string() } else { "gpt-4.1-mini".to_string() }
        });

    let is_anthropic = provider == "minimax" || provider == "anthropic" || model.starts_with("MiniMax-");

    let (api_key, endpoint) = if is_anthropic {
        let key = first_non_empty_setting(state, &["llm/minimax_api_key", "llm/anthropic_api_key"])
            .ok()
            .flatten()
            .or_else(|| std::env::var("MINIMAX_API_KEY").ok())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())?;
        let base = std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimax.io/anthropic".to_string());
        let ep = if base.ends_with("/v1/messages") { base } else { format!("{}/v1/messages", base.trim_end_matches('/')) };
        (key, ep)
    } else {
        let key = first_non_empty_setting(state, &["llm/openai_api_key"])
            .ok()
            .flatten()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())?;
        (key, "https://api.openai.com/v1/chat/completions".to_string())
    };

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
pub(crate) fn build_system_prompt(state: &AppState) -> String {
    build_system_prompt_with_context(state, None)
}

pub(crate) fn build_system_prompt_with_context(state: &AppState, relevant_entity_ids: Option<&[(i64, f32)]>) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let home = std::env::var("HOME").unwrap_or_default();
    let workspace = state
        .workspace_root
        .lock()
        .map(|w| w.display().to_string())
        .unwrap_or_default();

    let mut prompt = format!(
        "You are Spino, a helpful and intelligent AI assistant running locally on the user's Mac. \
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
        caps.push("Search for places and businesses — maps display automatically, ALWAYS use the tool for location queries instead of text-only answers (search_places)");
    }
    if enabled.contains("get_directions") {
        caps.push("Get directions between locations — maps display automatically, ALWAYS use the tool instead of text-only directions (get_directions)");
    }

    if !caps.is_empty() {
        prompt.push_str("\n\nAvailable tools:\n");
        for cap in &caps {
            prompt.push_str(&format!("- {}\n", cap));
        }
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
            .prepare("SELECT path, name, spino_md FROM projects ORDER BY last_opened_at DESC LIMIT 20")
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
        for (path, name, spino_md) in &projects {
            let display_name = if name.is_empty() { path.as_str() } else { name.as_str() };
            let preview: String = spino_md.lines().take(3).collect::<Vec<_>>().join(" ");
            if preview.is_empty() {
                prompt.push_str(&format!("\n- {} ({})", display_name, path));
            } else {
                prompt.push_str(&format!("\n- {} ({}): {}", display_name, path, preview));
            }
        }
    }

    prompt
}

pub(crate) fn build_project_system_prompt(state: &AppState, project_path: &str) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Read spino.md directly from the workspace directory
    let spino_md = if !project_path.is_empty() {
        let spino_path = std::path::Path::new(project_path).join("spino.md");
        std::fs::read_to_string(&spino_path).unwrap_or_default()
    } else {
        String::new()
    };

    let mut prompt = format!(
        "You are Spino, working within the project at {project_path}. \
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

    if !caps.is_empty() {
        prompt.push_str("\n\nAvailable tools:\n");
        for cap in &caps {
            prompt.push_str(&format!("- {}\n", cap));
        }
    }

    if !spino_md.is_empty() {
        prompt.push_str(&format!("\n\n## Project Description (spino.md)\n{}", spino_md));
    }

    prompt
}

pub(crate) fn openai_tools_json(enabled: &HashSet<String>) -> Vec<Value> {
    let mut tools = Vec::new();
    if enabled.contains("run_command") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a safe shell command in the local workspace and return text output.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "cwd": {"type": "string"}
                    },
                    "required": ["command"]
                }
            }
        }));
    }
    if enabled.contains("read_file") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file within workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        }));
    }
    if enabled.contains("write_file") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write content to a file within workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }
            }
        }));
    }
    if enabled.contains("list_files") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List files in a workspace directory.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        }));
    }
    if enabled.contains("search_files") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "search_files",
                "description": "Search text recursively using ripgrep.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "pattern": {"type": "string"}
                    },
                    "required": ["path", "pattern"]
                }
            }
        }));
    }
    if enabled.contains("web_search") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web using Brave Search API. Returns titles, URLs, and snippets.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"},
                        "count": {"type": "integer", "description": "Number of results (default 5, max 20)"}
                    },
                    "required": ["query"]
                }
            }
        }));
    }
    if enabled.contains("web_fetch") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch the text content of a URL. Returns the page body as plain text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "The URL to fetch"}
                    },
                    "required": ["url"]
                }
            }
        }));
    }
    if enabled.contains("generate_image") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "generate_image",
                "description": "Generate an image from a text description. Use when the user wants to create, draw, or visualize something.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "description": "Detailed description of the image to generate"},
                        "style": {"type": "string", "description": "Art style hint: realistic, artistic, cartoon, abstract"}
                    },
                    "required": ["prompt"]
                }
            }
        }));
    }
    if enabled.contains("search_places") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "search_places",
                "description": "Search for places, businesses, and points of interest. Results display on an interactive map.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "What to search for (e.g., 'coffee shops near downtown Seattle')"},
                        "limit": {"type": "integer", "description": "Max results (default 5)"}
                    },
                    "required": ["query"]
                }
            }
        }));
    }
    if enabled.contains("get_directions") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "get_directions",
                "description": "Get directions between two locations. Results display on an interactive map with the route.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from": {"type": "string", "description": "Starting address or place"},
                        "to": {"type": "string", "description": "Destination address or place"},
                        "mode": {"type": "string", "description": "Transport mode: driving, walking, cycling (default driving)"}
                    },
                    "required": ["from", "to"]
                }
            }
        }));
    }
    tools
}

pub(crate) fn anthropic_tools_json(enabled: &HashSet<String>) -> Vec<Value> {
    let mut tools = Vec::new();
    if enabled.contains("run_command") {
        tools.push(json!({
            "name": "run_command",
            "description": "Run a safe shell command in the local workspace and return text output.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string"}
                },
                "required": ["command"]
            }
        }));
    }
    if enabled.contains("read_file") {
        tools.push(json!({
            "name": "read_file",
            "description": "Read a file within workspace.",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }));
    }
    if enabled.contains("write_file") {
        tools.push(json!({
            "name": "write_file",
            "description": "Write content to a file within workspace.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }
        }));
    }
    if enabled.contains("list_files") {
        tools.push(json!({
            "name": "list_files",
            "description": "List files in a workspace directory.",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }));
    }
    if enabled.contains("search_files") {
        tools.push(json!({
            "name": "search_files",
            "description": "Search text recursively using ripgrep.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "pattern": {"type": "string"}
                },
                "required": ["path", "pattern"]
            }
        }));
    }
    if enabled.contains("web_search") {
        tools.push(json!({
            "name": "web_search",
            "description": "Search the web using Brave Search API. Returns titles, URLs, and snippets.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"},
                    "count": {"type": "integer", "description": "Number of results (default 5, max 20)"}
                },
                "required": ["query"]
            }
        }));
    }
    if enabled.contains("web_fetch") {
        tools.push(json!({
            "name": "web_fetch",
            "description": "Fetch the text content of a URL. Returns the page body as plain text.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"}
                },
                "required": ["url"]
            }
        }));
    }
    if enabled.contains("generate_image") {
        tools.push(json!({
            "name": "generate_image",
            "description": "Generate an image from a text description. Use when the user wants to create, draw, or visualize something.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Detailed description of the image to generate"},
                    "style": {"type": "string", "description": "Art style hint: realistic, artistic, cartoon, abstract"}
                },
                "required": ["prompt"]
            }
        }));
    }
    if enabled.contains("search_places") {
        tools.push(json!({
            "name": "search_places",
            "description": "Search for places, businesses, and points of interest. Results display on an interactive map.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to search for (e.g., 'coffee shops near downtown Seattle')"},
                    "limit": {"type": "integer", "description": "Max results (default 5)"}
                },
                "required": ["query"]
            }
        }));
    }
    if enabled.contains("get_directions") {
        tools.push(json!({
            "name": "get_directions",
            "description": "Get directions between two locations. Results display on an interactive map with the route.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "Starting address or place"},
                    "to": {"type": "string", "description": "Destination address or place"},
                    "mode": {"type": "string", "description": "Transport mode: driving, walking, cycling (default driving)"}
                },
                "required": ["from", "to"]
            }
        }));
    }
    tools
}

pub(crate) async fn chat_with_openai(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    model: &str,
    history: &[(String, String)],
    scope: Option<&WorkspaceScope>,
) -> Result<String, String> {
    let api_key = if let Some(value) = first_non_empty_setting(state, &["llm/openai_api_key"])? {
        value
    } else {
        std::env::var("OPENAI_API_KEY")
            .map_err(|_| "OPENAI API key missing (set OPENAI_API_KEY or save it in Settings).".to_string())?
    };
    let enabled_tools = enabled_tool_ids(state)?;
    let tools = openai_tools_json(&enabled_tools);

    // Retrieve relevant memory context for the user's latest message
    let system_prompt = if let Some(s) = scope {
        build_project_system_prompt(state, &s.root.display().to_string())
    } else {
        let user_msg = history.last().map(|(_, c)| c.as_str()).unwrap_or("");
        let relevant = retrieve_relevant_memory(state, user_msg).await;
        build_system_prompt_with_context(state, relevant.as_deref())
    };
    let mut messages: Vec<Value> = vec![json!({
        "role": "system",
        "content": system_prompt
    })];

    for (role, content) in history {
        messages.push(json!({ "role": role, "content": content }));
    }

    let mut tool_round: u32 = 0;

    loop {
        // Check if user requested stop
        if check_stop_requested(state, session_id) {
            let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.to_string() });
            return Ok("[Generation stopped by user]".to_string());
        }

        let request_body = json!({
            "model": model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "stream": true
        });

        let response = state
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .bearer_auth(&api_key)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        if !status.is_success() {
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            return Err(format!("OpenAI API error {}: {}", status, body));
        }

        let mut content_acc = String::new();
        let mut tool_calls_acc: HashMap<u64, (String, String, String)> = HashMap::new();
        let mut line_buf = String::new();
        let mut usage_input: u64 = 0;
        let mut usage_output: u64 = 0;

        let mut stream = response;
        while let Some(bytes) = stream.chunk().await.map_err(|e| e.to_string())? {
            let text = String::from_utf8_lossy(&bytes);
            line_buf.push_str(&text);

            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim().to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.is_empty() || line == "data: [DONE]" || !line.starts_with("data: ") {
                    continue;
                }
                let json_str = &line[6..];
                let chunk: Value = match serde_json::from_str(json_str) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // Extract usage from final chunk (OpenAI format)
                if let Some(usage) = chunk.get("usage") {
                    if let Some(v) = usage.get("prompt_tokens").and_then(Value::as_u64) { usage_input = v; }
                    if let Some(v) = usage.get("completion_tokens").and_then(Value::as_u64) { usage_output = v; }
                }

                let delta = match chunk
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
                        content_acc.push_str(t);
                        let _ = app.emit(
                            "chat-token",
                            ChatTokenEvent {
                                session_id: session_id.to_string(),
                                token: t.to_string(),
                            },
                        );
                    }
                }

                if let Some(tc_arr) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tc in tc_arr {
                        let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let entry = tool_calls_acc.entry(idx).or_insert_with(|| {
                            let id = tc
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            (id, name, String::new())
                        });
                        if let Some(args_chunk) = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                        {
                            entry.2.push_str(args_chunk);
                        }
                    }
                }
            }
        }

        if !tool_calls_acc.is_empty() {
            let mut sorted_calls: Vec<(u64, (String, String, String))> =
                tool_calls_acc.into_iter().collect();
            sorted_calls.sort_by_key(|(idx, _)| *idx);

            let tool_calls_json: Vec<Value> = sorted_calls
                .iter()
                .map(|(_, (id, name, args))| {
                    json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": args }
                    })
                })
                .collect();

            let mut assistant_msg = json!({ "role": "assistant" });
            if !content_acc.is_empty() {
                assistant_msg["content"] = json!(content_acc);
            } else {
                assistant_msg["content"] = Value::Null;
            }
            assistant_msg["tool_calls"] = json!(tool_calls_json);
            messages.push(assistant_msg);

            for (_, (call_id, tool_name, args_text)) in &sorted_calls {
                let _ = app.emit("tool-executed", json!({
                    "session_id": session_id,
                    "tool": tool_name,
                    "args": args_text,
                    "status": "running"
                }));
                let args_json: Value =
                    serde_json::from_str(args_text).unwrap_or_else(|_| json!({}));
                let result = execute_agent_tool(
                    app,
                    state,
                    session_id,
                    &enabled_tools,
                    tool_name,
                    &args_json,
                    scope,
                ).await;
                match result {
                    Ok(output) => {
                        log_tool(state, session_id, tool_name, args_text, &output, "ok");
                        let _ = app.emit("tool-executed", json!({
                            "session_id": session_id,
                            "tool": tool_name,
                            "args": args_text,
                            "status": "ok"
                        }));
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": output
                        }));
                    }
                    Err(error) => {
                        log_tool(state, session_id, tool_name, args_text, &error, "error");
                        let _ = app.emit("tool-executed", json!({
                            "session_id": session_id,
                            "tool": tool_name,
                            "args": args_text,
                            "status": "error"
                        }));
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": format!("Tool error: {}", error)
                        }));
                    }
                }
            }
            tool_round += 1;
            if tool_round % 5 == 0 {
                let _ = app.emit("chat-token", json!({
                    "session_id": session_id,
                    "token": format!("\n\n*[Progress: {} tool rounds completed so far. Type /stop to cancel.]*\n\n", tool_round)
                }));
            }
            continue;
        }

        let text_content = content_acc;
        if let Some((tool_name, args_str)) = parse_tool_code_block(&text_content) {
            let args_json: Value = serde_json::from_str(&args_str).unwrap_or_else(|_| json!({}));
            let args_text = args_str.to_string();

            let _ = app.emit("tool-executed", json!({
                "session_id": session_id,
                "tool": tool_name,
                "args": args_text,
                "status": "running"
            }));
            let result =
                execute_agent_tool(app, state, session_id, &enabled_tools, &tool_name, &args_json, scope).await;
            let output = match result {
                Ok(output) => {
                    log_tool(state, session_id, &tool_name, &args_text, &output, "ok");
                    let _ = app.emit("tool-executed", json!({
                        "session_id": session_id,
                        "tool": &tool_name,
                        "args": &args_text,
                        "status": "ok"
                    }));
                    output
                }
                Err(error) => {
                    log_tool(state, session_id, &tool_name, &args_text, &error, "error");
                    let _ = app.emit("tool-executed", json!({
                        "session_id": session_id,
                        "tool": &tool_name,
                        "args": &args_text,
                        "status": "error"
                    }));
                    format!("Tool error: {}", error)
                }
            };

            messages.push(json!({
                "role": "assistant",
                "content": text_content
            }));
            messages.push(json!({
                "role": "user",
                "content": format!("Tool result: {}", output)
            }));
            tool_round += 1;
            if tool_round % 5 == 0 {
                let _ = app.emit("chat-token", json!({
                    "session_id": session_id,
                    "token": format!("\n\n*[Progress: {} tool rounds completed so far. Type /stop to cancel.]*\n\n", tool_round)
                }));
            }
            continue;
        }

        if usage_input > 0 || usage_output > 0 {
            let _ = app.emit("chat-usage", ChatUsageEvent {
                session_id: session_id.to_string(),
                input_tokens: usage_input,
                output_tokens: usage_output,
            });
        }
        let _ = app.emit(
            "chat-done",
            ChatDoneEvent {
                session_id: session_id.to_string(),
            },
        );
        return Ok(text_content);
    }

}

pub(crate) async fn chat_with_minimax_anthropic(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    model: &str,
    history: &[(String, String)],
    scope: Option<&WorkspaceScope>,
) -> Result<String, String> {
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

    let mut messages: Vec<Value> = Vec::new();
    for (role, content) in history {
        messages.push(json!({
            "role": role,
            "content": [{"type": "text", "text": content}]
        }));
    }

    let enabled_tools = enabled_tool_ids(state)?;
    let tools = anthropic_tools_json(&enabled_tools);

    // Retrieve relevant memory context for the user's latest message
    let system_prompt = if let Some(s) = scope {
        build_project_system_prompt(state, &s.root.display().to_string())
    } else {
        let user_msg = history.last().map(|(_, c)| c.as_str()).unwrap_or("");
        let relevant = retrieve_relevant_memory(state, user_msg).await;
        build_system_prompt_with_context(state, relevant.as_deref())
    };

    let mut tool_round: u32 = 0;

    loop {
        // Check if user requested stop
        if check_stop_requested(state, session_id) {
            let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.to_string() });
            return Ok("[Generation stopped by user]".to_string());
        }

        let request_body = json!({
            "model": model,
            "max_tokens": 4096,
            "system": system_prompt,
            "messages": messages,
            "tools": tools,
            "stream": true
        });

        let response = state
            .http
            .post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        if !status.is_success() {
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            return Err(format!("MiniMax Anthropic API error {}: {}", status, body));
        }

        let mut content_acc = String::new();
        let mut tool_uses: Vec<(String, String, String)> = Vec::new();
        let mut current_event_type = String::new();
        let mut line_buf = String::new();
        let mut usage_input: u64 = 0;
        let mut usage_output: u64 = 0;

        let mut stream = response;
        while let Some(bytes) = stream.chunk().await.map_err(|e| e.to_string())? {
            let text = String::from_utf8_lossy(&bytes);
            line_buf.push_str(&text);

            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim().to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.starts_with("event: ") {
                    current_event_type = line[7..].to_string();
                    continue;
                }
                if line.is_empty() || !line.starts_with("data: ") {
                    continue;
                }
                let json_str = &line[6..];
                let data: Value = match serde_json::from_str(json_str) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match current_event_type.as_str() {
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
                                let id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                tool_uses.push((id, name, String::new()));
                            }
                        }
                    }
                    "content_block_delta" => {
                        if let Some(delta) = data.get("delta") {
                            let delta_type =
                                delta.get("type").and_then(Value::as_str).unwrap_or("");
                            if delta_type == "text_delta" {
                                if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                    if !t.is_empty() {
                                        content_acc.push_str(t);
                                        let _ = app.emit(
                                            "chat-token",
                                            ChatTokenEvent {
                                                session_id: session_id.to_string(),
                                                token: t.to_string(),
                                            },
                                        );
                                    }
                                }
                            } else if delta_type == "input_json_delta" {
                                if let Some(json_chunk) =
                                    delta.get("partial_json").and_then(Value::as_str)
                                {
                                    if let Some(last) = tool_uses.last_mut() {
                                        last.2.push_str(json_chunk);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if !tool_uses.is_empty() {
            let mut content_blocks: Vec<Value> = Vec::new();
            if !content_acc.is_empty() {
                content_blocks.push(json!({"type": "text", "text": content_acc}));
            }
            for (id, name, input_str) in &tool_uses {
                let input: Value =
                    serde_json::from_str(input_str).unwrap_or_else(|_| json!({}));
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input
                }));
            }

            messages.push(json!({
                "role": "assistant",
                "content": content_blocks
            }));

            for (tool_id, tool_name, input_str) in &tool_uses {
                let args: Value =
                    serde_json::from_str(input_str).unwrap_or_else(|_| json!({}));
                let args_text = args.to_string();

                let _ = app.emit("tool-executed", json!({
                    "session_id": session_id,
                    "tool": tool_name,
                    "args": &args_text,
                    "status": "running"
                }));
                let result =
                    execute_agent_tool(app, state, session_id, &enabled_tools, tool_name, &args, scope).await;
                let output = match result {
                    Ok(output) => {
                        log_tool(state, session_id, tool_name, &args_text, &output, "ok");
                        let _ = app.emit("tool-executed", json!({
                            "session_id": session_id,
                            "tool": tool_name,
                            "args": &args_text,
                            "status": "ok"
                        }));
                        output
                    }
                    Err(error) => {
                        log_tool(state, session_id, tool_name, &args_text, &error, "error");
                        let _ = app.emit("tool-executed", json!({
                            "session_id": session_id,
                            "tool": tool_name,
                            "args": &args_text,
                            "status": "error"
                        }));
                        format!("Tool error: {}", error)
                    }
                };

                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_id,
                        "content": output
                    }]
                }));
            }
            tool_round += 1;
            if tool_round % 5 == 0 {
                let _ = app.emit("chat-token", json!({
                    "session_id": session_id,
                    "token": format!("\n\n*[Progress: {} tool rounds completed so far. Type /stop to cancel.]*\n\n", tool_round)
                }));
            }
            continue;
        }

        let text_content = content_acc;
        if let Some((tool_name, args_str)) = parse_tool_code_block(&text_content) {
            let args_json: Value = serde_json::from_str(&args_str).unwrap_or_else(|_| json!({}));
            let args_text = args_str.to_string();

            let _ = app.emit("tool-executed", json!({
                "session_id": session_id,
                "tool": &tool_name,
                "args": &args_text,
                "status": "running"
            }));
            let result =
                execute_agent_tool(app, state, session_id, &enabled_tools, &tool_name, &args_json, scope).await;
            let output = match result {
                Ok(output) => {
                    log_tool(state, session_id, &tool_name, &args_text, &output, "ok");
                    let _ = app.emit("tool-executed", json!({
                        "session_id": session_id,
                        "tool": &tool_name,
                        "args": &args_text,
                        "status": "ok"
                    }));
                    output
                }
                Err(error) => {
                    log_tool(state, session_id, &tool_name, &args_text, &error, "error");
                    let _ = app.emit("tool-executed", json!({
                        "session_id": session_id,
                        "tool": &tool_name,
                        "args": &args_text,
                        "status": "error"
                    }));
                    format!("Tool error: {}", error)
                }
            };

            messages.push(json!({
                "role": "assistant",
                "content": text_content
            }));
            messages.push(json!({
                "role": "user",
                "content": format!("Tool result: {}", output)
            }));
            tool_round += 1;
            if tool_round % 5 == 0 {
                let _ = app.emit("chat-token", json!({
                    "session_id": session_id,
                    "token": format!("\n\n*[Progress: {} tool rounds completed so far. Type /stop to cancel.]*\n\n", tool_round)
                }));
            }
            continue;
        }

        if usage_input > 0 || usage_output > 0 {
            let _ = app.emit("chat-usage", ChatUsageEvent {
                session_id: session_id.to_string(),
                input_tokens: usage_input,
                output_tokens: usage_output,
            });
        }
        let _ = app.emit(
            "chat-done",
            ChatDoneEvent {
                session_id: session_id.to_string(),
            },
        );
        return Ok(text_content);
    }
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
    history: &[(String, String)],
) -> Result<String, String> {
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
    let configured_provider =
        get_app_setting_value(state, "llm/provider")?.unwrap_or_else(|| "minimax".to_string());
    let provider = configured_provider;
    let model = get_app_setting_value(state, "llm/model")?
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| {
            if provider == "minimax" {
                "MiniMax-M2.7".to_string()
            } else {
                "gpt-4.1-mini".to_string()
            }
        });

    let is_anthropic = provider == "minimax" || provider == "anthropic" || model.starts_with("MiniMax-");

    if is_anthropic {
        run_agent_anthropic(app, state, session_id, &agent, &agent_system, &allowed_tools, &model, history).await
    } else {
        run_agent_openai(app, state, session_id, &agent, &agent_system, &allowed_tools, &model, history).await
    }
}

async fn run_agent_openai(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    agent: &crate::models::AgentRecord,
    system_prompt: &str,
    allowed_tools: &HashSet<String>,
    model: &str,
    history: &[(String, String)],
) -> Result<String, String> {
    let api_key = if let Some(value) = first_non_empty_setting(state, &["llm/openai_api_key"])? {
        value
    } else {
        std::env::var("OPENAI_API_KEY")
            .map_err(|_| "OpenAI API key missing".to_string())?
    };
    let tools = openai_tools_json(allowed_tools);
    let mut messages: Vec<Value> = vec![json!({
        "role": "system",
        "content": system_prompt
    })];
    for (role, content) in history {
        messages.push(json!({ "role": role, "content": content }));
    }

    let mut iteration: i64 = 0;
    loop {
        iteration += 1;
        let _ = app.emit("agent-iteration", AgentIterationEvent {
            session_id: session_id.to_string(),
            agent_name: agent.name.clone(),
            iteration,
            max_iterations: 0,
        });

        let request_body = json!({
            "model": model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "stream": true
        });

        let response = state
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .bearer_auth(&api_key)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        if !status.is_success() {
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            return Err(format!("OpenAI API error {}: {}", status, body));
        }

        let mut content_acc = String::new();
        let mut tool_calls_acc: HashMap<u64, (String, String, String)> = HashMap::new();
        let mut line_buf = String::new();

        let mut stream = response;
        while let Some(bytes) = stream.chunk().await.map_err(|e| e.to_string())? {
            let text = String::from_utf8_lossy(&bytes);
            line_buf.push_str(&text);
            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim().to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();
                if line.is_empty() || line == "data: [DONE]" || !line.starts_with("data: ") {
                    continue;
                }
                let chunk: Value = match serde_json::from_str(&line[6..]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let delta = match chunk.get("choices").and_then(Value::as_array)
                    .and_then(|arr| arr.first()).and_then(|c| c.get("delta")) {
                    Some(d) => d,
                    None => continue,
                };
                if let Some(t) = delta.get("content").and_then(Value::as_str) {
                    if !t.is_empty() {
                        content_acc.push_str(t);
                        let _ = app.emit("chat-token", ChatTokenEvent {
                            session_id: session_id.to_string(),
                            token: t.to_string(),
                        });
                    }
                }
                if let Some(tc_arr) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tc in tc_arr {
                        let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let entry = tool_calls_acc.entry(idx).or_insert_with(|| {
                            let id = tc.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            let name = tc.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
                            (id, name, String::new())
                        });
                        if let Some(args_chunk) = tc.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str) {
                            entry.2.push_str(args_chunk);
                        }
                    }
                }
            }
        }

        if !tool_calls_acc.is_empty() {
            let mut sorted_calls: Vec<(u64, (String, String, String))> = tool_calls_acc.into_iter().collect();
            sorted_calls.sort_by_key(|(idx, _)| *idx);
            let tool_calls_json: Vec<Value> = sorted_calls.iter().map(|(_, (id, name, args))| {
                json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
            }).collect();

            let mut assistant_msg = json!({"role": "assistant"});
            if !content_acc.is_empty() {
                assistant_msg["content"] = json!(content_acc);
            } else {
                assistant_msg["content"] = Value::Null;
            }
            assistant_msg["tool_calls"] = json!(tool_calls_json);
            messages.push(assistant_msg);

            for (_, (call_id, tool_name, args_text)) in &sorted_calls {
                let args_json: Value = serde_json::from_str(args_text).unwrap_or_else(|_| json!({}));
                let result = execute_agent_tool(app, state, session_id, allowed_tools, tool_name, &args_json, None).await;
                match result {
                    Ok(output) => {
                        log_tool(state, session_id, tool_name, args_text, &output, "ok");
                        messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
                    }
                    Err(error) => {
                        log_tool(state, session_id, tool_name, args_text, &error, "error");
                        messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": format!("Tool error: {}", error)}));
                    }
                }
            }
            continue;
        }

        // No tool calls — we have a final text response
        let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.to_string() });
        return Ok(content_acc);
    }
}

async fn run_agent_anthropic(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    agent: &crate::models::AgentRecord,
    system_prompt: &str,
    allowed_tools: &HashSet<String>,
    model: &str,
    history: &[(String, String)],
) -> Result<String, String> {
    let api_key = if let Some(value) = first_non_empty_setting(state, &["llm/minimax_api_key", "llm/anthropic_api_key"])? {
        value
    } else {
        std::env::var("MINIMAX_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .map_err(|_| "MiniMax/Anthropic API key missing".to_string())?
    };

    let base = std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.minimax.io/anthropic".to_string());
    let endpoint = if base.ends_with("/v1/messages") {
        base
    } else {
        format!("{}/v1/messages", base.trim_end_matches('/'))
    };

    let tools = anthropic_tools_json(allowed_tools);
    let mut messages: Vec<Value> = Vec::new();
    for (role, content) in history {
        messages.push(json!({"role": role, "content": [{"type": "text", "text": content}]}));
    }

    let mut iteration: i64 = 0;
    loop {
        iteration += 1;
        let _ = app.emit("agent-iteration", AgentIterationEvent {
            session_id: session_id.to_string(),
            agent_name: agent.name.clone(),
            iteration,
            max_iterations: 0,
        });

        let request_body = json!({
            "model": model,
            "max_tokens": 4096,
            "system": system_prompt,
            "messages": messages,
            "tools": tools,
            "stream": true
        });

        let response = state
            .http
            .post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        if !status.is_success() {
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            return Err(format!("API error {}: {}", status, body));
        }

        let mut content_acc = String::new();
        let mut tool_uses: Vec<(String, String, String)> = Vec::new();
        let mut current_event_type = String::new();
        let mut line_buf = String::new();

        let mut stream = response;
        while let Some(bytes) = stream.chunk().await.map_err(|e| e.to_string())? {
            let text = String::from_utf8_lossy(&bytes);
            line_buf.push_str(&text);
            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim().to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();
                if line.starts_with("event: ") {
                    current_event_type = line[7..].to_string();
                    continue;
                }
                if line.is_empty() || !line.starts_with("data: ") { continue; }
                let data: Value = match serde_json::from_str(&line[6..]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match current_event_type.as_str() {
                    "content_block_start" => {
                        if let Some(block) = data.get("content_block") {
                            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                                let id = block.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                                let name = block.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                                tool_uses.push((id, name, String::new()));
                            }
                        }
                    }
                    "content_block_delta" => {
                        if let Some(delta) = data.get("delta") {
                            let dt = delta.get("type").and_then(Value::as_str).unwrap_or("");
                            if dt == "text_delta" {
                                if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                    if !t.is_empty() {
                                        content_acc.push_str(t);
                                        let _ = app.emit("chat-token", ChatTokenEvent {
                                            session_id: session_id.to_string(),
                                            token: t.to_string(),
                                        });
                                    }
                                }
                            } else if dt == "input_json_delta" {
                                if let Some(json_chunk) = delta.get("partial_json").and_then(Value::as_str) {
                                    if let Some(last) = tool_uses.last_mut() {
                                        last.2.push_str(json_chunk);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if !tool_uses.is_empty() {
            let mut content_blocks: Vec<Value> = Vec::new();
            if !content_acc.is_empty() {
                content_blocks.push(json!({"type": "text", "text": content_acc}));
            }
            for (id, name, input_str) in &tool_uses {
                let input: Value = serde_json::from_str(input_str).unwrap_or_else(|_| json!({}));
                content_blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
            }
            messages.push(json!({"role": "assistant", "content": content_blocks}));

            for (tool_id, tool_name, input_str) in &tool_uses {
                let args: Value = serde_json::from_str(input_str).unwrap_or_else(|_| json!({}));
                let args_text = args.to_string();
                let result = execute_agent_tool(app, state, session_id, allowed_tools, tool_name, &args, None).await;
                let output = match result {
                    Ok(output) => { log_tool(state, session_id, tool_name, &args_text, &output, "ok"); output }
                    Err(error) => { log_tool(state, session_id, tool_name, &args_text, &error, "error"); format!("Tool error: {}", error) }
                };
                messages.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": output}]}));
            }
            continue;
        }

        let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.to_string() });
        return Ok(content_acc);
    }
}

pub(crate) async fn run_extraction(app: &AppHandle, state: &AppState, user_msg: &str, assistant_msg: &str) {
    if user_msg.len() + assistant_msg.len() < 50 {
        return;
    }

    let provider = match get_app_setting_value(state, "llm/provider") {
        Ok(Some(p)) if !p.trim().is_empty() => p,
        _ => "minimax".to_string(),
    };

    let model = match get_app_setting_value(state, "llm/model") {
        Ok(Some(m)) if !m.trim().is_empty() => m,
        _ => {
            if provider == "minimax" {
                "MiniMax-M2.7".to_string()
            } else {
                "gpt-4.1-mini".to_string()
            }
        }
    };

    let (api_key, endpoint, is_anthropic) = if provider == "minimax" {
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

    let api_key = match api_key {
        Some(k) if !k.is_empty() => k,
        _ => return,
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
