//! Scheduled task CRUD; execution lives in scheduler.rs.

use rusqlite::params;
use tauri::State;


use crate::AppState;
use crate::db::*;
use crate::models::*;

#[tauri::command]
pub fn get_tasks(state: State<AppState>) -> Result<Vec<TaskRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"
                SELECT id, name, description, schedule, schedule_config, next_run, status,
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
                    created_at: row.get(14)?,
                    updated_at: row.get(15)?,
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

    with_db(&state, |db| {
        db.execute(
            r#"
            INSERT INTO tasks (name, description, schedule, schedule_config, next_run, status, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, 'scheduled', CURRENT_TIMESTAMP)
            "#,
            params![name, req.description.unwrap_or_default(), schedule, schedule_config, next_run],
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
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?1
                "#,
                params![req.id, name, req.description.unwrap_or_default(), schedule, schedule_config, next_run],
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
