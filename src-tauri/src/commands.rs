use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use base64::{engine::general_purpose::STANDARD as B64, Engine};

use crate::AppState;
use crate::db::*;
use crate::llm::*;
use crate::models::*;
use crate::tools::*;

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
                "SELECT role, content, created_at FROM messages WHERE session_id = ?1 ORDER BY id ASC",
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
pub fn get_tools(state: State<AppState>) -> Result<Vec<ToolConfigRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"
                SELECT tool_id, name, description, icon_name, enabled
                FROM tools
                ORDER BY tool_id ASC
                "#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ToolConfigRecord {
                    tool_id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    icon_name: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? == 1,
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
pub fn set_tool_enabled(state: State<AppState>, req: ToolToggleRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute(
                r#"
                UPDATE tools
                SET enabled = ?2, updated_at = CURRENT_TIMESTAMP
                WHERE tool_id = ?1
                "#,
                params![req.tool_id, if req.enabled { 1 } else { 0 }],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn get_feed_items(state: State<AppState>, req: FeedListRequest) -> Result<Vec<FeedRecord>, String> {
    let limit = req.limit.unwrap_or(100).min(500) as i64;
    let unread_only = req.unread_only.unwrap_or(false);

    with_db(&state, |db| {
        let mut out = Vec::new();
        if unread_only {
            let mut stmt = db
                .prepare(
                    r#"
                    SELECT id, kind, title, message, icon_name, is_read, metadata, created_at
                    FROM feed_items
                    WHERE is_read = 0
                    ORDER BY id DESC
                    LIMIT ?1
                    "#,
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![limit], |row| {
                    Ok(FeedRecord {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        title: row.get(2)?,
                        message: row.get(3)?,
                        icon_name: row.get(4)?,
                        is_read: row.get::<_, i64>(5)? == 1,
                        metadata: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                out.push(row.map_err(|e| e.to_string())?);
            }
        } else {
            let mut stmt = db
                .prepare(
                    r#"
                    SELECT id, kind, title, message, icon_name, is_read, metadata, created_at
                    FROM feed_items
                    ORDER BY id DESC
                    LIMIT ?1
                    "#,
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![limit], |row| {
                    Ok(FeedRecord {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        title: row.get(2)?,
                        message: row.get(3)?,
                        icon_name: row.get(4)?,
                        is_read: row.get::<_, i64>(5)? == 1,
                        metadata: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                out.push(row.map_err(|e| e.to_string())?);
            }
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn mark_feed_item_read(state: State<AppState>, req: FeedItemRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("UPDATE feed_items SET is_read = 1 WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn mark_all_feed_read(state: State<AppState>) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute("UPDATE feed_items SET is_read = 1", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn remove_feed_item(state: State<AppState>, req: FeedItemRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM feed_items WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn clear_feed_items(state: State<AppState>) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute("DELETE FROM feed_items", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn get_tasks(state: State<AppState>) -> Result<Vec<TaskRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"
                SELECT id, name, description, schedule, next_run, status,
                       completed_at, result, run_count, triggered_by,
                       chain_next, conditions, condition_status,
                       created_at, updated_at
                FROM tasks
                ORDER BY id DESC
                "#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(TaskRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    schedule: row.get(3)?,
                    next_run: row.get(4)?,
                    status: row.get(5)?,
                    completed_at: row.get(6)?,
                    result: row.get(7)?,
                    run_count: row.get(8)?,
                    triggered_by: row.get(9)?,
                    chain_next: row.get(10)?,
                    conditions: row.get(11)?,
                    condition_status: row.get(12)?,
                    created_at: row.get(13)?,
                    updated_at: row.get(14)?,
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
pub fn add_task(state: State<AppState>, req: TaskCreateRequest) -> Result<bool, String> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("task name is required".to_string());
    }

    let schedule = req.schedule.unwrap_or_default();
    let mut next_run = req.next_run.unwrap_or_default();

    // Auto-compute next_run from schedule if not explicitly provided
    if next_run.trim().is_empty() && !schedule.trim().is_empty() {
        let now = chrono::Local::now().naive_local();
        if let Some(computed) = crate::scheduler::next_run_from_schedule(&schedule, now) {
            next_run = computed.format("%Y-%m-%dT%H:%M:%S").to_string();
        }
    }

    with_db(&state, |db| {
        db.execute(
            r#"
            INSERT INTO tasks (name, description, schedule, next_run, status, updated_at)
            VALUES (?1, ?2, ?3, ?4, 'scheduled', CURRENT_TIMESTAMP)
            "#,
            params![name, req.description.unwrap_or_default(), schedule, next_run],
        )
        .map_err(|e| e.to_string())?;
        Ok(true)
    })
}

#[tauri::command]
pub fn update_task(state: State<AppState>, req: TaskUpdateRequest) -> Result<bool, String> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("task name is required".to_string());
    }

    let schedule = req.schedule.unwrap_or_default();
    let mut next_run = req.next_run.unwrap_or_default();

    // Auto-compute next_run from schedule if not explicitly provided
    if next_run.trim().is_empty() && !schedule.trim().is_empty() {
        let now = chrono::Local::now().naive_local();
        if let Some(computed) = crate::scheduler::next_run_from_schedule(&schedule, now) {
            next_run = computed.format("%Y-%m-%dT%H:%M:%S").to_string();
        }
    }

    with_db(&state, |db| {
        let changed = db
            .execute(
                r#"
                UPDATE tasks
                SET name = ?2,
                    description = ?3,
                    schedule = ?4,
                    next_run = ?5,
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?1
                "#,
                params![req.id, name, req.description.unwrap_or_default(), schedule, next_run],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn set_task_status(state: State<AppState>, req: TaskStatusRequest) -> Result<bool, String> {
    let normalized = req.status.trim().to_lowercase();
    if !["scheduled", "running", "completed"].contains(&normalized.as_str()) {
        return Err("status must be scheduled, running, or completed".to_string());
    }

    with_db(&state, |db| {
        let changed = db
            .execute(
                r#"
                UPDATE tasks
                SET status = ?2, updated_at = CURRENT_TIMESTAMP
                WHERE id = ?1
                "#,
                params![req.id, normalized],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn remove_task(state: State<AppState>, req: TaskIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM tasks WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn get_agents(state: State<AppState>) -> Result<Vec<AgentRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"
                SELECT agent_id, name, description, slash_command, system_prompt, max_iterations,
                       enabled, allowed_tools_json, created_at, updated_at
                FROM agents
                ORDER BY datetime(updated_at) DESC
                "#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AgentRecord {
                    agent_id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    slash_command: row.get(3)?,
                    system_prompt: row.get(4)?,
                    max_iterations: row.get(5)?,
                    enabled: row.get::<_, i64>(6)? == 1,
                    allowed_tools_json: row.get(7)?,
                    created_at: row.get(8)?,
                    updated_at: row.get(9)?,
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
pub fn add_agent(state: State<AppState>, req: AgentCreateRequest) -> Result<String, String> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("agent name is required".to_string());
    }
    let agent_id = Uuid::new_v4().to_string();
    let allowed = serde_json::to_string(&req.allowed_tools.unwrap_or_default())
        .map_err(|e| e.to_string())?;

    with_db(&state, |db| {
        db.execute(
            r#"
            INSERT INTO agents (
              agent_id, name, description, slash_command, system_prompt,
              max_iterations, enabled, allowed_tools_json, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, CURRENT_TIMESTAMP)
            "#,
            params![
                agent_id,
                name,
                req.description.unwrap_or_default(),
                req.slash_command.unwrap_or_default(),
                req.system_prompt.unwrap_or_default(),
                req.max_iterations.unwrap_or(5),
                if req.enabled.unwrap_or(true) { 1 } else { 0 },
                allowed
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    Ok(agent_id)
}

#[tauri::command]
pub fn update_agent(state: State<AppState>, req: AgentUpdateRequest) -> Result<bool, String> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("agent name is required".to_string());
    }
    let allowed = serde_json::to_string(&req.allowed_tools.unwrap_or_default())
        .map_err(|e| e.to_string())?;
    let enabled = if req.enabled.unwrap_or(true) { 1 } else { 0 };

    with_db(&state, |db| {
        let changed = db
            .execute(
                r#"
                UPDATE agents
                SET name = ?2,
                    description = ?3,
                    slash_command = ?4,
                    system_prompt = ?5,
                    max_iterations = ?6,
                    enabled = ?7,
                    allowed_tools_json = ?8,
                    updated_at = CURRENT_TIMESTAMP
                WHERE agent_id = ?1
                "#,
                params![
                    req.agent_id,
                    name,
                    req.description.unwrap_or_default(),
                    req.slash_command.unwrap_or_default(),
                    req.system_prompt.unwrap_or_default(),
                    req.max_iterations.unwrap_or(5),
                    enabled,
                    allowed
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn set_agent_enabled(state: State<AppState>, req: AgentToggleRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute(
                r#"
                UPDATE agents
                SET enabled = ?2, updated_at = CURRENT_TIMESTAMP
                WHERE agent_id = ?1
                "#,
                params![req.agent_id, if req.enabled { 1 } else { 0 }],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn remove_agent(state: State<AppState>, req: AgentIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM agents WHERE agent_id = ?1", params![req.agent_id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn get_app_settings(state: State<AppState>) -> Result<HashMap<String, String>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare("SELECT key, value FROM app_config")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        let mut out = HashMap::new();
        for row in rows {
            let (key, value) = row.map_err(|e| e.to_string())?;
            out.insert(key, value);
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn set_app_settings(state: State<AppState>, req: SettingsUpdateRequest) -> Result<(), String> {
    with_db(&state, |db| {
        for (key, value) in req.settings {
            db.execute(
                r#"
                INSERT INTO app_config (key, value, updated_at)
                VALUES (?1, ?2, CURRENT_TIMESTAMP)
                ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP
                "#,
                params![key, value],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
}

#[tauri::command]
pub fn get_memory_profile(state: State<AppState>) -> Result<MemoryProfileRecord, String> {
    with_db(&state, |db| {
        db.query_row(
            "SELECT user_name, user_about, profile_text, identity, personality, goals, preferences, current_state, patterns, updated_at FROM memory_profile WHERE id = 1",
            [],
            |row| {
                Ok(MemoryProfileRecord {
                    user_name: row.get(0)?,
                    user_about: row.get(1)?,
                    profile_text: row.get(2)?,
                    identity: row.get(3)?,
                    personality: row.get(4)?,
                    goals: row.get(5)?,
                    preferences: row.get(6)?,
                    current_state: row.get(7)?,
                    patterns: row.get(8)?,
                    updated_at: row.get(9)?,
                })
            },
        )
        .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub fn set_memory_profile(state: State<AppState>, req: MemoryProfileUpdateRequest) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute(
            r#"
            UPDATE memory_profile
            SET user_name = COALESCE(?1, user_name),
                user_about = COALESCE(?2, user_about),
                profile_text = COALESCE(?3, profile_text),
                identity = COALESCE(?4, identity),
                personality = COALESCE(?5, personality),
                goals = COALESCE(?6, goals),
                preferences = COALESCE(?7, preferences),
                current_state = COALESCE(?8, current_state),
                patterns = COALESCE(?9, patterns),
                updated_at = CURRENT_TIMESTAMP
            WHERE id = 1
            "#,
            params![req.user_name, req.user_about, req.profile_text,
                    req.identity, req.personality, req.goals,
                    req.preferences, req.current_state, req.patterns],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn get_entities(state: State<AppState>) -> Result<Vec<EntityRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare("SELECT id, kind, name, summary, aliases_json, created_at, updated_at FROM entities ORDER BY updated_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let aliases_json: String = row.get(4)?;
                let aliases: Vec<String> = serde_json::from_str(&aliases_json).unwrap_or_default();
                Ok(EntityRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    summary: row.get(3)?,
                    aliases,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
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
pub fn upsert_entity(state: State<AppState>, req: EntityUpsertRequest) -> Result<bool, String> {
    let kind = req.kind.trim().to_string();
    let name = req.name.trim().to_string();
    if kind.is_empty() || name.is_empty() {
        return Err("entity kind and name are required".to_string());
    }
    let aliases_json = serde_json::to_string(&req.aliases.unwrap_or_default())
        .unwrap_or_else(|_| "[]".to_string());
    with_db(&state, |db| {
        if let Some(id) = req.id {
            let changed = db
                .execute(
                    "UPDATE entities SET kind = ?2, name = ?3, summary = ?4, aliases_json = ?5, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
                    params![id, kind, name, req.summary.unwrap_or_default(), aliases_json],
                )
                .map_err(|e| e.to_string())?;
            Ok(changed > 0)
        } else {
            db.execute(
                "INSERT INTO entities (kind, name, summary, aliases_json) VALUES (?1, ?2, ?3, ?4)",
                params![kind, name, req.summary.unwrap_or_default(), aliases_json],
            )
            .map_err(|e| e.to_string())?;
            Ok(true)
        }
    })
}

#[tauri::command]
pub fn delete_entity(state: State<AppState>, req: EntityIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        // Clean up relationships and episode links (SQLite FK cascade may not be enabled)
        let _ = db.execute("DELETE FROM relationships WHERE from_entity_id = ?1 OR to_entity_id = ?1", params![req.id]);
        let _ = db.execute("DELETE FROM episode_entities WHERE entity_id = ?1", params![req.id]);
        let changed = db
            .execute("DELETE FROM entities WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn get_episodes(state: State<AppState>) -> Result<Vec<EpisodeRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare("SELECT id, description, timestamp, created_at FROM episodes ORDER BY id DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, description, timestamp, created_at) = row.map_err(|e| e.to_string())?;
            // Get linked entity names
            let entity_names: Vec<String> = {
                let mut name_stmt = db
                    .prepare("SELECT e.name FROM episode_entities ee JOIN entities e ON ee.entity_id = e.id WHERE ee.episode_id = ?1")
                    .map_err(|e| e.to_string())?;
                let name_rows = name_stmt
                    .query_map(params![id], |row| row.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                let mut names = Vec::new();
                for nr in name_rows {
                    if let Ok(n) = nr {
                        names.push(n);
                    }
                }
                names
            };
            out.push(EpisodeRecord { id, description, timestamp, entity_names, created_at });
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn add_episode(state: State<AppState>, req: EpisodeCreateRequest) -> Result<bool, String> {
    let description = req.description.trim().to_string();
    if description.is_empty() {
        return Err("episode description is required".to_string());
    }
    with_db(&state, |db| {
        db.execute(
            "INSERT INTO episodes (description, timestamp) VALUES (?1, ?2)",
            params![description, req.timestamp.unwrap_or_default()],
        )
        .map_err(|e| e.to_string())?;
        let episode_id = db.last_insert_rowid();

        // Link entity names if provided
        if let Some(names) = &req.entity_names {
            for name in names {
                let entity_id: Option<i64> = db
                    .query_row(
                        "SELECT id FROM entities WHERE name = ?1 COLLATE NOCASE",
                        params![name.trim()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some(eid) = entity_id {
                    let _ = db.execute(
                        "INSERT OR IGNORE INTO episode_entities (episode_id, entity_id) VALUES (?1, ?2)",
                        params![episode_id, eid],
                    );
                }
            }
        }
        Ok(true)
    })
}

#[tauri::command]
pub fn delete_episode(state: State<AppState>, req: EpisodeIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let _ = db.execute("DELETE FROM episode_entities WHERE episode_id = ?1", params![req.id]);
        let changed = db
            .execute("DELETE FROM episodes WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

// ========== RELATIONSHIPS ==========

#[tauri::command]
pub fn get_relationships(state: State<AppState>) -> Result<Vec<RelationshipRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"SELECT r.id, r.from_entity_id, r.to_entity_id,
                          COALESCE(ef.name, '?'), COALESCE(et.name, '?'),
                          r.label, r.created_at
                   FROM relationships r
                   LEFT JOIN entities ef ON r.from_entity_id = ef.id
                   LEFT JOIN entities et ON r.to_entity_id = et.id
                   ORDER BY r.id DESC"#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(RelationshipRecord {
                    id: row.get(0)?,
                    from_entity_id: row.get(1)?,
                    to_entity_id: row.get(2)?,
                    from_name: row.get(3)?,
                    to_name: row.get(4)?,
                    label: row.get(5)?,
                    created_at: row.get(6)?,
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
pub fn upsert_relationship(state: State<AppState>, req: RelationshipUpsertRequest) -> Result<bool, String> {
    let label = req.label.trim().to_string();
    if label.is_empty() {
        return Err("relationship label is required".to_string());
    }
    with_db(&state, |db| {
        db.execute(
            "INSERT OR IGNORE INTO relationships (from_entity_id, to_entity_id, label) VALUES (?1, ?2, ?3)",
            params![req.from_entity_id, req.to_entity_id, label],
        )
        .map_err(|e| e.to_string())?;
        Ok(true)
    })
}

#[tauri::command]
pub fn delete_relationship(state: State<AppState>, req: RelationshipIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM relationships WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

// ========== MESSAGE SEARCH ==========

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
                          (SELECT MAX(m2.created_at) FROM messages m2 WHERE m2.session_id = s.id) as last_msg,
                          (SELECT COUNT(*) FROM messages m3 WHERE m3.session_id = s.id) as msg_count,
                          (SELECT m4.content FROM messages m4 WHERE m4.session_id = s.id ORDER BY m4.id DESC LIMIT 1) as preview
                   FROM sessions s
                   JOIN messages m ON m.session_id = s.id
                   WHERE m.content LIKE ?1
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
pub fn get_gallery_paths(app: AppHandle) -> Result<GalleryPathsRecord, String> {
    let (images_dir, audio_dir) = gallery_dirs(&app)?;
    Ok(GalleryPathsRecord {
        images_dir: images_dir.display().to_string(),
        audio_dir: audio_dir.display().to_string(),
    })
}

#[tauri::command]
pub fn get_gallery_images(app: AppHandle) -> Result<Vec<MediaItemRecord>, String> {
    let (images_dir, _) = gallery_dirs(&app)?;
    list_media_in_dir(&images_dir, &["png", "jpg", "jpeg", "webp", "gif"])
}

#[tauri::command]
pub fn get_gallery_audio(app: AppHandle) -> Result<Vec<MediaItemRecord>, String> {
    let (_, audio_dir) = gallery_dirs(&app)?;
    list_media_in_dir(&audio_dir, &["wav", "mp3", "m4a", "aac", "flac"])
}

#[tauri::command]
pub fn open_content_dir(app: AppHandle) -> Result<bool, String> {
    let dir = content_dir(&app)?;
    std::process::Command::new("open")
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub fn get_content_path(app: AppHandle) -> Result<ContentPathRecord, String> {
    let dir = content_dir(&app)?;
    Ok(ContentPathRecord {
        content_dir: dir.display().to_string(),
    })
}

#[tauri::command]
pub fn get_content_files(app: AppHandle) -> Result<Vec<MediaItemRecord>, String> {
    let dir = content_dir(&app)?;
    list_content_files(&dir)
}

#[tauri::command]
pub fn delete_content_file(app: AppHandle, req: MediaDeleteRequest) -> Result<bool, String> {
    let dir = content_dir(&app)?;
    let candidate = dir.join(&req.name);
    if !candidate.starts_with(&dir) {
        return Err("invalid content path".to_string());
    }
    if !candidate.exists() {
        return Ok(false);
    }
    fs::remove_file(candidate).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub fn import_content_files(app: AppHandle, req: ContentImportRequest) -> Result<Vec<String>, String> {
    let dir = content_dir(&app)?;
    let mut imported = Vec::new();
    for src_path in &req.paths {
        let src = std::path::Path::new(src_path);
        if !src.exists() || !src.is_file() {
            continue;
        }
        let file_name = src.file_name().unwrap_or_default().to_string_lossy().to_string();
        let mut dest = dir.join(&file_name);
        // Handle name collisions
        if dest.exists() {
            let stem = src.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let ext = src.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
            let mut i = 1u32;
            loop {
                dest = dir.join(format!("{} ({}){}", stem, i, ext));
                if !dest.exists() { break; }
                i += 1;
            }
        }
        if fs::copy(src, &dest).is_ok() {
            imported.push(dest.file_name().unwrap_or_default().to_string_lossy().to_string());
        }
    }
    Ok(imported)
}

#[tauri::command]
pub fn read_content_text(app: AppHandle, req: ContentReadRequest) -> Result<ContentReadResponse, String> {
    let dir = content_dir(&app)?;
    let file_path = std::path::Path::new(&req.path);
    let canonical = file_path.canonicalize().map_err(|e| e.to_string())?;
    let canon_dir = dir.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(&canon_dir) {
        return Err("path outside content directory".to_string());
    }
    let bytes = fs::read(&canonical).map_err(|e| e.to_string())?;
    let content = if bytes.len() > 65536 {
        String::from_utf8_lossy(&bytes[..65536]).to_string() + "\n\n... (truncated)"
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };
    let name = canonical.file_name().unwrap_or_default().to_string_lossy().to_string();
    Ok(ContentReadResponse { content, name })
}

#[tauri::command]
pub fn open_content_file(app: AppHandle, req: ContentReadRequest) -> Result<bool, String> {
    let dir = content_dir(&app)?;
    let file_path = std::path::Path::new(&req.path);
    let canonical = file_path.canonicalize().map_err(|e| e.to_string())?;
    let canon_dir = dir.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(&canon_dir) {
        return Err("path outside content directory".to_string());
    }
    std::process::Command::new("open")
        .arg(&canonical)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub fn delete_gallery_media(app: AppHandle, req: MediaDeleteRequest) -> Result<bool, String> {
    let (images_dir, audio_dir) = gallery_dirs(&app)?;
    let base = if req.kind.trim().eq_ignore_ascii_case("audio") {
        audio_dir
    } else {
        images_dir
    };
    let candidate = base.join(req.name);
    if !candidate.starts_with(&base) {
        return Err("invalid media path".to_string());
    }
    if !candidate.exists() {
        return Ok(false);
    }
    fs::remove_file(candidate).map_err(|e| e.to_string())?;
    Ok(true)
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
                             WHERE m2.session_id = s.id
                             ORDER BY m2.id DESC
                             LIMIT 1),
                            ''
                        ) AS preview,
                        s.project_path
                    FROM sessions s
                    LEFT JOIN messages m ON m.session_id = s.id
                    WHERE s.project_path = ?1
                      AND EXISTS (
                        SELECT 1
                        FROM messages mx
                        WHERE mx.session_id = s.id AND mx.content LIKE ?2
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
                             WHERE m2.session_id = s.id
                             ORDER BY m2.id DESC
                             LIMIT 1),
                            ''
                        ) AS preview,
                        s.project_path
                    FROM sessions s
                    LEFT JOIN messages m ON m.session_id = s.id
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

#[tauri::command]
pub fn check_command_safety(command: String) -> SafetyCheck {
    is_dangerous_command(&command)
}

#[tauri::command]
pub fn get_workspace_root(state: State<AppState>) -> Result<PathResult, String> {
    let root = state
        .workspace_root
        .lock()
        .map_err(|_| "workspace lock poisoned".to_string())?
        .display()
        .to_string();
    Ok(PathResult {
        workspace_root: root,
    })
}

#[tauri::command]
pub fn set_workspace_root(state: State<AppState>, req: WorkspaceRootRequest) -> Result<PathResult, String> {
    let root = PathBuf::from(req.workspace_root);
    let canon = root.canonicalize().map_err(|e| e.to_string())?;
    if !canon.is_dir() {
        return Err("workspace root must be a directory".to_string());
    }

    {
        let mut r = state
            .workspace_root
            .lock()
            .map_err(|_| "workspace lock poisoned".to_string())?;
        *r = root.clone();
    }
    {
        let mut c = state
            .workspace_root_canon
            .lock()
            .map_err(|_| "workspace lock poisoned".to_string())?;
        *c = canon;
    }

    upsert_saved_workspace(&state, &root.display().to_string())?;

    Ok(PathResult {
        workspace_root: root.display().to_string(),
    })
}

#[tauri::command]
pub fn list_saved_workspaces(state: State<AppState>) -> Result<Vec<WorkspaceRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT path, last_used_at FROM saved_workspaces ORDER BY datetime(last_used_at) DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(WorkspaceRecord {
                    path: row.get(0)?,
                    last_used_at: row.get(1)?,
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
pub fn save_workspace(state: State<AppState>, req: WorkspacePathRequest) -> Result<(), String> {
    let canon = PathBuf::from(req.path)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !canon.is_dir() {
        return Err("workspace path must be a directory".to_string());
    }
    upsert_saved_workspace(&state, &canon.display().to_string())
}

#[tauri::command]
pub fn remove_saved_workspace(state: State<AppState>, req: WorkspacePathRequest) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute("DELETE FROM saved_workspaces WHERE path = ?1", params![req.path])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn read_file(state: State<AppState>, req: FileReadRequest) -> Result<FsResult, String> {
    read_file_impl(&state, &req)
}

#[tauri::command]
pub fn write_file(state: State<AppState>, req: FileWriteRequest) -> Result<FsResult, String> {
    write_file_impl(&state, &req)
}

#[tauri::command]
pub fn list_files(state: State<AppState>, req: FileListRequest) -> Result<FsResult, String> {
    list_files_impl(&state, &req)
}

#[tauri::command]
pub fn search_files(state: State<AppState>, req: FileSearchRequest) -> Result<FsResult, String> {
    search_files_impl(&state, &req)
}

#[tauri::command]
pub fn start_terminal_process(
    app: AppHandle,
    state: State<AppState>,
    req: TerminalStartRequest,
) -> Result<ProcessStartResponse, String> {
    let safety = is_dangerous_command(&req.command);
    if safety.dangerous && !req.approved_dangerous {
        return Err(format!(
            "command requires approval: {}",
            safety.reasons.join("; ")
        ));
    }

    let cwd = command_cwd(&state, req.cwd.as_deref())?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let mut command = CommandBuilder::new("/bin/zsh");
    command.arg("-lc");
    command.arg(req.command.clone());
    command.cwd(cwd);

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| e.to_string())?;
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    let process_id = Uuid::new_v4().to_string();
    let writer_arc: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(writer));
    let child_arc: Arc<Mutex<Box<dyn portable_pty::Child + Send>>> = Arc::new(Mutex::new(child));

    {
        let mut processes = state
            .processes
            .lock()
            .map_err(|_| "process lock poisoned".to_string())?;
        processes.insert(
            process_id.clone(),
            RunningProcess {
                writer: writer_arc,
                child: child_arc.clone(),
            },
        );
    }

    log_tool(
        &state,
        &req.session_id,
        "terminal_start",
        &req.command,
        "started",
        "ok",
    );

    let app_clone = app.clone();
    let process_id_clone = process_id.clone();

    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = app_clone.emit(
                        "terminal-output",
                        TerminalOutputEvent {
                            process_id: process_id_clone.clone(),
                            chunk,
                        },
                    );
                }
                Err(_) => break,
            }
        }

        let exit_code = match child_arc.lock() {
            Ok(mut child) => match child.wait() {
                Ok(status) => Some(status.exit_code()),
                Err(_) => None,
            },
            Err(_) => None,
        };

        let _ = app_clone.emit(
            "terminal-exit",
            TerminalExitEvent {
                process_id: process_id_clone,
                exit_code,
            },
        );
    });

    Ok(ProcessStartResponse { process_id })
}

#[tauri::command]
pub fn send_terminal_input(state: State<AppState>, req: TerminalInputRequest) -> Result<(), String> {
    let processes = state
        .processes
        .lock()
        .map_err(|_| "process lock poisoned".to_string())?;
    let process = processes
        .get(&req.process_id)
        .ok_or_else(|| "process not found".to_string())?;
    let mut writer = process
        .writer
        .lock()
        .map_err(|_| "writer lock poisoned".to_string())?;

    writer
        .write_all(req.input.as_bytes())
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn stop_terminal_process(state: State<AppState>, req: TerminalStopRequest) -> Result<(), String> {
    let mut processes = state
        .processes
        .lock()
        .map_err(|_| "process lock poisoned".to_string())?;
    let process = processes
        .remove(&req.process_id)
        .ok_or_else(|| "process not found".to_string())?;

    let mut child = process
        .child
        .lock()
        .map_err(|_| "child lock poisoned".to_string())?;
    child.kill().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn chat(app: AppHandle, state: State<'_, AppState>, req: ChatRequest) -> Result<ChatResponse, String> {
    with_db(&state, |db| {
        // Ensure the session exists (it may have been cleaned up by a previous version)
        db.execute(
            "INSERT OR IGNORE INTO sessions (id) VALUES (?1)",
            params![req.session_id],
        )
        .map_err(|e| e.to_string())?;
        db.execute(
            "INSERT INTO messages (session_id, role, content) VALUES (?1, 'user', ?2)",
            params![req.session_id, req.message],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;
    add_feed_item(
        &state,
        "message",
        "User Message",
        &truncate_chars(&req.message, 140),
        "chat",
        &req.session_id,
    );

    let history = with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT 200",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![req.session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;

        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(|e| e.to_string())?);
        }
        items.reverse();

        // Trim history to fit within ~150k tokens (leaving room for system prompt + response)
        // Rough estimate: 1 token ≈ 4 chars
        let max_history_chars: usize = 150_000 * 4;
        let mut total_chars: usize = items.iter().map(|(_, c)| c.len()).sum();
        while total_chars > max_history_chars && items.len() > 2 {
            total_chars -= items[0].1.len();
            items.remove(0);
        }

        Ok(items)
    })?;

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

    let scope = if session_project_path.is_empty() {
        None
    } else {
        Some(WorkspaceScope::from_path(&session_project_path)?)
    };

    let reply = if req.agent_id.is_some() {
        run_agent(&app, &state, &req.session_id, req.agent_id.as_ref().unwrap(), &history).await?
    } else if provider == "minimax" || model.starts_with("MiniMax-") {
        chat_with_minimax_anthropic(&app, &state, &req.session_id, &model, &history, scope.as_ref()).await?
    } else {
        chat_with_openai(&app, &state, &req.session_id, &model, &history, scope.as_ref()).await?
    };

    with_db(&state, |db| {
        db.execute(
            "INSERT INTO messages (session_id, role, content) VALUES (?1, 'assistant', ?2)",
            params![req.session_id, reply],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;
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
    let mut stops = state.stop_requests.lock().map_err(|_| "lock error".to_string())?;
    stops.insert(session_id);
    Ok(())
}

// ==================== PROJECT CRUD ====================

#[tauri::command]
pub fn get_projects(state: State<AppState>) -> Result<Vec<ProjectRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare("SELECT path, name, spino_md, created_at, last_opened_at FROM projects ORDER BY last_opened_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    spino_md: row.get(2)?,
                    created_at: row.get(3)?,
                    last_opened_at: row.get(4)?,
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
pub fn add_project(state: State<AppState>, req: ProjectAddRequest) -> Result<ProjectRecord, String> {
    let path = req.path.trim().to_string();
    if path.is_empty() {
        return Err("path is required".to_string());
    }
    let p = std::path::Path::new(&path);
    if !p.is_dir() {
        return Err(format!("directory does not exist: {}", path));
    }
    let name = req.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&path)
            .to_string()
    });

    // Read spino.md if present
    let spino_path = p.join("spino.md");
    let spino_md = if spino_path.is_file() {
        fs::read_to_string(&spino_path).unwrap_or_default()
    } else {
        String::new()
    };

    with_db(&state, |db| {
        db.execute(
            r#"
            INSERT INTO projects (path, name, spino_md, created_at, last_opened_at)
            VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(path) DO UPDATE SET name = ?2, spino_md = ?3, last_opened_at = CURRENT_TIMESTAMP
            "#,
            params![path, name, spino_md],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    with_db(&state, |db| {
        db.query_row(
            "SELECT path, name, spino_md, created_at, last_opened_at FROM projects WHERE path = ?1",
            params![path],
            |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    spino_md: row.get(2)?,
                    created_at: row.get(3)?,
                    last_opened_at: row.get(4)?,
                })
            },
        )
        .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub fn remove_project(state: State<AppState>, req: ProjectPathRequest) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute("DELETE FROM projects WHERE path = ?1", params![req.path])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn open_project(state: State<AppState>, req: ProjectPathRequest) -> Result<ProjectRecord, String> {
    let path = req.path.trim().to_string();
    let p = std::path::Path::new(&path);

    // Re-read spino.md from disk
    let spino_path = p.join("spino.md");
    let spino_md = if spino_path.is_file() {
        fs::read_to_string(&spino_path).unwrap_or_default()
    } else {
        String::new()
    };

    with_db(&state, |db| {
        db.execute(
            "UPDATE projects SET last_opened_at = CURRENT_TIMESTAMP, spino_md = ?2 WHERE path = ?1",
            params![path, spino_md],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    with_db(&state, |db| {
        db.query_row(
            "SELECT path, name, spino_md, created_at, last_opened_at FROM projects WHERE path = ?1",
            params![path],
            |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    spino_md: row.get(2)?,
                    created_at: row.get(3)?,
                    last_opened_at: row.get(4)?,
                })
            },
        )
        .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub async fn init_project(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ProjectInitRequest,
) -> Result<String, String> {
    let path = req.path.trim().to_string();
    let p = std::path::Path::new(&path);
    if !p.is_dir() {
        return Err(format!("directory does not exist: {}", path));
    }

    let scope = WorkspaceScope::from_path(&path)?;

    // Create a temporary session for streaming events
    let session_id = uuid::Uuid::new_v4().to_string();

    let prompt = format!(
        "You are analyzing the project at: {path}\n\n\
        Use the provided tools to explore the project thoroughly:\n\
        - Use list_files to understand the directory structure (check subdirectories too)\n\
        - Use read_file to read key files (README, config files, entry points, important source files)\n\
        - Use run_command to check git log, dependency info, etc.\n\
        - Use search_files to find patterns if needed\n\n\
        After exploring, generate a concise spino.md file (under 100 lines, markdown) that describes:\n\
        1. What the project is (name, purpose)\n\
        2. Tech stack and key dependencies\n\
        3. Project structure overview\n\
        4. How to build/run/test\n\
        5. Key files and their purposes\n\n\
        Output the final spino.md content as your last message, with no preamble like \"Here is the spino.md\"."
    );

    // Init tools — read-only exploration only
    let init_tools: std::collections::HashSet<String> = [
        "read_file", "list_files", "search_files", "run_command",
    ].iter().map(|s| s.to_string()).collect();

    let tools_json = vec![
        serde_json::json!({
            "name": "run_command",
            "description": "Run a shell command in the project directory and return output.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string"}
                },
                "required": ["command"]
            }
        }),
        serde_json::json!({
            "name": "read_file",
            "description": "Read a file within the project.",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "list_files",
            "description": "List files in a project directory.",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "search_files",
            "description": "Search text recursively using ripgrep.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "pattern": {"type": "string"}
                },
                "required": ["path", "pattern"]
            }
        }),
    ];

    // Determine provider/model
    let configured_provider = get_app_setting_value(&state, "llm/provider")?.unwrap_or_else(|| "minimax".to_string());
    let model = get_app_setting_value(&state, "llm/model")?
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| if configured_provider == "minimax" { "MiniMax-M2.7".to_string() } else { "gpt-4.1-mini".to_string() });

    let is_anthropic = configured_provider == "minimax" || configured_provider == "anthropic" || model.starts_with("MiniMax-");

    let system_prompt = format!(
        "You are Spino, a project analyzer. You explore projects using tools and generate a spino.md description file. \
        The project root is: {path}. All file paths are relative to this root. Be thorough but efficient — \
        explore the structure, read key files, then write a concise summary."
    );

    let result = if is_anthropic {
        let api_key = first_non_empty_setting(&state, &["llm/minimax_api_key", "llm/anthropic_api_key"])?
            .or_else(|| std::env::var("MINIMAX_API_KEY").ok())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| "API key not configured".to_string())?;
        let base = std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimax.io/anthropic".to_string());
        let endpoint = if base.ends_with("/v1/messages") { base.clone() } else { format!("{}/v1/messages", base.trim_end_matches('/')) };

        let mut messages: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": prompt}]})
        ];

        let final_text;

        loop {
            let request_body = serde_json::json!({
                "model": model,
                "max_tokens": 4096,
                "system": system_prompt,
                "messages": messages,
                "tools": tools_json,
                "stream": true
            });

            let response = state.http.post(&endpoint)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&request_body)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let status = response.status();
            if !status.is_success() {
                let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
                return Err(format!("API error {}: {}", status, body));
            }

            // Parse SSE stream
            let mut content_acc = String::new();
            let mut tool_uses: Vec<(String, String, String)> = Vec::new();
            let mut current_event_type = String::new();
            let mut line_buf = String::new();

            let mut stream = response;
            while let Some(bytes) = stream.chunk().await.map_err(|e| e.to_string())? {
                let text = String::from_utf8_lossy(&bytes);
                line_buf.push_str(&text);

                while let Some(newline_pos) = line_buf.find('\n') {
                    let line = line_buf[..newline_pos].trim().to_string();
                    line_buf = line_buf[newline_pos + 1..].to_string();

                    if line.starts_with("event: ") {
                        current_event_type = line[7..].to_string();
                        continue;
                    }
                    if line.is_empty() || !line.starts_with("data: ") { continue; }
                    let data: serde_json::Value = match serde_json::from_str(&line[6..]) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    match current_event_type.as_str() {
                        "content_block_start" => {
                            if let Some(block) = data.get("content_block") {
                                if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    tool_uses.push((id, name, String::new()));
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = data.get("delta") {
                                let dt = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                if dt == "text_delta" {
                                    if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                        content_acc.push_str(t);
                                        let _ = app.emit("chat-token", ChatTokenEvent {
                                            session_id: session_id.clone(),
                                            token: t.to_string(),
                                        });
                                    }
                                } else if dt == "input_json_delta" {
                                    if let Some(chunk) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                        if let Some(last) = tool_uses.last_mut() {
                                            last.2.push_str(chunk);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            // If tool calls, execute them in parallel and continue the loop
            if !tool_uses.is_empty() {
                let mut content_blocks: Vec<serde_json::Value> = Vec::new();
                if !content_acc.is_empty() {
                    content_blocks.push(serde_json::json!({"type": "text", "text": content_acc}));
                }
                for (id, name, input_str) in &tool_uses {
                    let input: serde_json::Value = serde_json::from_str(input_str).unwrap_or_else(|_| serde_json::json!({}));
                    content_blocks.push(serde_json::json!({
                        "type": "tool_use", "id": id, "name": name, "input": input
                    }));
                }
                messages.push(serde_json::json!({"role": "assistant", "content": content_blocks}));

                // Emit "running" for all tools first
                for (_, tool_name, input_str) in &tool_uses {
                    let args: serde_json::Value = serde_json::from_str(input_str).unwrap_or_else(|_| serde_json::json!({}));
                    let _ = app.emit("tool-executed", serde_json::json!({
                        "session_id": session_id,
                        "tool": tool_name,
                        "args": args.to_string(),
                        "status": "running"
                    }));
                }

                // Execute all tools in parallel
                let futures: Vec<_> = tool_uses.iter().map(|(_, tool_name, input_str)| {
                    let args: serde_json::Value = serde_json::from_str(input_str).unwrap_or_else(|_| serde_json::json!({}));
                    let app = app.clone();
                    let state_ref = &state;
                    let session_id = session_id.clone();
                    let init_tools = init_tools.clone();
                    let scope = scope.clone();
                    let tool_name = tool_name.clone();
                    async move {
                        let output = match execute_agent_tool(&app, state_ref, &session_id, &init_tools, &tool_name, &args, Some(&scope)).await {
                            Ok(o) => {
                                let _ = app.emit("tool-executed", serde_json::json!({
                                    "session_id": session_id,
                                    "tool": tool_name,
                                    "args": args.to_string(),
                                    "status": "ok"
                                }));
                                o
                            }
                            Err(e) => {
                                let _ = app.emit("tool-executed", serde_json::json!({
                                    "session_id": session_id,
                                    "tool": tool_name,
                                    "args": args.to_string(),
                                    "status": "error"
                                }));
                                format!("Tool error: {}", e)
                            }
                        };
                        output
                    }
                }).collect();

                let results = futures::future::join_all(futures).await;

                for (i, output) in results.into_iter().enumerate() {
                    let tool_id = &tool_uses[i].0;
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": output}]
                    }));
                }
                continue;
            }

            // No tool calls — this is the final response
            final_text = content_acc;
            break;
        }

        let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.clone() });
        final_text
    } else {
        // OpenAI-compatible path — same streaming tool loop
        let api_key = first_non_empty_setting(&state, &["llm/openai_api_key"])?
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| "OpenAI API key not configured".to_string())?;

        let openai_tools: Vec<serde_json::Value> = tools_json.iter().map(|t| serde_json::json!({
            "type": "function",
            "function": {
                "name": t["name"],
                "description": t["description"],
                "parameters": t["input_schema"]
            }
        })).collect();

        let mut messages: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "system", "content": system_prompt}),
            serde_json::json!({"role": "user", "content": prompt}),
        ];

        let final_text;

        loop {
            let request_body = serde_json::json!({
                "model": model,
                "messages": messages,
                "tools": openai_tools,
                "stream": true
            });

            let response = state.http.post("https://api.openai.com/v1/chat/completions")
                .bearer_auth(&api_key)
                .json(&request_body)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let status = response.status();
            if !status.is_success() {
                let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
                return Err(format!("API error {}: {}", status, body));
            }

            let mut content_acc = String::new();
            let mut tool_calls: Vec<(String, String, String)> = Vec::new(); // (id, name, args)
            let mut line_buf = String::new();

            let mut stream = response;
            while let Some(bytes) = stream.chunk().await.map_err(|e| e.to_string())? {
                let text = String::from_utf8_lossy(&bytes);
                line_buf.push_str(&text);

                while let Some(newline_pos) = line_buf.find('\n') {
                    let line = line_buf[..newline_pos].trim().to_string();
                    line_buf = line_buf[newline_pos + 1..].to_string();

                    if !line.starts_with("data: ") || line == "data: [DONE]" { continue; }
                    let data: serde_json::Value = match serde_json::from_str(&line[6..]) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
                        for choice in choices {
                            let delta = match choice.get("delta") {
                                Some(d) => d,
                                None => continue,
                            };
                            if let Some(t) = delta.get("content").and_then(|v| v.as_str()) {
                                content_acc.push_str(t);
                                let _ = app.emit("chat-token", ChatTokenEvent {
                                    session_id: session_id.clone(),
                                    token: t.to_string(),
                                });
                            }
                            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                                for tc in tcs {
                                    let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                                    while tool_calls.len() <= idx {
                                        tool_calls.push((String::new(), String::new(), String::new()));
                                    }
                                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                        tool_calls[idx].0 = id.to_string();
                                    }
                                    if let Some(f) = tc.get("function") {
                                        if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                                            tool_calls[idx].1 = name.to_string();
                                        }
                                        if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                                            tool_calls[idx].2.push_str(args);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !tool_calls.is_empty() {
                let tc_json: Vec<serde_json::Value> = tool_calls.iter().map(|(id, name, args)| {
                    serde_json::json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
                }).collect();
                let mut assistant_msg = serde_json::json!({"role": "assistant", "tool_calls": tc_json});
                if !content_acc.is_empty() {
                    assistant_msg["content"] = serde_json::json!(content_acc);
                }
                messages.push(assistant_msg);

                // Emit "running" for all tools first
                for (_, tool_name, args_str) in &tool_calls {
                    let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or_else(|_| serde_json::json!({}));
                    let _ = app.emit("tool-executed", serde_json::json!({
                        "session_id": session_id,
                        "tool": tool_name,
                        "args": args.to_string(),
                        "status": "running"
                    }));
                }

                // Execute all tools in parallel
                let futures: Vec<_> = tool_calls.iter().map(|(_, tool_name, args_str)| {
                    let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or_else(|_| serde_json::json!({}));
                    let app = app.clone();
                    let state_ref = &state;
                    let session_id = session_id.clone();
                    let init_tools = init_tools.clone();
                    let scope = scope.clone();
                    let tool_name = tool_name.clone();
                    async move {
                        let output = match execute_agent_tool(&app, state_ref, &session_id, &init_tools, &tool_name, &args, Some(&scope)).await {
                            Ok(o) => {
                                let _ = app.emit("tool-executed", serde_json::json!({
                                    "session_id": session_id,
                                    "tool": tool_name,
                                    "args": args.to_string(),
                                    "status": "ok"
                                }));
                                o
                            }
                            Err(e) => {
                                let _ = app.emit("tool-executed", serde_json::json!({
                                    "session_id": session_id,
                                    "tool": tool_name,
                                    "args": args.to_string(),
                                    "status": "error"
                                }));
                                format!("Tool error: {}", e)
                            }
                        };
                        output
                    }
                }).collect();

                let results = futures::future::join_all(futures).await;

                for (i, output) in results.into_iter().enumerate() {
                    let tool_id = &tool_calls[i].0;
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": tool_id,
                        "content": output
                    }));
                }
                continue;
            }

            final_text = content_acc;
            break;
        }

        let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.clone() });
        final_text
    };

    if result.is_empty() {
        return Err("Init completed but no content was generated".to_string());
    }

    // Write spino.md to project directory
    let spino_path = p.join("spino.md");
    fs::write(&spino_path, &result).map_err(|e| format!("Failed to write spino.md: {}", e))?;

    // Update projects table cache
    with_db(&state, |db| {
        db.execute(
            "UPDATE projects SET spino_md = ?2 WHERE path = ?1",
            params![path, result],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    Ok(result)
}

#[tauri::command]
pub async fn transcribe_whisper(
    state: State<'_, AppState>,
    req: TranscribeRequest,
) -> Result<TranscribeResponse, String> {
    let api_key = get_app_setting_value(&state, "llm/openai_api_key")?
        .or_else(|| get_app_setting_value(&state, "llm/minimax_api_key").ok().flatten())
        .ok_or_else(|| "OpenAI API key not configured (set llm/openai_api_key in settings)".to_string())?;

    let audio_bytes = B64.decode(&req.audio_base64).map_err(|e| format!("base64 decode error: {e}"))?;

    let part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("audio.webm")
        .mime_str("audio/webm")
        .map_err(|e| e.to_string())?;

    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);

    let resp = state
        .http
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(&api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("whisper request failed: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("whisper API error: {body}"));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let text = json["text"].as_str().unwrap_or("").to_string();
    Ok(TranscribeResponse { text })
}

#[tauri::command]
pub async fn tts_speak(
    state: State<'_, AppState>,
    req: TtsRequest,
) -> Result<TtsResponse, String> {
    match req.provider.as_str() {
        "kokoro" => {
            let body = serde_json::json!({
                "model": "kokoro",
                "input": req.text,
                "voice": "af_heart"
            });
            let resp = state
                .http
                .post("http://127.0.0.1:8003/v1/audio/speech")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("kokoro request failed: {e}"))?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("kokoro error: {body}"));
            }
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("audio/mp3")
                .to_string();
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok(TtsResponse {
                audio_base64: B64.encode(&bytes),
                content_type,
            })
        }
        "fishspeech" => {
            let body = serde_json::json!({
                "model": "fishaudio",
                "input": req.text,
                "voice": "default"
            });
            let resp = state
                .http
                .post("http://127.0.0.1:8080/v1/audio/speech")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("fishspeech request failed: {e}"))?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("fishspeech error: {body}"));
            }
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("audio/mp3")
                .to_string();
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok(TtsResponse {
                audio_base64: B64.encode(&bytes),
                content_type,
            })
        }
        "system" => {
            let text = req.text.clone();
            std::thread::spawn(move || {
                let _ = std::process::Command::new("say").arg(&text).status();
            });
            Ok(TtsResponse {
                audio_base64: String::new(),
                content_type: String::new(),
            })
        }
        _ => Err(format!("unknown TTS provider: {}", req.provider)),
    }
}

#[tauri::command]
pub fn start_voice_recording(state: State<AppState>, app: AppHandle) -> Result<(), String> {
    if state.audio.is_recording() {
        return Err("Already recording".to_string());
    }
    state.audio.start(app);
    Ok(())
}

#[tauri::command]
pub fn stop_voice_recording(state: State<AppState>) -> Result<(), String> {
    state.audio.stop();
    Ok(())
}

#[tauri::command]
pub async fn transcribe_wav(
    state: State<'_, AppState>,
    wav_base64: String,
) -> Result<TranscribeResponse, String> {
    let api_key = get_app_setting_value(&state, "llm/openai_api_key")?
        .or_else(|| get_app_setting_value(&state, "llm/minimax_api_key").ok().flatten())
        .ok_or_else(|| "OpenAI API key not configured".to_string())?;

    let audio_bytes = B64.decode(&wav_base64).map_err(|e| format!("base64 decode error: {e}"))?;

    let part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;

    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);

    let resp = state
        .http
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(&api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("whisper request failed: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("whisper API error: {body}"));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let text = json["text"].as_str().unwrap_or("").to_string();
    Ok(TranscribeResponse { text })
}

#[tauri::command]
pub fn read_image_base64(path: String) -> Result<String, String> {
    let bytes = fs::read(&path).map_err(|e| format!("Failed to read image: {}", e))?;
    let b64 = B64.encode(&bytes);
    let ext = path.rsplit('.').next().unwrap_or("png").to_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    Ok(format!("data:{};base64,{}", mime, b64))
}

// ==================== AI FEEDS ====================

#[tauri::command]
pub fn get_ai_feeds(state: State<AppState>) -> Result<Vec<AiFeedRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT id, name, topics_json, refresh_interval_mins, last_refreshed_at, created_at \
                 FROM ai_feeds ORDER BY id ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, topics_json, interval, last_refreshed, created_at) =
                row.map_err(|e| e.to_string())?;
            let topics: Vec<String> = serde_json::from_str(&topics_json).unwrap_or_default();
            out.push(AiFeedRecord {
                id,
                name,
                topics,
                refresh_interval_mins: interval,
                last_refreshed_at: last_refreshed,
                created_at,
            });
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn create_ai_feed(state: State<AppState>, req: AiFeedCreateRequest) -> Result<AiFeedRecord, String> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("Feed name is required.".to_string());
    }
    let topics_json = serde_json::to_string(&req.topics).map_err(|e| e.to_string())?;
    let interval = req.refresh_interval_mins.unwrap_or(60).max(1);

    with_db(&state, |db| {
        db.execute(
            "INSERT INTO ai_feeds (name, topics_json, refresh_interval_mins) VALUES (?1, ?2, ?3)",
            params![name, topics_json, interval],
        )
        .map_err(|e| e.to_string())?;
        let id = db.last_insert_rowid();
        let (last_refreshed, created_at) = db
            .query_row(
                "SELECT last_refreshed_at, created_at FROM ai_feeds WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| e.to_string())?;
        Ok(AiFeedRecord {
            id,
            name: name.clone(),
            topics: req.topics.clone(),
            refresh_interval_mins: interval,
            last_refreshed_at: last_refreshed,
            created_at,
        })
    })
}

#[tauri::command]
pub fn delete_ai_feed(state: State<AppState>, req: AiFeedIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM ai_feeds WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn get_ai_feed_items(state: State<AppState>, req: AiFeedIdRequest) -> Result<Vec<AiFeedItemRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT id, feed_id, title, summary, url, source_name, is_read, created_at \
                 FROM ai_feed_items WHERE feed_id = ?1 ORDER BY id DESC LIMIT 200",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![req.id], |row| {
                Ok(AiFeedItemRecord {
                    id: row.get(0)?,
                    feed_id: row.get(1)?,
                    title: row.get(2)?,
                    summary: row.get(3)?,
                    url: row.get(4)?,
                    source_name: row.get(5)?,
                    is_read: row.get::<_, i64>(6)? == 1,
                    created_at: row.get(7)?,
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
pub fn mark_ai_feed_item_read(state: State<AppState>, req: AiFeedItemIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("UPDATE ai_feed_items SET is_read = 1 WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub async fn refresh_ai_feed(
    state: State<'_, AppState>,
    req: AiFeedIdRequest,
) -> Result<AiFeedRefreshResult, String> {
    let items_added = crate::feeds::refresh_feed_impl(state.inner(), req.id).await?;
    Ok(AiFeedRefreshResult { items_added })
}
