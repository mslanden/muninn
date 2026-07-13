//! Scheduled task CRUD; execution lives in scheduler.rs.

use rusqlite::{params, OptionalExtension};
use tauri::{Emitter, State};


use crate::AppState;
use crate::db::*;
use crate::models::*;

/// Statuses a user (or agent) may set directly. 'running' and 'failed' are
/// engine-managed: 'running' is only set by the atomic claim in
/// `scheduler::execute_task`, 'failed' only by an errored schedule-less run.
pub(crate) fn validate_user_task_status(status: &str) -> Result<String, String> {
    let normalized = status.trim().to_lowercase();
    if ["scheduled", "completed", "paused"].contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err("status must be scheduled, completed, or paused (running/failed are engine-managed)".to_string())
    }
}

#[tauri::command]
pub fn get_tasks(state: State<AppState>) -> Result<Vec<TaskRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"
                SELECT id, name, description, schedule, schedule_config, next_run, status,
                       completed_at, result, run_count, triggered_by,
                       chain_next, conditions, condition_status,
                       project_path, agent_id,
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
                    schedule_config: row.get(4)?,
                    next_run: row.get(5)?,
                    status: row.get(6)?,
                    completed_at: row.get(7)?,
                    result: row.get(8)?,
                    run_count: row.get(9)?,
                    triggered_by: row.get(10)?,
                    chain_next: row.get(11)?,
                    conditions: row.get(12)?,
                    condition_status: row.get(13)?,
                    project_path: row.get(14)?,
                    agent_id: row.get(15)?,
                    created_at: row.get(16)?,
                    updated_at: row.get(17)?,
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
    let schedule_config = req.schedule_config.unwrap_or_default();
    let mut next_run = req.next_run.unwrap_or_default();

    // Auto-compute next_run from schedule_config or legacy schedule
    if next_run.trim().is_empty() {
        let now = chrono::Local::now().naive_local();
        if !schedule_config.trim().is_empty() {
            if let Some(computed) = crate::scheduler::next_run_from_schedule_config(&schedule_config, now) {
                next_run = computed.format("%Y-%m-%dT%H:%M:%S").to_string();
            }
        } else if !schedule.trim().is_empty() {
            if let Some(computed) = crate::scheduler::next_run_from_schedule(&schedule, now) {
                next_run = computed.format("%Y-%m-%dT%H:%M:%S").to_string();
            }
        }
    }

    let project_path = req.project_path.unwrap_or_default().trim().to_string();
    let agent_id = req.agent_id.unwrap_or_default().trim().to_string();

    with_db(&state, |db| {
        db.execute(
            r#"
            INSERT INTO tasks (name, description, schedule, schedule_config, next_run, chain_next, conditions, project_path, agent_id, status, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'scheduled', CURRENT_TIMESTAMP)
            "#,
            params![
                name,
                req.description.unwrap_or_default(),
                schedule,
                schedule_config,
                next_run,
                req.chain_next.unwrap_or_default(),
                req.conditions.unwrap_or_default(),
                project_path,
                agent_id
            ],
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
    let schedule_config = req.schedule_config.unwrap_or_default();
    let mut next_run = req.next_run.unwrap_or_default();

    // Auto-compute next_run from schedule_config or legacy schedule
    if next_run.trim().is_empty() {
        let now = chrono::Local::now().naive_local();
        if !schedule_config.trim().is_empty() {
            if let Some(computed) = crate::scheduler::next_run_from_schedule_config(&schedule_config, now) {
                next_run = computed.format("%Y-%m-%dT%H:%M:%S").to_string();
            }
        } else if !schedule.trim().is_empty() {
            if let Some(computed) = crate::scheduler::next_run_from_schedule(&schedule, now) {
                next_run = computed.format("%Y-%m-%dT%H:%M:%S").to_string();
            }
        }
    }

    let project_path = req.project_path.unwrap_or_default().trim().to_string();
    let agent_id = req.agent_id.unwrap_or_default().trim().to_string();

    with_db(&state, |db| {
        let changed = db
            .execute(
                r#"
                UPDATE tasks
                SET name = ?2,
                    description = ?3,
                    schedule = ?4,
                    schedule_config = ?5,
                    next_run = ?6,
                    chain_next = ?7,
                    conditions = ?8,
                    project_path = ?9,
                    agent_id = ?10,
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?1
                "#,
                params![
                    req.id,
                    name,
                    req.description.unwrap_or_default(),
                    schedule,
                    schedule_config,
                    next_run,
                    req.chain_next.unwrap_or_default(),
                    req.conditions.unwrap_or_default(),
                    project_path,
                    agent_id
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn set_task_status(
    app: tauri::AppHandle,
    state: State<AppState>,
    req: TaskStatusRequest,
) -> Result<bool, String> {
    let normalized = validate_user_task_status(&req.status)?;

    let changed = with_db(&state, |db| {
        let row: Option<(String, String, String)> = db
            .query_row(
                "SELECT schedule, schedule_config, next_run FROM tasks WHERE id = ?1",
                params![req.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let (schedule, schedule_config, mut next_run) = match row {
            Some(t) => t,
            None => return Ok(0),
        };

        // Resuming (paused/failed/completed → scheduled) with no next_run would leave the
        // task permanently un-runnable; recompute from schedule_config (preferred) or schedule.
        if normalized == "scheduled" && next_run.trim().is_empty() {
            let now = chrono::Local::now().naive_local();
            next_run = if !schedule_config.trim().is_empty() {
                crate::scheduler::next_run_from_schedule_config(&schedule_config, now)
            } else if !schedule.trim().is_empty() {
                crate::scheduler::next_run_from_schedule(&schedule, now)
            } else {
                None
            }
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();
        }

        db.execute(
            r#"
            UPDATE tasks
            SET status = ?2, next_run = ?3, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![req.id, normalized, next_run],
        )
        .map_err(|e| e.to_string())
    })?;

    let _ = app.emit("tasks-changed", ());
    Ok(changed > 0)
}

/// Fire a task immediately, regardless of its schedule. Errors if the task
/// doesn't exist or is already running; otherwise spawns the run in the
/// background and returns right away (the UI tracks progress via the
/// `tasks-changed` events the run emits). The recorded task_run has
/// trigger_kind = "manual". Note: like scheduled runs, a successful manual run
/// of a schedule-less task marks it 'completed' (re-fire it with run_task /
/// Run now whenever needed).
#[tauri::command]
pub fn run_task_now(
    app: tauri::AppHandle,
    state: State<AppState>,
    req: TaskIdRequest,
) -> Result<bool, String> {
    let status: Option<String> = with_db(&state, |db| {
        db.query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            params![req.id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    })?;
    let status = status.ok_or_else(|| format!("no task with id {}", req.id))?;
    if status == "running" {
        return Err("task is already running".to_string());
    }

    let id = req.id;
    tauri::async_runtime::spawn(async move {
        if let Err(e) = crate::scheduler::execute_task(app, id, true).await {
            eprintln!("[Tasks] manual run of task {} not started: {}", id, e);
        }
    });
    Ok(true)
}

#[tauri::command]
pub fn get_task_runs(state: State<AppState>, task_id: i64) -> Result<Vec<TaskRun>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"
                SELECT id, task_id, session_id, trigger_kind, triggered_by,
                       status, output, note, started_at, finished_at
                FROM task_runs
                WHERE task_id = ?1
                ORDER BY id DESC
                LIMIT 50
                "#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![task_id], |row| {
                Ok(TaskRun {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    session_id: row.get(2)?,
                    trigger_kind: row.get(3)?,
                    triggered_by: row.get(4)?,
                    status: row.get(5)?,
                    output: row.get(6)?,
                    note: row.get(7)?,
                    started_at: row.get(8)?,
                    finished_at: row.get(9)?,
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
pub fn remove_task(state: State<AppState>, req: TaskIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM tasks WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_settable_statuses_are_validated() {
        // User-settable states pass (and normalize).
        assert_eq!(validate_user_task_status("scheduled").unwrap(), "scheduled");
        assert_eq!(validate_user_task_status("  Completed ").unwrap(), "completed");
        assert_eq!(validate_user_task_status("PAUSED").unwrap(), "paused");
        // Engine-managed states are rejected.
        assert!(validate_user_task_status("running").is_err());
        assert!(validate_user_task_status("failed").is_err());
        assert!(validate_user_task_status("bogus").is_err());
        assert!(validate_user_task_status("").is_err());
    }
}
