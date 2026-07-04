//! Session lifecycle: create/list/delete sessions, message history, tool logs, search.

use rusqlite::{params, OptionalExtension};
use tauri::State;
use uuid::Uuid;


use crate::AppState;
use crate::db::*;
use crate::models::*;

#[tauri::command]
pub fn create_session(
    state: State<AppState>,
    req: Option<SessionCreateRequest>,
) -> Result<SessionRecord, String> {
    create_session_core(&state, req)
}

pub(crate) fn create_session_core(
    state: &AppState,
    req: Option<SessionCreateRequest>,
) -> Result<SessionRecord, String> {
    let id = Uuid::new_v4().to_string();
    let project_path = normalize_project_path(req.as_ref().and_then(|item| item.project_path.as_deref()));

    with_db(&state, |db| {
        db.execute(
            "INSERT INTO sessions (id, project_path) VALUES (?1, ?2)",
            params![id, project_path],
        )
            .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    let (created_at, saved_project_path) = with_db(&state, |db| {
        db.query_row(
            "SELECT created_at, project_path FROM sessions WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| e.to_string())
    })?;

    Ok(SessionRecord {
        id,
        created_at,
        project_path: saved_project_path,
    })
}

#[tauri::command]
pub fn get_messages(state: State<AppState>, session_id: String) -> Result<Vec<MessageRecord>, String> {
    get_messages_core(&state, &session_id)
}

pub(crate) fn get_messages_core(state: &AppState, session_id: &str) -> Result<Vec<MessageRecord>, String> {
    with_db(state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT role, content, content_json, created_at FROM messages WHERE session_id = ?1 AND hidden = 0 ORDER BY id ASC",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![session_id], |row| {
                let blocks_str: String = row.get(2)?;
                let blocks: Vec<serde_json::Value> =
                    serde_json::from_str(&blocks_str).unwrap_or_default();
                Ok(MessageRecord {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    blocks,
                    created_at: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    })
}

/// Render a session's visible conversation as Markdown for export / clipboard copy.
#[tauri::command]
pub fn export_session_markdown(state: State<AppState>, session_id: String) -> Result<String, String> {
    with_db(&state, |db| {
        let created_at: String = db
            .query_row(
                "SELECT created_at FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_default();

        let mut stmt = db
            .prepare(
                "SELECT role, content, created_at FROM messages \
                 WHERE session_id = ?1 AND hidden = 0 ORDER BY id ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;

        let mut md = String::from("# Muninn conversation\n\n");
        if !created_at.is_empty() {
            md.push_str(&format!("_Started {}_\n\n", created_at));
        }
        let mut count = 0;
        for row in rows {
            let (role, content, ts) = row.map_err(|e| e.to_string())?;
            if content.trim().is_empty() {
                continue;
            }
            let speaker = match role.as_str() {
                "user" => "You",
                "assistant" => "Assistant",
                other => other,
            };
            md.push_str(&format!("## {} · {}\n\n{}\n\n", speaker, ts, content.trim()));
            count += 1;
        }
        if count == 0 {
            md.push_str("_(no messages)_\n");
        }
        Ok(md)
    })
}

#[tauri::command]
pub fn get_tool_logs(state: State<AppState>, session_id: String) -> Result<Vec<ToolLogRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT kind, input, output, status, created_at FROM tool_logs WHERE session_id = ?1 ORDER BY id DESC LIMIT 100",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(ToolLogRecord {
                    kind: row.get(0)?,
                    input: row.get(1)?,
                    output: row.get(2)?,
                    status: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn search_messages(state: State<AppState>, req: SearchMessagesRequest) -> Result<Vec<SessionListItem>, String> {
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Err("search query is required".to_string());
    }
    let limit = req.limit.unwrap_or(20);
    let pattern = format!("%{}%", query);
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"SELECT DISTINCT s.id, s.created_at, s.project_path,
                          (SELECT MAX(m2.created_at) FROM messages m2 WHERE m2.session_id = s.id AND m2.hidden = 0) as last_msg,
                          (SELECT COUNT(*) FROM messages m3 WHERE m3.session_id = s.id AND m3.hidden = 0) as msg_count,
                          (SELECT m4.content FROM messages m4 WHERE m4.session_id = s.id AND m4.hidden = 0 ORDER BY m4.id DESC LIMIT 1) as preview,
                          s.pinned, s.title
                   FROM sessions s
                   JOIN messages m ON m.session_id = s.id
                   WHERE m.content LIKE ?1 AND m.hidden = 0
                   ORDER BY last_msg DESC
                   LIMIT ?2"#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![pattern, limit], |row| {
                Ok(SessionListItem {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    project_path: row.get(2)?,
                    last_message_at: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    message_count: row.get(4)?,
                    preview: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    pinned: row.get::<_, i64>(6)? != 0,
                    title: row.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn get_sessions(state: State<AppState>, req: SessionQueryRequest) -> Result<Vec<SessionListItem>, String> {
    get_sessions_core(&state, req)
}

pub(crate) fn get_sessions_core(state: &AppState, req: SessionQueryRequest) -> Result<Vec<SessionListItem>, String> {
    let limit = req.limit.unwrap_or(50).min(500) as i64;
    let query = req.query.unwrap_or_default().trim().to_string();
    let has_query = !query.is_empty();
    let query_like = format!("%{}%", query);
    let project_path = normalize_project_path(req.project_path.as_deref());

    with_db(&state, |db| {
        if has_query {
            let mut stmt = db
                .prepare(
                    r#"
                    SELECT
                        s.id,
                        s.created_at,
                        COALESCE(MAX(m.created_at), s.created_at) AS last_message_at,
                        COUNT(m.id) AS message_count,
                        COALESCE(
                            (SELECT m2.content
                             FROM messages m2
                             WHERE m2.session_id = s.id AND m2.hidden = 0
                             ORDER BY m2.id DESC
                             LIMIT 1),
                            ''
                        ) AS preview,
                        s.project_path,
                        s.pinned,
                        s.title
                    FROM sessions s
                    LEFT JOIN messages m ON m.session_id = s.id AND m.hidden = 0
                    WHERE s.project_path = ?1
                      AND EXISTS (
                        SELECT 1
                        FROM messages mx
                        WHERE mx.session_id = s.id AND mx.hidden = 0 AND mx.content LIKE ?2
                    )
                    GROUP BY s.id
                    HAVING COUNT(m.id) > 0 OR s.pinned = 1
                    ORDER BY s.pinned DESC, datetime(last_message_at) DESC
                    LIMIT ?3
                    "#,
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![project_path, query_like, limit], |row| {
                Ok(SessionListItem {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    last_message_at: row.get(2)?,
                    message_count: row.get(3)?,
                    preview: row.get(4)?,
                    project_path: row.get(5)?,
                    pinned: row.get::<_, i64>(6)? != 0,
                    title: row.get(7)?,
                })
            })
                .map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| e.to_string())?);
            }
            Ok(out)
        } else {
            let mut stmt = db
                .prepare(
                    r#"
                    SELECT
                        s.id,
                        s.created_at,
                        COALESCE(MAX(m.created_at), s.created_at) AS last_message_at,
                        COUNT(m.id) AS message_count,
                        COALESCE(
                            (SELECT m2.content
                             FROM messages m2
                             WHERE m2.session_id = s.id AND m2.hidden = 0
                             ORDER BY m2.id DESC
                             LIMIT 1),
                            ''
                        ) AS preview,
                        s.project_path,
                        s.pinned,
                        s.title
                    FROM sessions s
                    LEFT JOIN messages m ON m.session_id = s.id AND m.hidden = 0
                    WHERE s.project_path = ?1
                    GROUP BY s.id
                    HAVING COUNT(m.id) > 0 OR s.pinned = 1
                    ORDER BY s.pinned DESC, datetime(last_message_at) DESC
                    LIMIT ?2
                    "#,
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![project_path, limit], |row| {
                Ok(SessionListItem {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    last_message_at: row.get(2)?,
                    message_count: row.get(3)?,
                    preview: row.get(4)?,
                    project_path: row.get(5)?,
                    pinned: row.get::<_, i64>(6)? != 0,
                    title: row.get(7)?,
                })
            })
                .map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| e.to_string())?);
            }
            Ok(out)
        }
    })
}

#[tauri::command]
pub fn delete_session(state: State<AppState>, session_id: String) -> Result<bool, String> {
    // Clear any ephemeral per-session state so a reused id doesn't inherit stale data.
    crate::todos::clear_for_session(&state, &session_id);
    crate::interactive::cancel_question(&state, &session_id);

    with_db(&state, |db| {
        db.execute("DELETE FROM tool_logs WHERE session_id = ?1", params![session_id.as_str()])
            .map_err(|e| e.to_string())?;
        db.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id.as_str()])
            .map_err(|e| e.to_string())?;
        let deleted = db
            .execute("DELETE FROM sessions WHERE id = ?1", params![session_id.as_str()])
            .map_err(|e| e.to_string())?;
        Ok(deleted > 0)
    })
}

