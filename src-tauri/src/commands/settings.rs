//! App settings (app_config table) with secret redaction toward the webview.

use rusqlite::params;
use std::collections::HashMap;
use tauri::State;


use crate::AppState;
use crate::db::*;
use crate::models::*;

/// Sentinel returned in place of secret values so the frontend can tell a key is
/// configured without ever holding the actual secret in JS memory.
pub(crate) const SECRET_PLACEHOLDER: &str = "__secret_set__";

/// Settings keys whose values must never be sent to the webview. The Mapbox key is
/// deliberately excluded: it is a client-side token consumed by Mapbox GL JS in the
/// frontend, so withholding it would break the maps feature without a security gain.
fn is_secret_setting(key: &str) -> bool {
    (key.contains("api_key") || key.ends_with("_token") || key.ends_with("_secret"))
        && key != "maps/mapbox_api_key"
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
            if is_secret_setting(&key) && !value.is_empty() {
                out.insert(key, SECRET_PLACEHOLDER.to_string());
            } else {
                out.insert(key, value);
            }
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn set_app_settings(state: State<AppState>, req: SettingsUpdateRequest) -> Result<(), String> {
    with_db(&state, |db| {
        for (key, value) in req.settings {
            // Never write the redaction placeholder back over a real secret.
            if value == SECRET_PLACEHOLDER {
                continue;
            }
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
