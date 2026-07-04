//! Deferred-command queue management for the auto-verify safety flow.
//!
//! When the LLM verifier escalates a dangerous command and the user is unavailable to
//! approve it, the command is parked in `deferred_commands`. These commands let the UI list
//! pending entries and later approve (re-resolve the workspace scope and run) or dismiss them.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use tauri::State;

use crate::AppState;
use crate::db::{insert_message, with_db};
use crate::tools::{log_tool, run_command_scoped_unchecked, WorkspaceScope};

#[derive(Debug, Serialize)]
pub struct DeferredCommand {
    pub id: i64,
    pub session_id: String,
    pub command: String,
    pub cwd: Option<String>,
    pub workspace_root: Option<String>,
    pub reasons: Option<String>,
    pub verifier_note: Option<String>,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

/// List deferred commands, most recent first. With no explicit `status`, returns only
/// pending rows. An optional `session_id` scopes the list to one session.
#[tauri::command]
pub fn list_deferred_commands(
    state: State<AppState>,
    session_id: Option<String>,
    status: Option<String>,
) -> Result<Vec<DeferredCommand>, String> {
    list_deferred_commands_core(&state, session_id, status)
}

pub(crate) fn list_deferred_commands_core(
    state: &AppState,
    session_id: Option<String>,
    status: Option<String>,
) -> Result<Vec<DeferredCommand>, String> {
    let status_filter = status.unwrap_or_else(|| "pending".to_string());
    with_db(state, |db| {
        let mut sql = String::from(
            "SELECT id, session_id, command, cwd, workspace_root, reasons, verifier_note, status, created_at, resolved_at \
             FROM deferred_commands WHERE status = ?1",
        );
        if session_id.is_some() {
            sql.push_str(" AND session_id = ?2");
        }
        sql.push_str(" ORDER BY id DESC");

        let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<DeferredCommand> {
            Ok(DeferredCommand {
                id: row.get(0)?,
                session_id: row.get(1)?,
                command: row.get(2)?,
                cwd: row.get(3)?,
                workspace_root: row.get(4)?,
                reasons: row.get(5)?,
                verifier_note: row.get(6)?,
                status: row.get(7)?,
                created_at: row.get(8)?,
                resolved_at: row.get(9)?,
            })
        };
        let rows: Vec<DeferredCommand> = if let Some(sid) = &session_id {
            stmt.query_map(params![status_filter, sid], map_row)
                .map_err(|e| e.to_string())?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| e.to_string())?
        } else {
            stmt.query_map(params![status_filter], map_row)
                .map_err(|e| e.to_string())?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| e.to_string())?
        };
        Ok(rows)
    })
}

/// Approve and execute a pending deferred command: re-resolve the workspace scope from the
/// stored root, run the command unchecked (approval already granted), mark it approved, and
/// insert a hidden message so the agent sees the outcome next turn. Returns the output.
#[tauri::command]
pub fn approve_deferred_command(state: State<AppState>, id: i64) -> Result<String, String> {
    approve_deferred_command_core(&state, id)
}

pub(crate) fn approve_deferred_command_core(state: &AppState, id: i64) -> Result<String, String> {
    // Load the pending row (guard: only pending rows can be approved).
    let row: Option<(String, String, Option<String>, Option<String>)> = with_db(state, |db| {
        db.query_row(
            "SELECT session_id, command, cwd, workspace_root FROM deferred_commands WHERE id = ?1 AND status = 'pending'",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|e| e.to_string())
    })?;

    let (session_id, command, cwd, workspace_root) =
        row.ok_or_else(|| "no pending deferred command with that id".to_string())?;

    // Atomically claim the row before running so a double-click (or two windows) can't
    // execute the command twice. Whoever loses the claim gets a clean error.
    let claimed = with_db(&state, |db| {
        db.execute(
            "UPDATE deferred_commands SET status = 'approved', resolved_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status = 'pending'",
            params![id],
        )
        .map_err(|e| e.to_string())
    })?;
    if claimed == 0 {
        return Err("no pending deferred command with that id".to_string());
    }

    let root = workspace_root
        .clone()
        .ok_or_else(|| "deferred command has no stored workspace root".to_string())?;

    // If the run can't proceed, release the claim so the user can retry after fixing the cause.
    let run = WorkspaceScope::from_path(&root)
        .and_then(|scope| run_command_scoped_unchecked(&scope, &command, cwd.as_deref()));
    let output = match run {
        Ok(out) => out,
        Err(e) => {
            let _ = with_db(&state, |db| {
                db.execute(
                    "UPDATE deferred_commands SET status = 'pending', resolved_at = NULL WHERE id = ?1",
                    params![id],
                )
                .map_err(|e| e.to_string())
            });
            return Err(e);
        }
    };

    // Best-effort tool log.
    log_tool(&state, &session_id, "run_command", &command, &output, "ok");

    // Hidden message so the agent sees the outcome on its next turn.
    let hidden_text = format!("[deferred command id {} was approved and executed]\n{}", id, output);
    let _ = insert_message(&state, &session_id, "user", &hidden_text, &[], true);

    Ok(output)
}

/// Dismiss a pending deferred command. Returns whether a pending row was updated.
#[tauri::command]
pub fn dismiss_deferred_command(state: State<AppState>, id: i64) -> Result<bool, String> {
    dismiss_deferred_command_core(&state, id)
}

pub(crate) fn dismiss_deferred_command_core(state: &AppState, id: i64) -> Result<bool, String> {
    with_db(state, |db| {
        let changed = db
            .execute(
                "UPDATE deferred_commands SET status = 'dismissed', resolved_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status = 'pending'",
                params![id],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}
