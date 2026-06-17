//! Native desktop notifications for background events (scheduled tasks, feed
//! refreshes). Gated by the `notifications/enabled` setting (default on) so the
//! user can silence Muninn without disabling the scheduler.

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::db::get_app_setting_value;
use crate::AppState;

pub(crate) fn notify_user(app: &AppHandle, title: &str, body: &str) {
    let state = app.state::<AppState>();
    let enabled = get_app_setting_value(&state, "notifications/enabled")
        .ok()
        .flatten()
        .map(|v| v != "false")
        .unwrap_or(true);
    if !enabled {
        return;
    }
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        eprintln!("[Notify] failed to show notification: {}", e);
    }
}
