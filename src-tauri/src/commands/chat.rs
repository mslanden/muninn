//! The chat turn entrypoint, stop generation, session todos, ask_user plumbing.

use rusqlite::params;
use tauri::{AppHandle, Manager, State};


use crate::AppState;
use crate::db::*;
use crate::llm::*;
use crate::models::*;
use crate::tools::*;

#[tauri::command]
pub async fn chat(app: AppHandle, state: State<'_, AppState>, req: ChatRequest) -> Result<ChatResponse, String> {
    with_db(&state, |db| {
        // Ensure the session exists (it may have been cleaned up by a previous version)
        db.execute(
            "INSERT OR IGNORE INTO sessions (id) VALUES (?1)",
            params![req.session_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;
    let user_blocks = vec![serde_json::json!({"type": "text", "text": req.message.clone()})];
    insert_message(&state, &req.session_id, "user", &req.message, &user_blocks, false)?;
    add_feed_item(
        &state,
        "message",
        "User Message",
        &truncate_chars(&req.message, 140),
        "chat",
        &req.session_id,
    );

    let mut history = load_history_structured(&state, &req.session_id, 200)?;

    // Trim history to fit within ~80k tokens (leaving ample room for system prompt, tool
    // definitions, and the response on smaller-context providers like MiniMax-M2.7). Rough
    // estimate: 1 token ≈ 4 chars. We measure on the structured JSON length so tool_use /
    // tool_result blocks count toward the budget. After trimming we sanitize so no dangling
    // tool_use/tool_result pairs survive (the provider API rejects those with a 400).
    let max_history_chars: usize = 80_000 * 4;
    let mut total_chars: usize = history.iter().map(|h| h.char_len()).sum();
    while total_chars > max_history_chars && history.len() > 2 {
        total_chars -= history[0].char_len();
        history.remove(0);
    }
    sanitize_tool_pairs(&mut history);

    let configured_provider =
        get_app_setting_value(&state, "llm/provider")?.unwrap_or_else(|| "minimax".to_string());
    let provider = req
        .provider
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or(configured_provider);
    let model = req.model.unwrap_or_else(|| {
        get_app_setting_value(&state, "llm/model")
            .ok()
            .flatten()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| {
                if provider == "minimax" {
                    "MiniMax-M2.7".to_string()
                } else {
                    "gpt-4.1-mini".to_string()
                }
            })
    });

    // Determine if this session belongs to a project
    let session_project_path: String = with_db(&state, |db| {
        db.query_row(
            "SELECT project_path FROM sessions WHERE id = ?1",
            params![req.session_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| e.to_string())
    })?;

    // A chat without an explicit folder operates in the default workspace root
    // ("main directory"); picking a folder scopes it to that project path.
    let scope = if session_project_path.is_empty() {
        Some(WorkspaceScope::from_state(&state)?)
    } else {
        Some(WorkspaceScope::from_path(&session_project_path)?)
    };

    let (reply, reply_blocks) = if let Some(agent_id) = req.agent_id.as_ref() {
        run_agent(&app, &state, &req.session_id, agent_id, &history).await?
    } else if provider == "minimax" || model.starts_with("MiniMax-") {
        chat_with_minimax_anthropic(&app, &state, &req.session_id, &model, &history, scope.as_ref(), true).await?
    } else {
        chat_with_openai(&app, &state, &req.session_id, &model, &history, scope.as_ref(), true).await?
    };

    insert_message(&state, &req.session_id, "assistant", &reply, &reply_blocks, false)?;
    add_feed_item(
        &state,
        "response",
        "Assistant Response",
        &truncate_chars(&reply, 140),
        "robot",
        &req.session_id,
    );

    // Only run extraction for main chat (no project)
    if session_project_path.is_empty() {
        let extraction_app = app.clone();
        let extraction_user_msg = req.message.clone();
        let extraction_reply = reply.clone();
        tauri::async_runtime::spawn(async move {
            let state = extraction_app.state::<AppState>();
            run_extraction(&extraction_app, &state, &extraction_user_msg, &extraction_reply).await;
        });
    }

    Ok(ChatResponse { reply })
}

#[tauri::command]
pub fn stop_chat(state: State<AppState>, session_id: String) -> Result<(), String> {
    {
        // Recover from a poisoned lock rather than propagating an error: a panic in
        // an unrelated chat turn must never permanently disable the user's ability to
        // stop generation. The HashSet contents are still valid after a poison.
        let mut stops = state
            .stop_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stops.insert(session_id.clone());
    }
    // Also unblock any in-flight ask_user call — the provider loop can't
    // terminate if it's parked on an awaited oneshot.
    crate::interactive::cancel_question(&state, &session_id);
    Ok(())
}

// ==================== TODOS + INTERACTIVE QUESTIONS ====================

#[tauri::command]
pub fn get_session_todos(state: State<AppState>, session_id: String) -> Result<Vec<serde_json::Value>, String> {
    Ok(crate::todos::get_todos(&state, &session_id))
}

#[tauri::command]
pub fn answer_user_question(
    state: State<AppState>,
    session_id: String,
    answer: String,
) -> Result<(), String> {
    crate::interactive::deliver_answer(&state, &session_id, &answer)
}

#[tauri::command]
pub fn cancel_user_question(state: State<AppState>, session_id: String) -> Result<(), String> {
    crate::interactive::cancel_question(&state, &session_id);
    Ok(())
}

#[tauri::command]
pub fn has_pending_question(state: State<AppState>, session_id: String) -> Result<bool, String> {
    Ok(crate::interactive::has_pending(&state, &session_id))
}

// ==================== PROJECT CRUD ====================
