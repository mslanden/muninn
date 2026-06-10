//! The activity feed (feed_items table): list, mark read, remove, clear.

use rusqlite::params;
use tauri::State;


use crate::AppState;
use crate::db::*;
use crate::models::*;

#[tauri::command]
pub fn get_feed_items(state: State<AppState>, req: FeedListRequest) -> Result<Vec<FeedRecord>, String> {
    let limit = req.limit.unwrap_or(100).min(500) as i64;
    let unread_only = req.unread_only.unwrap_or(false);

    with_db(&state, |db| {
        let mut out = Vec::new();
        if unread_only {
            let mut stmt = db
                .prepare(
                    r#"
                    SELECT id, kind, title, message, icon_name, is_read, metadata, created_at
                    FROM feed_items
                    WHERE is_read = 0
                    ORDER BY id DESC
                    LIMIT ?1
                    "#,
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![limit], |row| {
                    Ok(FeedRecord {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        title: row.get(2)?,
                        message: row.get(3)?,
                        icon_name: row.get(4)?,
                        is_read: row.get::<_, i64>(5)? == 1,
                        metadata: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                out.push(row.map_err(|e| e.to_string())?);
            }
        } else {
            let mut stmt = db
                .prepare(
                    r#"
                    SELECT id, kind, title, message, icon_name, is_read, metadata, created_at
                    FROM feed_items
                    ORDER BY id DESC
                    LIMIT ?1
                    "#,
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![limit], |row| {
                    Ok(FeedRecord {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        title: row.get(2)?,
                        message: row.get(3)?,
                        icon_name: row.get(4)?,
                        is_read: row.get::<_, i64>(5)? == 1,
                        metadata: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                out.push(row.map_err(|e| e.to_string())?);
            }
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn mark_feed_item_read(state: State<AppState>, req: FeedItemRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("UPDATE feed_items SET is_read = 1 WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn mark_all_feed_read(state: State<AppState>) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute("UPDATE feed_items SET is_read = 1", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn remove_feed_item(state: State<AppState>, req: FeedItemRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM feed_items WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn clear_feed_items(state: State<AppState>) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute("DELETE FROM feed_items", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}
