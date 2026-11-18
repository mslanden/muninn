use serde_json::{json, Value};

use crate::db::first_non_empty_setting;
use crate::models::{AiWebSearchResult, AiWebSource};
use crate::AppState;

const SEARCH_MAX_ROUNDS: u32 = 5;

/// Run an AI-powered web search: the model researches the query with web_search +
/// web_fetch, then returns a synthesized Markdown answer plus a list of cited sources.
pub async fn ai_web_search_impl(state: &AppState, query: &str) -> Result<AiWebSearchResult, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("Empty search query.".to_string());
    }

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let system = format!(
        r#"You are an AI web research assistant. Today's date is {today}.

You have access to two tools:
- web_search: Search the web (Brave). Returns titles, URLs, and snippets.
- web_fetch: Fetch and read a specific page's text.

For the user's query:
1. Run one or more web_search calls. If the query is a person, brand, product, or site, prioritize finding their official site and primary resources.
2. web_fetch the most promising 1-3 results to ground your answer in real content.
3. Synthesize a concise, well-structured answer that directly addresses the query.

When done, output ONLY a single JSON object (no prose, no code fences):
{{
  "answer": "Markdown synthesis. Use short paragraphs, headings, and bullet points where useful. Be specific and grounded in what you found.",
  "sources": [
    {{ "title": "Result title", "url": "https://real-url", "snippet": "1-2 sentences on why this is relevant" }}
  ]
}}

Rules:
- Only use real URLs returned by web_search or web_fetch — never invent URLs.
- Include 3-8 sources, most relevant first; put the official/primary source first when applicable.
- Do not fabricate facts. If something is unknown, say so.
- Your final message must be ONLY the JSON object."#
    );

    let user = format!("User query: {q}\n\nResearch this and produce the final JSON object.");

    let raw = run_agentic_loop(state, &system, &user, SEARCH_MAX_ROUNDS).await?;

    let obj = extract_json_object(&raw).map_err(|e| {
        format!(
            "Could not parse search response as JSON: {}. Raw: {}",
            e,
            &raw[..raw.len().min(300)]
        )
    })?;

    let answer = obj
        .get("answer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let sources = obj
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let url = s.get("url").and_then(|v| v.as_str())?.trim().to_string();
                    if url.is_empty() {
                        return None;
                    }
                    Some(AiWebSource {
                        title: s.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        url,
                        snippet: s.get("snippet").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if answer.is_empty() && sources.is_empty() {
        return Err("Search produced no answer.".to_string());
    }

    Ok(AiWebSearchResult { answer, sources })
}

// ---------------------------------------------------------------------------------------
// Research loop (moved here from the removed feeds module; AI web search is its only user)
// ---------------------------------------------------------------------------------------

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

/// Extract a JSON object from a string that may contain markdown fences or extra text.
fn extract_json_object(text: &str) -> Result<Value, String> {
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

/// Tool definitions for the research loop (web_search and web_fetch only).
fn research_tools() -> Vec<Value> {
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

/// Execute a research tool call, returning the result string.
async fn execute_research_tool(state: &AppState, name: &str, input: &Value) -> String {
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

    let text = crate::llm::strip_think_tags(&content_acc);
    Ok((text, tool_uses))
}

/// Run an agentic web-research loop (streaming) against the MiniMax Anthropic-compatible
/// API. The model is given the `web_search` + `web_fetch` tools and loops up to
/// `max_rounds`, executing tool calls in-Rust, until it returns a final text answer.
/// Returns the final assistant text (the caller parses whatever format the system prompt
/// requested).
///
/// Note: this predates the shared `llm::run_tool_loop` and is still pinned to MiniMax-M3 on
/// the Anthropic-compatible endpoint, whatever model is configured.
async fn run_agentic_loop(
    state: &AppState,
    system: &str,
    user_prompt: &str,
    max_rounds: u32,
) -> Result<String, String> {
    let (api_key, endpoint) = resolve_minimax_config(state)?;
    let tools = research_tools();

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
            let result = execute_research_tool(state, tool_name, &input).await;
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
    fn extract_json_object_from_noisy_model_output() {
        let raw = "```json\n{\"title\":\"hi\",\"summary\":\"s\"}\n```";
        let obj = extract_json_object(raw).expect("should parse object");
        assert_eq!(obj["title"], "hi");
    }

    #[test]
    fn extract_json_object_errors_on_malformed() {
        assert!(extract_json_object("not json").is_err());
        assert!(extract_json_object("{ broken").is_err());
    }
}
