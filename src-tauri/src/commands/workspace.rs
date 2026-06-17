//! Workspace root management, saved workspaces, scoped file ops, command safety check.

use std::path::PathBuf;
use tauri::State;


use crate::AppState;
use crate::models::*;
use crate::tools::*;

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

    Ok(PathResult {
        workspace_root: root.display().to_string(),
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