/// Pin/unpin a session so it sorts to the top of the list (and shows even with no messages).
#[tauri::command]
pub fn set_session_pinned(state: State<AppState>, session_id: String, pinned: bool) -> Result<bool, String> {
    with_db(&state, |db| {
        let updated = db
            .execute(
                "UPDATE sessions SET pinned = ?1 WHERE id = ?2",
                params![pinned as i64, session_id.as_str()],
            )
            .map_err(|e| e.to_string())?;
        Ok(updated > 0)
    })
}

/// Set a user-facing title for a session; an empty string clears it back to NULL.
#[tauri::command]
pub fn rename_session(state: State<AppState>, session_id: String, title: String) -> Result<bool, String> {
    let trimmed = title.trim();
    let new_title: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
    with_db(&state, |db| {
        let updated = db
            .execute(
                "UPDATE sessions SET title = ?1 WHERE id = ?2",
                params![new_title, session_id.as_str()],
            )
            .map_err(|e| e.to_string())?;
        Ok(updated > 0)
    })
}

/// Set a session's auto-continue goal. An empty/whitespace string clears it back to NULL,
/// which disables the "/goal" loop. Returns whether a row was updated.
#[tauri::command]
pub fn set_session_goal(state: State<AppState>, session_id: String, goal: String) -> Result<bool, String> {
    set_session_goal_core(&state, &session_id, &goal)
}

pub(crate) fn set_session_goal_core(state: &AppState, session_id: &str, goal: &str) -> Result<bool, String> {
    let trimmed = goal.trim();
    let new_goal: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
    with_db(state, |db| {
        let updated = db
            .execute(
                "UPDATE sessions SET goal = ?1 WHERE id = ?2",
                params![new_goal, session_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(updated > 0)
    })
}

/// Fetch a session's active goal, or None if unset/cleared.
#[tauri::command]
pub fn get_session_goal(state: State<AppState>, session_id: String) -> Result<Option<String>, String> {
    get_session_goal_core(&state, &session_id)
}

pub(crate) fn get_session_goal_core(state: &AppState, session_id: &str) -> Result<Option<String>, String> {
    with_db(state, |db| {
        let goal: Option<String> = db
            .query_row(
                "SELECT goal FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .flatten();
        Ok(goal.filter(|g| !g.trim().is_empty()))
    })
}
