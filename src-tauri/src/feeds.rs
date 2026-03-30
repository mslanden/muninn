use rusqlite::params;
use serde_json::{json, Value};

use crate::db::*;
use crate::AppState;

/// Strip <think>...</think> reasoning tags from MiniMax model output.
fn strip_think_tags(text: &str) -> String {
    let mut result = text.to_string();
    loop {
        let start = result.find("<think>");
        let end = result.find("</think>");
        match (start, end) {
            (Some(s), Some(e)) if e > s => {
                result = format!("{}{}", &result[..s], &result[e + 8..]);
            }
            (Some(s), None) => {
                result = result[..s].to_string();
                break;
            }
            _ => break,
        }
    }
    result.trim().to_string()
}

/// Extract a JSON array from a string that may contain markdown fences or extra text.
fn extract_json_array(text: &str) -> Result<Vec<Value>, String> {
    // Try stripping markdown code fences first
    let cleaned = if let Some(start) = text.find("```") {
        let inner = &text[start + 3..];
        // Skip optional language tag
        let inner = if inner.starts_with("json") { &inner[4..] } else { inner };
        if let Some(end) = inner.find("```") {
            inner[..end].trim()
        } else {
            inner.trim()
        }
    } else {
        text.trim()
    };

    // Find the outermost JSON array
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

/// Call MiniMax via the Anthropic-compatible API (non-streaming) with a single user message.
/// Returns the assistant's text response.
async fn call_minimax_simple(state: &AppState, prompt: &str) -> Result<String, String> {
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

    let request_body = json!({
        "model": "MiniMax-M2.7",
        "max_tokens": 2048,
        "messages": [
            {
                "role": "user",
                "content": [{"type": "text", "text": prompt}]
            }
        ]
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

    let resp: Value = response.json().await.map_err(|e| e.to_string())?;
    let text = resp["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok(strip_think_tags(&text))
}

/// Refresh a feed: search for each topic with Brave, pass results to MiniMax,
/// store curated items in the database. Returns the number of items added.
pub async fn refresh_feed_impl(state: &AppState, feed_id: i64) -> Result<usize, String> {
    // 1. Load feed config
    let (name, topics_json) = with_db(state, |db| {
        db.query_row(
            "SELECT name, topics_json FROM ai_feeds WHERE id = ?1",
            params![feed_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| e.to_string())
    })?;

    let topics: Vec<String> = serde_json::from_str(&topics_json).unwrap_or_default();
    if topics.is_empty() {
        return Err("No topics configured for this feed.".to_string());
    }

    // 2. Search each topic with Brave (3 results per topic)
    let mut search_sections: Vec<String> = Vec::new();
    for topic in &topics {
        match crate::tools::web_search_impl(state, topic, 3).await {
            Ok(results) if results != "No results found." => {
                search_sections.push(format!("### Topic: {}\n{}", topic, results));
            }
            Ok(_) => {}
            Err(e) => {
                // Include the error as context so the AI can note it, but continue
                search_sections.push(format!("### Topic: {}\nSearch failed: {}", topic, e));
            }
        }
    }

    if search_sections.is_empty() {
        return Err(
            "All searches returned no results. Check your Brave Search API key in Settings."
                .to_string(),
        );
    }

    // 3. Build prompt for MiniMax
    let search_context = search_sections.join("\n\n");
    let prompt = format!(
        r#"You are a news curator for a personal feed called "{}".
Based on the search results below, create 4-6 high-quality feed items that are interesting and informative.

{}

Output ONLY a JSON array with this exact structure (no markdown, no extra text):
[
  {{
    "title": "A short, compelling headline (max 80 characters)",
    "summary": "A 2-3 sentence summary of the key point (max 220 characters)",
    "url": "The most relevant URL from the search results above",
    "source_name": "The website or publication name"
  }}
]

Rules:
- Only use URLs that appear verbatim in the search results above — never invent URLs.
- Keep titles under 80 characters.
- Keep summaries concise and informative."#,
        name, search_context
    );

    // 4. Call MiniMax
    let response_text = call_minimax_simple(state, &prompt).await?;

    // 5. Parse the JSON array
    let items = extract_json_array(&response_text)
        .map_err(|e| format!("Could not parse AI response as JSON: {}. Raw: {}", e, &response_text[..response_text.len().min(300)]))?;

    // 6. Store items in the database
    let mut count = 0usize;
    for item in &items {
        let title = item["title"].as_str().unwrap_or("").trim().to_string();
        let summary = item["summary"].as_str().unwrap_or("").trim().to_string();
        let url = item["url"].as_str().unwrap_or("").trim().to_string();
        let source_name = item["source_name"].as_str().unwrap_or("").trim().to_string();

        if title.is_empty() {
            continue;
        }

        with_db(state, |db| {
            db.execute(
                r#"INSERT INTO ai_feed_items (feed_id, title, summary, url, source_name, is_read)
                   VALUES (?1, ?2, ?3, ?4, ?5, 0)"#,
                params![feed_id, title, summary, url, source_name],
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })?;
        count += 1;
    }

    // 7. Update last_refreshed_at
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
