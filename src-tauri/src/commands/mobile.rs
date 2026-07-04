//! Desktop-only commands controlling the embedded mobile companion server.
//!
//! These are invoked exclusively by the trusted desktop Settings UI. `get_mobile_server_status`
//! returns the raw token by design (same trust domain as the UI that already handles it) — it
//! deliberately bypasses `get_app_settings`, which would redact the `mobile/server_token` key.

use tauri::AppHandle;

/// Start the companion server; returns the bound address (e.g. "0.0.0.0:8737").
#[tauri::command]
pub fn start_mobile_server(app: AppHandle) -> Result<String, String> {
    crate::mobile::start(app)
}

/// Stop the companion server. Returns whether a running server was stopped.
#[tauri::command]
pub fn stop_mobile_server() -> bool {
    crate::mobile::stop()
}

/// Full status incl. token + LAN URLs. Trusted desktop caller only.
#[tauri::command]
pub fn get_mobile_server_status(app: AppHandle) -> serde_json::Value {
    crate::mobile::status(&app)
}

/// Generate and persist a new token (restarts the server if running). Returns the new token.
#[tauri::command]
pub fn regenerate_mobile_token(app: AppHandle) -> Result<String, String> {
    crate::mobile::regenerate_token(&app)
}
