//! Project CRUD and the init_project onboarding flow.

use rusqlite::params;
use std::fs;
use tauri::{AppHandle, State};


use crate::AppState;
use crate::db::*;
use crate::models::*;
use crate::tools::*;

#[tauri::command]
pub fn get_projects(state: State<AppState>) -> Result<Vec<ProjectRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare("SELECT path, name, agents_md, created_at, last_opened_at FROM projects ORDER BY last_opened_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    agents_md: row.get(2)?,
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

    // One-time migration: if AGENTS.md doesn't exist but a legacy spino.md does, rename it.
    let agents_path = p.join("AGENTS.md");
    let legacy_path = p.join("spino.md");
    if !agents_path.is_file() && legacy_path.is_file() {
        let _ = fs::rename(&legacy_path, &agents_path);
    }

    // Read AGENTS.md if present
    let agents_md = if agents_path.is_file() {
        fs::read_to_string(&agents_path).unwrap_or_default()
    } else {
        String::new()
    };

    with_db(&state, |db| {
        db.execute(
            r#"
            INSERT INTO projects (path, name, agents_md, created_at, last_opened_at)
            VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(path) DO UPDATE SET name = ?2, agents_md = ?3, last_opened_at = CURRENT_TIMESTAMP
            "#,
            params![path, name, agents_md],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    with_db(&state, |db| {
        db.query_row(
            "SELECT path, name, agents_md, created_at, last_opened_at FROM projects WHERE path = ?1",
            params![path],
            |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    agents_md: row.get(2)?,
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

    // One-time migration: if AGENTS.md doesn't exist but a legacy spino.md does, rename it.
    let agents_path = p.join("AGENTS.md");
    let legacy_path = p.join("spino.md");
    if !agents_path.is_file() && legacy_path.is_file() {
        let _ = fs::rename(&legacy_path, &agents_path);
    }

    // Re-read AGENTS.md from disk
    let agents_md = if agents_path.is_file() {
        fs::read_to_string(&agents_path).unwrap_or_default()
    } else {
        String::new()
    };

    with_db(&state, |db| {
        db.execute(
            "UPDATE projects SET last_opened_at = CURRENT_TIMESTAMP, agents_md = ?2 WHERE path = ?1",
            params![path, agents_md],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    with_db(&state, |db| {
        db.query_row(
            "SELECT path, name, agents_md, created_at, last_opened_at FROM projects WHERE path = ?1",
            params![path],
            |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    agents_md: row.get(2)?,
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

    // A throwaway session id; analyzer events stream to the active tile if one matches.
    let session_id = uuid::Uuid::new_v4().to_string();

    let result = crate::llm::run_project_init(&app, &state, &session_id, &path, &scope).await?;

    if result.is_empty() {
        return Err("Init completed but no content was generated".to_string());
    }

    // Write AGENTS.md to project directory
    let agents_path = p.join("AGENTS.md");
    fs::write(&agents_path, &result).map_err(|e| format!("Failed to write AGENTS.md: {}", e))?;

    // Update projects table cache
    with_db(&state, |db| {
        db.execute(
            "UPDATE projects SET agents_md = ?2 WHERE path = ?1",
            params![path, result],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    Ok(result)
}
