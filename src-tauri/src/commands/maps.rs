//! Map commands for the webview: map images for chat cards, opening Apple Maps, and
//! looking up the user's location. All Apple Maps (see `crate::maps`) — no key.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rusqlite::params;
use serde::Deserialize;
use tauri::State;

use crate::db::with_db;
use crate::maps::Marker;
use crate::AppState;

#[derive(Deserialize)]
pub struct MapSnapshotRequest {
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// [lat, lon] points of a route to draw.
    #[serde(default)]
    pub route: Vec<[f64; 2]>,
    #[serde(default)]
    pub dark: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Render an Apple Maps image of some places and/or a route; returns a PNG data URL. Called
/// by chat.js when it shows a map card (it knows the theme, so it picks light or dark).
#[tauri::command]
pub async fn map_snapshot(req: MapSnapshotRequest) -> Result<String, String> {
    if req.markers.is_empty() && req.route.len() < 2 {
        return Err("nothing to show on the map".to_string());
    }
    let markers: Vec<Marker> = req.markers.into_iter().take(30).collect();
    let route: Vec<[f64; 2]> = req.route.into_iter().take(2000).collect();
    let png = crate::maps::snapshot(
        &markers,
        &route,
        req.dark,
        req.width.unwrap_or(640).clamp(200, 1200),
        req.height.unwrap_or(320).clamp(150, 900),
    )
    .await?;
    Ok(format!("data:image/png;base64,{}", B64.encode(png)))
}

/// Open a `maps://` link (built by `crate::maps`) in the Apple Maps app.
#[tauri::command]
pub fn open_in_maps(url: String) -> Result<(), String> {
    if !url.starts_with("maps://?") || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("not an Apple Maps link".to_string());
    }
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|e| format!("could not open Apple Maps: {e}"))?;
    Ok(())
}

/// Look up the user's location with Apple Maps and store it as `user/location_coords`
/// ("lon,lat"), used to center place searches, plus `user/location_coords_for` (the text
/// it was looked up from) so Settings only looks it up again when the location changes.
#[tauri::command]
pub async fn geocode_user_location(state: State<'_, AppState>, location: String) -> Result<(), String> {
    let location = location.trim().to_string();
    if location.is_empty() {
        return Err("no location".to_string());
    }
    let place = crate::maps::geocode(&location, None).await?;
    let coords = format!("{},{}", place.lon, place.lat);
    with_db(&state, |db| {
        for (key, value) in [("user/location_coords", coords.as_str()), ("user/location_coords_for", location.as_str())] {
            db.execute(
                r#"INSERT INTO app_config (key, value, updated_at)
                   VALUES (?1, ?2, CURRENT_TIMESTAMP)
                   ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP"#,
                params![key, value],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
}
