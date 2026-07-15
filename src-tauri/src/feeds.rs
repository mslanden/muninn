use rusqlite::params;
use serde_json::{json, Value};

use crate::db::*;
use crate::AppState;

/// Strip reasoning tags (<think>, <thinking>) from model output.
pub(crate) fn strip_think_tags(text: &str) -> String {
    let mut result = text.to_string();
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        loop {
            let start = result.find(open);
            let end = result.find(close);
            match (start, end) {
                (Some(s), Some(e)) if e > s => {
                    result = format!("{}{}", &result[..s], &result[e + close.len()..]);
                }
                (Some(s), None) => {
                    result = result[..s].to_string();
                    break;
                }
                _ => break,
            }
        }
    }
    result.trim().to_string()
}

/// Strip markdown code fences from model output, returning the inner/trimmed text.
fn strip_json_fences(text: &str) -> &str {
    if let Some(start) = text.find("```") {
        let inner = &text[start + 3..];
        let inner = if inner.starts_with("json") { &inner[4..] } else { inner };
        if let Some(end) = inner.find("```") {
            inner[..end].trim()
        } else {
            inner.trim()
        }
    } else {
        text.trim()
    }
}

/// Extract a JSON array from a string that may contain markdown fences or extra text.
pub(crate) fn extract_json_array(text: &str) -> Result<Vec<Value>, String> {
    let cleaned = strip_json_fences(text);
    if let Some(start) = cleaned.find('[') {
        if let Some(end) = cleaned.rfind(']') {
            if end > start {
                let json_str = &cleaned[start..=end];
                return serde_json::from_str(json_str)
                    .map_err(|e| format!("JSON parse error: {}", e));
            }
        }
    }
    Err("No JSON array found in model response".to_string())
}

/// Extract a JSON object from a string that may contain markdown fences or extra text.
pub(crate) fn extract_json_object(text: &str) -> Result<Value, String> {
    let cleaned = strip_json_fences(text);
    if let Some(start) = cleaned.find('{') {
        if let Some(end) = cleaned.rfind('}') {
            if end > start {
                let json_str = &cleaned[start..=end];
                return serde_json::from_str(json_str)
                    .map_err(|e| format!("JSON parse error: {}", e));
            }
        }
    }
    Err("No JSON object found in model response".to_string())
}

/// Resolve API key and endpoint for MiniMax Anthropic-compatible API.
fn resolve_minimax_config(state: &AppState) -> Result<(String, String), String> {
    let api_key = first_non_empty_setting(state, &["llm/minimax_api_key", "llm/anthropic_api_key"])
        .map_err(|e| e.to_string())?
        .or_else(|| std::env::var("MINIMAX_API_KEY").ok())
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
        .ok_or_else(|| "MiniMax API key not configured. Set it in Settings.".to_string())?;

    let base = std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.minimax.io/anthropic".to_string());
    let endpoint = if base.ends_with("/v1/messages") {
        base
    } else {
        format!("{}/v1/messages", base.trim_end_matches('/'))
    };

    Ok((api_key, endpoint))
}

/// Tool definitions for the feed agent (web_search and web_fetch only).
fn feed_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "web_search",
            "description": "Search the web using Brave Search. Returns titles, URLs, and snippets.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"},
                    "count": {"type": "integer", "description": "Number of results (default 5, max 20)"}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "web_fetch",
            "description": "Fetch a web page and return its text content (HTML stripped). Use this to read specific URLs, articles, or web pages.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"}
                },
                "required": ["url"]
            }
        }),
    ]
}

