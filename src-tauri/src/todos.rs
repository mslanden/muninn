//! Agent-owned to-do list: per-session scratchpad the agent rewrites via the
//! `write_todos` tool. Held in `AppState.todos` as a
//! `HashMap<session_id, Vec<TodoItem>>` for fast access, and mirrored to the
//! `session_todos` table so a chat's plan survives an app restart (the UI
//! rehydrates it via `get_todos`).

use rusqlite::params;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::db::with_db;
use crate::models::TodoItem;
use crate::AppState;

/// Serialize todos to the canonical JSON array used in both the DB row and the
/// `todos-changed` event / `get_todos` output.
fn todos_to_json(items: &[TodoItem]) -> Vec<Value> {
    items
        .iter()
        .map(|i| json!({"content": i.content, "status": i.status, "activeForm": i.active_form}))
        .collect()
}

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

    // Persist so the plan survives a restart (best-effort — failure here must not
    // fail the tool call; the in-memory copy is still authoritative this session).
    let json_items = todos_to_json(&items);
    let _ = with_db(state, |db| {
        db.execute(
            "INSERT INTO session_todos (session_id, todos_json, updated_at) VALUES (?1, ?2, CURRENT_TIMESTAMP) \
             ON CONFLICT(session_id) DO UPDATE SET todos_json = excluded.todos_json, updated_at = CURRENT_TIMESTAMP",
            params![session_id, serde_json::to_string(&json_items).unwrap_or_else(|_| "[]".to_string())],
        )
        .map_err(|e| e.to_string())
    });

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
    // In-memory copy wins when present (same-session, freshest).
    if let Ok(todos) = state.todos.lock() {
        if let Some(items) = todos.get(session_id) {
            return todos_to_json(items);
        }
    }
    // Otherwise rehydrate from the DB (e.g. after an app restart).
    with_db(state, |db| {
        let stored: Option<String> = db
            .query_row(
                "SELECT todos_json FROM session_todos WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .ok();
        Ok(stored
            .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
            .unwrap_or_default())
    })
    .unwrap_or_default()
}

/// Drop todos for a session — called when a session is deleted.
pub(crate) fn clear_for_session(state: &AppState, session_id: &str) {
    if let Ok(mut todos) = state.todos.lock() {
        todos.remove(session_id);
    }
    let _ = with_db(state, |db| {
        db.execute("DELETE FROM session_todos WHERE session_id = ?1", params![session_id])
            .map_err(|e| e.to_string())
    });
}
