//! Tool enable/disable configuration surfaced in the Tools panel.

use rusqlite::params;
use tauri::State;


use crate::AppState;
use crate::db::*;
use crate::models::*;

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
