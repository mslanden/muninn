//! AI web search for the Web tile (the research loop lives in `crate::search`).

use tauri::State;

use crate::models::*;
use crate::AppState;

/// AI-powered web search: researches a natural-language query and returns a synthesized
/// Markdown answer plus a list of cited source links.
#[tauri::command]
pub async fn ai_web_search(
    state: State<'_, AppState>,
    req: AiWebSearchRequest,
) -> Result<AiWebSearchResult, String> {
    crate::search::ai_web_search_impl(state.inner(), &req.query).await
}
