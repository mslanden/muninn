//! Skills: the SKILL.md folders (see skills.rs), exposed to the Skills tile for
//! browsing and hand-editing. User edits here need no approval — only the model's
//! `skill` tool writes are gated.

use tauri::{AppHandle, Emitter, State};

use crate::skills::{self, Skill, SkillMeta};
use crate::AppState;

#[tauri::command]
pub fn list_skills(state: State<AppState>) -> Vec<SkillMeta> {
    skills::list_skills(&state.skills_dir)
}

#[tauri::command]
pub fn read_skill(state: State<AppState>, name: String) -> Result<Skill, String> {
    skills::load_skill(&state.skills_dir, &name)
}

/// Save a skill from the editor. When `previous_name` differs from `name` the skill
/// folder is renamed (reference files move with it). Returns the sanitized name.
#[tauri::command]
pub fn save_skill(
    app: AppHandle,
    state: State<AppState>,
    name: String,
    description: String,
    body: String,
    allowed_tools: Vec<String>,
    previous_name: Option<String>,
) -> Result<String, String> {
    let dir = &state.skills_dir;
    let target = crate::memory::sanitize_name(&name)?;
    if let Some(prev) = previous_name.filter(|p| !p.trim().is_empty()) {
        let prev = crate::memory::sanitize_name(&prev)?;
        if prev != target && dir.join(&prev).is_dir() {
            if dir.join(&target).exists() {
                return Err(format!("a skill named '{}' already exists", target));
            }
            std::fs::rename(dir.join(&prev), dir.join(&target)).map_err(|e| e.to_string())?;
        }
    }
    let saved = skills::write_skill(dir, &target, &description, &body, &allowed_tools)?;
    let _ = app.emit("skills-changed", ());
    Ok(saved)
}

#[tauri::command]
pub fn delete_skill(app: AppHandle, state: State<AppState>, name: String) -> Result<(), String> {
    skills::delete_skill(&state.skills_dir, &name)?;
    let _ = app.emit("skills-changed", ());
    Ok(())
}

/// Reveal the skills folder in Finder (creating it first so there's something to open).
#[tauri::command]
pub fn open_skills_folder(state: State<AppState>) -> Result<(), String> {
    std::fs::create_dir_all(&state.skills_dir).map_err(|e| e.to_string())?;
    std::process::Command::new("open")
        .arg(&state.skills_dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}
