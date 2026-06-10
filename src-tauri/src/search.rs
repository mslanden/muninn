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

    let raw = crate::feeds::run_agentic_loop(state, &system, &user, SEARCH_MAX_ROUNDS).await?;

    let obj = crate::feeds::extract_json_object(&raw).map_err(|e| {
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