/// Execute a feed tool call, returning the result string.
async fn execute_feed_tool(state: &AppState, name: &str, input: &Value) -> String {
    match name {
        "web_search" => {
            let query = input["query"].as_str().unwrap_or("");
            let count = input["count"].as_u64().unwrap_or(5) as usize;
            match crate::tools::web_search_impl(state, query, count).await {
                Ok(r) => r,
                Err(e) => format!("Search error: {}", e),
            }
        }
        "web_fetch" => {
            let url = input["url"].as_str().unwrap_or("");
            match crate::tools::web_fetch_impl(state, url).await {
                Ok(r) => r,
                Err(e) => format!("Fetch error: {}", e),
            }
        }
        _ => format!("Unknown tool: {}", name),
    }
}

/// Parse a streaming SSE response from the Anthropic-compatible API.
/// Returns (text_content, tool_uses) where tool_uses is Vec<(id, name, input_json_string)>.
async fn parse_streaming_response(
    response: reqwest::Response,
) -> Result<(String, Vec<(String, String, String)>), String> {
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
            if line.is_empty() || !line.starts_with("data: ") {
                continue;
            }
            let json_str = &line[6..];
            let data: Value = match serde_json::from_str(json_str) {
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
                        let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
                        if delta_type == "text_delta" {
                            if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                content_acc.push_str(t);
                            }
                        } else if delta_type == "input_json_delta" {
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

    let text = strip_think_tags(&content_acc);
    Ok((text, tool_uses))
}

/// Load the last N feed items for context.
fn load_recent_items(state: &AppState, feed_id: i64, limit: usize) -> Result<Vec<Value>, String> {
    with_db(state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT title, summary, url, source_name FROM ai_feed_items \
                 WHERE feed_id = ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![feed_id, limit], |row| {
                Ok(json!({
                    "title": row.get::<_, String>(0)?,
                    "summary": row.get::<_, String>(1)?,
                    "url": row.get::<_, String>(2)?,
                    "source_name": row.get::<_, String>(3)?,
                }))
            })
            .map_err(|e| e.to_string())?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(|e| e.to_string())?);
        }
        Ok(items)
    })
}

const MAX_TOOL_ROUNDS: u32 = 5;

/// Run an agentic web-research loop (streaming) against the MiniMax Anthropic-compatible
/// API. The model is given the `web_search` + `web_fetch` tools and loops up to
/// `max_rounds`, executing tool calls in-Rust, until it returns a final text answer.
/// Returns the final assistant text (the caller parses whatever format the system prompt
/// requested). Shared by feed refresh and AI web search.
pub(crate) async fn run_agentic_loop(
    state: &AppState,
    system: &str,
    user_prompt: &str,
    max_rounds: u32,
) -> Result<String, String> {
    let (api_key, endpoint) = resolve_minimax_config(state)?;
    let tools = feed_tools();

    let mut messages: Vec<Value> = vec![json!({
        "role": "user",
        "content": [{"type": "text", "text": user_prompt}]
    })];

    let mut final_text = String::new();

    for _round in 0..max_rounds {
        let request_body = json!({
            "model": "MiniMax-M3",
            "max_tokens": 4096,
            "system": system,
            "messages": messages,
            "tools": tools,
            "stream": true,
        });

        let response = state
            .http
            .post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| format!("MiniMax request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            return Err(format!("MiniMax API error {}: {}", status, body));
        }

        let (text, tool_uses) = parse_streaming_response(response).await?;

        if tool_uses.is_empty() {
            // No tool calls — this is the final response
            final_text = text;
            break;
        }

        // Build the assistant content blocks to echo back
        let mut content_blocks: Vec<Value> = Vec::new();
        if !text.is_empty() {
            content_blocks.push(json!({"type": "text", "text": text}));
        }
        for (id, name, input_str) in &tool_uses {
            let input: Value = serde_json::from_str(input_str).unwrap_or_else(|_| json!({}));
            content_blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }

        messages.push(json!({
            "role": "assistant",
            "content": content_blocks,
        }));

        // Execute each tool and add results
        let mut tool_results: Vec<Value> = Vec::new();
        for (tool_id, tool_name, input_str) in &tool_uses {
            let input: Value = serde_json::from_str(input_str).unwrap_or_else(|_| json!({}));
            let result = execute_feed_tool(state, tool_name, &input).await;
            tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_id,
                "content": result,
            }));
        }

        messages.push(json!({
            "role": "user",
            "content": tool_results,
        }));
    }

    // If we exhausted rounds without final text, do one last call without tools
    if final_text.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "You've done enough research. Now output ONLY your final answer in the exact format specified in the system prompt, with no other text."}]
        }));

        let request_body = json!({
            "model": "MiniMax-M3",
            "max_tokens": 4096,
            "system": system,
            "messages": messages,
            "stream": true,
        });

        let response = state
            .http
            .post(&endpoint)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| format!("MiniMax request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            return Err(format!("MiniMax API error {}: {}", status, body));
        }

        let (text, _) = parse_streaming_response(response).await?;
        final_text = text;
    }

    Ok(final_text)
}

