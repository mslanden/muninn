//! The chat turn entrypoint, stop generation, session todos, ask_user plumbing.

use rusqlite::params;
use tauri::{AppHandle, Emitter, Manager, State};


use crate::AppState;
use crate::db::*;
use crate::llm::*;
use crate::models::*;
use crate::tools::*;

#[tauri::command]
pub async fn chat(app: AppHandle, state: State<'_, AppState>, req: ChatRequest) -> Result<ChatResponse, String> {
    chat_core(&app, &state, req).await
}

pub(crate) async fn chat_core(app: &AppHandle, state: &AppState, req: ChatRequest) -> Result<ChatResponse, String> {
    with_db(state, |db| {
        // Ensure the session exists (it may have been cleaned up by a previous version)
        db.execute(
            "INSERT OR IGNORE INTO sessions (id) VALUES (?1)",
            params![req.session_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;
    // Build the user turn: the text (if any) plus one Anthropic image block per attachment.
    // Vision-capable models (e.g. MiniMax-M3) read these directly; text-only models ignore
    // them. Images are stored as base64 source blocks so they persist and re-render.
    let mut user_blocks: Vec<serde_json::Value> = Vec::new();
    if !req.message.trim().is_empty() {
        user_blocks.push(serde_json::json!({"type": "text", "text": req.message.clone()}));
    }
    if let Some(attachments) = &req.attachments {
        for att in attachments {
            user_blocks.push(serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": att.media_type,
                    "data": att.data,
                }
            }));
        }
    }
    if user_blocks.is_empty() {
        // Guard: an empty turn with no text and no images — keep a text block so the row is valid.
        user_blocks.push(serde_json::json!({"type": "text", "text": req.message.clone()}));
    }
    let image_count = req.attachments.as_ref().map(|a| a.len()).unwrap_or(0);
    // Plain-text fallback for the DB `content` / feed when the turn is image-only.
    let display_text = if req.message.trim().is_empty() && image_count > 0 {
        format!("[{} image{}]", image_count, if image_count == 1 { "" } else { "s" })
    } else {
        req.message.clone()
    };
    insert_message(&state, &req.session_id, "user", &display_text, &user_blocks, false)?;
    add_feed_item(
        &state,
        "message",
        "User Message",
        &truncate_chars(&display_text, 140),
        "chat",
        &req.session_id,
    );

    let mut history = load_history_structured(&state, &req.session_id, 200)?;

    // Trim history to fit within ~300k tokens (MiniMax-M3 has a 1M context window; this
    // leaves ample room for the system prompt, tool definitions, images, and the response).
    // Rough estimate: 1 token ≈ 4 chars. `char_len` counts tool_use/tool_result blocks toward
    // the budget and counts image blocks at a fixed cost rather than their base64 length.
    // After trimming we sanitize so no dangling tool_use/tool_result pairs survive (the
    // provider API rejects those with a 400).
    let max_history_chars: usize = 300_000 * 4;
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
                    "MiniMax-M3".to_string()
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

    let (reply, reply_blocks) = dispatch_chat_turn(
        &app, &state, &req.session_id, req.agent_id.as_deref(), &provider, &model, &history, scope.as_ref(),
    )
    .await?;

    insert_message(&state, &req.session_id, "assistant", &reply, &reply_blocks, false)?;
    add_feed_item(
        &state,
        "response",
        "Assistant Response",
        &truncate_chars(&reply, 140),
        "robot",
        &req.session_id,
    );

    // ---- Goal auto-continue loop -----------------------------------------------------------
    // If this session has an active goal, evaluate whether the assistant's latest reply meets
    // it. If not, re-prompt the agent (via a hidden user message) and run another turn, up to
    // a configurable iteration cap, so the agent keeps working without the user in the loop.
    let mut latest_reply = reply.clone();
    // A stopped initial turn means the user wants everything halted — never enter the loop.
    if latest_reply != STOPPED_BY_USER_SENTINEL {
        let max_iterations: u32 = get_app_setting_value(&state, "goal/max_iterations")?
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(8)
            .clamp(1, 25);

        let mut iteration: u32 = 1;
        loop {
            // Re-read the goal each pass: it may have been met (cleared) or cleared by the user.
            let goal: Option<String> = with_db(&state, |db| {
                db.query_row(
                    "SELECT goal FROM sessions WHERE id = ?1",
                    params![req.session_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .map_err(|e| e.to_string())
            })?;
            let goal = match goal {
                Some(g) if !g.trim().is_empty() => g,
                _ => break, // no active goal
            };

            // Honor a Stop click, mirroring run_tool_loop: check_stop_requested removes and
            // returns the flag, so consuming it here halts the loop without leaving a stale
            // entry that would block the session's next turn.
            if check_stop_requested(&state, &req.session_id) {
                break;
            }

            let (met, eval_reason) = match evaluate_goal_met(&state, &goal, &latest_reply).await {
                Ok(v) => v,
                Err(e) => {
                    let _ = app.emit(
                        "goal-iteration",
                        serde_json::json!({
                            "session_id": req.session_id,
                            "iteration": iteration,
                            "reason": format!("goal evaluation failed: {e} — pausing auto-continue"),
                            "done": true,
                        }),
                    );
                    break; // leave the goal set so the user can retry by sending a message
                }
            };

            if met {
                with_db(&state, |db| {
                    db.execute(
                        "UPDATE sessions SET goal = NULL WHERE id = ?1",
                        params![req.session_id],
                    )
                    .map_err(|e| e.to_string())?;
                    Ok(())
                })?;
                let _ = app.emit(
                    "goal-met",
                    serde_json::json!({
                        "session_id": req.session_id,
                        "reason": eval_reason,
                    }),
                );
                add_feed_item(
                    &state,
                    "response",
                    "Goal met",
                    &truncate_chars(&eval_reason, 140),
                    "robot",
                    &req.session_id,
                );
                break;
            }

            if iteration > max_iterations {
                let _ = app.emit(
                    "goal-iteration",
                    serde_json::json!({
                        "session_id": req.session_id,
                        "iteration": iteration,
                        "reason": "iteration cap reached — send a message to continue",
                        "done": true,
                    }),
                );
                break; // leave the goal set
            }

            // Not met and within the cap: re-prompt the agent to keep working.
            let continue_msg = format!(
                "[goal loop — iteration {iteration}] The goal is not yet achieved: {eval_reason}. \
                Continue working toward: {goal}. If you are genuinely blocked, say exactly what you \
                need from the user."
            );
            let continue_blocks =
                vec![serde_json::json!({"type": "text", "text": continue_msg.clone()})];
            insert_message(&state, &req.session_id, "user", &continue_msg, &continue_blocks, true)?;

            let _ = app.emit(
                "goal-iteration",
                serde_json::json!({
                    "session_id": req.session_id,
                    "iteration": iteration,
                    "reason": eval_reason,
                    "done": false,
                }),
            );

            // Reload history exactly like the initial turn (load + char-budget trim + sanitize).
            let mut history = load_history_structured(&state, &req.session_id, 200)?;
            let mut total_chars: usize = history.iter().map(|h| h.char_len()).sum();
            while total_chars > max_history_chars && history.len() > 2 {
                total_chars -= history[0].char_len();
                history.remove(0);
            }
            sanitize_tool_pairs(&mut history);

            let (iter_reply, iter_blocks) = dispatch_chat_turn(
                &app, &state, &req.session_id, req.agent_id.as_deref(), &provider, &model, &history, scope.as_ref(),
            )
            .await?;

            insert_message(&state, &req.session_id, "assistant", &iter_reply, &iter_blocks, false)?;
            add_feed_item(
                &state,
                "response",
                "Assistant Response",
                &truncate_chars(&iter_reply, 140),
                "robot",
                &req.session_id,
            );

            latest_reply = iter_reply;

            // The user stopped generation mid-iteration: run_tool_loop consumed the flag and
            // returned the sentinel, so honor it here too instead of re-prompting over it.
            if latest_reply == STOPPED_BY_USER_SENTINEL {
                break;
            }

            iteration += 1;
        }
    }

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

    // Return the last iteration's reply (equals the first reply when no goal loop ran).
    Ok(ChatResponse { reply: latest_reply })
}

/// Run one chat turn against the appropriate provider branch. Written once so the initial
/// turn and every goal-loop iteration dispatch identically.
#[allow(clippy::too_many_arguments)]
async fn dispatch_chat_turn(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    agent_id: Option<&str>,
    provider: &str,
    model: &str,
    history: &[HistoryMessage],
    scope: Option<&WorkspaceScope>,
) -> Result<(String, Vec<serde_json::Value>), String> {
    if let Some(agent_id) = agent_id {
        run_agent(app, state, session_id, agent_id, history).await
    } else if provider == "minimax" || model.starts_with("MiniMax-") {
        // MiniMax fast path — unchanged.
        chat_with_minimax_anthropic(app, state, session_id, model, history, scope, true).await
    } else if provider == "openai" {
        chat_with_openai(app, state, session_id, model, history, scope, true).await
    } else {
        // Any other configured provider (anthropic/groq/deepseek/xai/…) routes here.
        chat_dynamic(app, state, session_id, provider, model, history, scope, true).await
    }
}

#[tauri::command]
pub fn stop_chat(state: State<AppState>, session_id: String) -> Result<(), String> {
    stop_chat_core(&state, &session_id)
}

pub(crate) fn stop_chat_core(state: &AppState, session_id: &str) -> Result<(), String> {
    {
        // Recover from a poisoned lock rather than propagating an error: a panic in
        // an unrelated chat turn must never permanently disable the user's ability to
        // stop generation. The HashSet contents are still valid after a poison.
        let mut stops = state
            .stop_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stops.insert(session_id.to_string());
    }
    // Also unblock any in-flight ask_user call — the provider loop can't
    // terminate if it's parked on an awaited oneshot.
    crate::interactive::cancel_question(state, session_id);
    Ok(())
}

// ==================== TODOS + INTERACTIVE QUESTIONS ====================

#[tauri::command]
pub fn get_session_todos(state: State<AppState>, session_id: String) -> Result<Vec<serde_json::Value>, String> {
    get_session_todos_core(&state, &session_id)
}

pub(crate) fn get_session_todos_core(state: &AppState, session_id: &str) -> Result<Vec<serde_json::Value>, String> {
    Ok(crate::todos::get_todos(state, session_id))
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
