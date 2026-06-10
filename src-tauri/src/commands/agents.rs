//! Custom agent CRUD (system prompt, slash command, allowed tools).

use rusqlite::params;
use tauri::State;
use uuid::Uuid;


use crate::AppState;
use crate::db::*;
use crate::models::*;

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