/// Refresh a feed using an agentic loop with streaming: the LLM decides which tools
/// to call (web_search, web_fetch) based on the user's sources, then curates feed items.
/// Returns the number of items added.
pub async fn refresh_feed_impl(state: &AppState, feed_id: i64, app: &tauri::AppHandle) -> Result<usize, String> {
    // 1. Load feed config
    let (name, sources_json) = with_db(state, |db| {
        db.query_row(
            "SELECT name, topics_json FROM ai_feeds WHERE id = ?1",
            params![feed_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| e.to_string())
    })?;

    let sources: Vec<String> = serde_json::from_str(&sources_json).unwrap_or_default();
    if sources.is_empty() {
        return Err("No sources configured for this feed.".to_string());
    }

    // 2. Load recent feed items for context
    let recent_items = load_recent_items(state, feed_id, 20)?;
    let history_section = if recent_items.is_empty() {
        "No previous items — this is a fresh feed.".to_string()
    } else {
        let summaries: Vec<String> = recent_items
            .iter()
            .filter_map(|item| {
                let title = item["title"].as_str()?;
                let url = item["url"].as_str().unwrap_or("");
                Some(format!("- {} ({})", title, url))
            })
            .collect();
        format!(
            "Previous items already in the feed (avoid duplicates, build on these):\n{}",
            summaries.join("\n")
        )
    };

    // 3. Build the system prompt
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let system = format!(
        r#"You are an intelligent feed curator for a personal feed called "{}".
Today's date is {}.

You have access to two tools:
- web_search: Search the web for information
- web_fetch: Fetch and read a specific web page

The user has configured these sources (they can be topics, URLs, or anything):
{}

Your job:
1. Look at each source and decide the best approach:
   - If it looks like a URL, use web_fetch to read it directly
   - If it looks like a topic or keyword, use web_search to find relevant articles
   - If it's something else, use your best judgment to find relevant content
2. Gather enough information to curate high-quality feed items
3. You may do multiple rounds of searching and fetching to get detailed information

{}

When you have gathered enough information, output your final answer as ONLY a JSON array:
[
  {{
    "title": "A short, compelling headline (max 80 characters)",
    "summary": "A 2-3 sentence summary of the key point (max 220 characters)",
    "url": "The source URL",
    "source_name": "The website or publication name"
  }}
]

Rules:
- Create 4-8 high-quality, non-duplicate feed items
- Only use real URLs from your search results or fetched pages — never invent URLs
- Avoid duplicating items already in the feed history above
- Keep titles under 80 characters, summaries under 220 characters
- When ready to give your final answer, output ONLY the JSON array with no other text"#,
        name,
        today,
        sources
            .iter()
            .map(|s| format!("- {}", s))
            .collect::<Vec<_>>()
            .join("\n"),
        history_section,
    );

    // 4. Run the agentic research loop
    let final_text = run_agentic_loop(
        state,
        &system,
        "Please refresh this feed now. Investigate each source and curate new items.",
        MAX_TOOL_ROUNDS,
    )
    .await?;

    // 5. Parse the JSON array from final text
    let items = extract_json_array(&final_text).map_err(|e| {
        format!(
            "Could not parse AI response as JSON: {}. Raw: {}",
            e,
            &final_text[..final_text.len().min(300)]
        )
    })?;

    // 7. Store items in the database, then fetch images
    let mut count = 0usize;
    let mut inserted_items: Vec<(i64, String, String)> = Vec::new(); // (row_id, url, title)
    for item in &items {
        let title = item["title"].as_str().unwrap_or("").trim().to_string();
        let summary = item["summary"].as_str().unwrap_or("").trim().to_string();
        let url = item["url"].as_str().unwrap_or("").trim().to_string();
        let source_name = item["source_name"].as_str().unwrap_or("").trim().to_string();

        if title.is_empty() {
            continue;
        }

        let row_id = with_db(state, |db| {
            db.execute(
                r#"INSERT INTO ai_feed_items (feed_id, title, summary, url, source_name, is_read, image_url)
                   VALUES (?1, ?2, ?3, ?4, ?5, 0, '')"#,
                params![feed_id, title, summary, url, source_name],
            )
            .map_err(|e| e.to_string())?;
            Ok(db.last_insert_rowid())
        })?;
        inserted_items.push((row_id, url.clone(), title.clone()));
        count += 1;
    }

    // 7b. Fetch images for inserted items (OG image first, then AI fallback)
    // Download all images locally to avoid ATS/CSP issues with remote URLs.
    for (row_id, url, title) in &inserted_items {
        let mut local_path: Option<String> = None;

        // Try OG image extraction from source URL
        if let Some(og_url) = crate::tools::extract_og_image(state, url).await {
            // Download the OG image locally
            local_path = crate::tools::download_feed_image(state, app, &og_url).await.ok();
        }

        // Fallback: generate a thumbnail with MiniMax (already saves locally)
        if local_path.is_none() {
            local_path = crate::tools::generate_feed_thumbnail(state, title, app).await.ok();
        }

        if let Some(path) = local_path {
            let _ = with_db(state, |db| {
                db.execute(
                    "UPDATE ai_feed_items SET image_url = ?1 WHERE id = ?2",
                    params![path, row_id],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            });
        }
    }

    // 8. Update last_refreshed_at
    with_db(state, |db| {
        db.execute(
            "UPDATE ai_feeds SET last_refreshed_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![feed_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences_handles_plain_json_and_json_fence() {
        assert_eq!(strip_json_fences("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_json_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_json_fences("```\n[1,2]\n```"), "[1,2]");
        // Unterminated fence: take everything after the opening fence.
        assert_eq!(strip_json_fences("```json\n{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn extract_json_array_from_noisy_model_output() {
        let raw = "Sure! Here are the items:\n```json\n[{\"title\":\"x\"},{\"title\":\"y\"}]\n```\nHope that helps.";
        let arr = extract_json_array(raw).expect("should parse array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["title"], "x");
    }

    #[test]
    fn extract_json_array_tolerates_surrounding_prose_without_fences() {
        let raw = "Result: [1, 2, 3] done";
        let arr = extract_json_array(raw).expect("should parse array");
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn extract_json_object_from_noisy_model_output() {
        let raw = "```json\n{\"title\":\"hi\",\"summary\":\"s\"}\n```";
        let obj = extract_json_object(raw).expect("should parse object");
        assert_eq!(obj["title"], "hi");
    }

    #[test]
    fn extract_json_array_errors_when_absent() {
        assert!(extract_json_array("no json here").is_err());
        // A bare object is not an array.
        assert!(extract_json_array("{\"a\":1}").is_err());
    }

    #[test]
    fn extract_json_object_errors_on_malformed() {
        assert!(extract_json_object("not json").is_err());
        assert!(extract_json_object("{ broken").is_err());
    }
}
