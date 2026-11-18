//! Per-tool on/off switch, used by the Connections window when a group is expanded.

use rusqlite::params;
use tauri::State;


use crate::AppState;
use crate::db::*;
use crate::models::*;

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
