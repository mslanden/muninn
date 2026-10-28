//! Memory: the model-managed notes folder (see memory.rs), exposed to the
//! Memory tile for browsing and hand-editing.

use tauri::{AppHandle, Emitter, State};

use crate::memory::{self, Note, NoteMeta};
use crate::AppState;

#[tauri::command]
pub fn memory_list_notes(state: State<AppState>) -> Vec<NoteMeta> {
    memory::list_notes(&state.memory_dir)
}

#[tauri::command]
pub fn memory_read_note(state: State<AppState>, name: String) -> Result<Note, String> {
    memory::read_note(&state.memory_dir, &name)
}

/// Save a note from the editor. When `previous_name` differs from `name` the note
/// is renamed (written under the new name, then the old file removed). Returns the
/// sanitized name actually written.
#[tauri::command]
pub fn memory_save_note(
    app: AppHandle,
    state: State<AppState>,
    name: String,
    description: String,
    body: String,
    previous_name: Option<String>,
) -> Result<String, String> {
    let saved = memory::write_note(&state.memory_dir, &name, &description, &body)?;
    if let Some(prev) = previous_name.filter(|p| !p.trim().is_empty()) {
        if memory::sanitize_name(&prev).ok().as_deref() != Some(saved.as_str()) {
            let _ = memory::delete_note(&state.memory_dir, &prev);
        }
    }
    let _ = app.emit("memory-changed", ());
    Ok(saved)
}

#[tauri::command]
pub fn memory_delete_note(app: AppHandle, state: State<AppState>, name: String) -> Result<(), String> {
    memory::delete_note(&state.memory_dir, &name)?;
    let _ = app.emit("memory-changed", ());
    Ok(())
}

/// Reveal the memory folder in Finder (creating it first so there's something to open).
#[tauri::command]
pub fn memory_open_folder(state: State<AppState>) -> Result<(), String> {
    std::fs::create_dir_all(&state.memory_dir).map_err(|e| e.to_string())?;
    std::process::Command::new("open")
        .arg(&state.memory_dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}
