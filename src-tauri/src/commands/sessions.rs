//! Session lifecycle: create/list/delete sessions, message history, tool logs, search.

use rusqlite::params;
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
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT role, content, created_at FROM messages WHERE session_id = ?1 AND hidden = 0 ORDER BY id ASC",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(MessageRecord {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    created_at: row.get(2)?,
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
                          (SELECT m4.content FROM messages m4 WHERE m4.session_id = s.id AND m4.hidden = 0 ORDER BY m4.id DESC LIMIT 1) as preview
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
                        s.project_path
                    FROM sessions s
                    LEFT JOIN messages m ON m.session_id = s.id AND m.hidden = 0
                    WHERE s.project_path = ?1
                      AND EXISTS (
                        SELECT 1
                        FROM messages mx
                        WHERE mx.session_id = s.id AND mx.hidden = 0 AND mx.content LIKE ?2
                    )
                    GROUP BY s.id
                    HAVING COUNT(m.id) > 0
                    ORDER BY datetime(last_message_at) DESC
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
                        s.project_path
                    FROM sessions s
                    LEFT JOIN messages m ON m.session_id = s.id AND m.hidden = 0
                    WHERE s.project_path = ?1
                    GROUP BY s.id
                    HAVING COUNT(m.id) > 0
                    ORDER BY datetime(last_message_at) DESC
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
