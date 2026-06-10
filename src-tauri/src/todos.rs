//! Agent-owned to-do list: ephemeral per-session scratchpad that the agent
//! rewrites via the `write_todos` tool. Held in `AppState.todos` as a
//! `HashMap<session_id, Vec<TodoItem>>` — intentionally not persisted.
//!
//! The agent's plan is only useful for the lifetime of a chat turn + a little
//! beyond (so a subsequent follow-up turn can see the checked-off state). If
//! the app restarts, the plan is gone and the agent rebuilds it.

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::models::TodoItem;
use crate::AppState;

const ALLOWED_STATUSES: &[&str] = &["pending", "in_progress", "completed"];

/// Replace the entire todo list for a session.
///
/// Enforces two invariants the agent tends to violate:
/// - at most one item may be `in_progress`
/// - status strings must be from the allowed set
///
/// Emits a `todos-changed` event so the UI can re-render.
pub(crate) fn write_todos(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    items: Vec<TodoItem>,
) -> Result<String, String> {
    for item in &items {
        if !ALLOWED_STATUSES.contains(&item.status.as_str()) {
            return Err(format!(
                "Invalid status '{}'. Must be one of: {}.",
                item.status,
                ALLOWED_STATUSES.join(", ")
            ));
        }
        if item.content.trim().is_empty() {
            return Err("Every todo must have non-empty content.".to_string());
        }
    }
    let in_progress_count = items.iter().filter(|i| i.status == "in_progress").count();
    if in_progress_count > 1 {
        return Err(
            "Only one todo may be 'in_progress' at a time. Set the others to 'pending' \
             or 'completed'."
                .to_string(),
        );
    }

    let pending = items.iter().filter(|i| i.status == "pending").count();
    let in_prog = in_progress_count;
    let done = items.iter().filter(|i| i.status == "completed").count();

    {
        let mut todos = state
            .todos
            .lock()
            .map_err(|_| "todos lock poisoned".to_string())?;
        todos.insert(session_id.to_string(), items.clone());
    }

    let _ = app.emit(
        "todos-changed",
        json!({
            "session_id": session_id,
            "items": items,
        }),
    );

    Ok(format!(
        "Todo list updated: {pending} pending, {in_prog} in progress, {done} completed."
    ))
}

/// Get a read-only snapshot of the current list (used by the UI to rehydrate
/// after reload, not by the agent).
pub(crate) fn get_todos(state: &AppState, session_id: &str) -> Vec<Value> {
    let todos = match state.todos.lock() {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    todos
        .get(session_id)
        .map(|items| {
            items
                .iter()
                .map(|i| {
                    json!({
                        "content": i.content,
                        "status": i.status,
                        "activeForm": i.active_form,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Drop todos for a session — called when a session is deleted.
pub(crate) fn clear_for_session(state: &AppState, session_id: &str) {
    if let Ok(mut todos) = state.todos.lock() {
        todos.remove(session_id);
    }
}
