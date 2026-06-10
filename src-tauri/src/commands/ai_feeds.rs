//! AI feeds: scheduled LLM-generated feeds, items, articles, web search, geocoding.

use rusqlite::params;
use tauri::{AppHandle, State};


use crate::AppState;
use crate::db::*;
use crate::models::*;

#[tauri::command]
pub fn get_ai_feeds(state: State<AppState>) -> Result<Vec<AiFeedRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT id, name, topics_json, refresh_interval_mins, schedule_config, last_refreshed_at, created_at \
                 FROM ai_feeds ORDER BY id ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, topics_json, interval, sched_config, last_refreshed, created_at) =
                row.map_err(|e| e.to_string())?;
            let topics: Vec<String> = serde_json::from_str(&topics_json).unwrap_or_default();
            out.push(AiFeedRecord {
                id,
                name,
                topics,
                refresh_interval_mins: interval,
                schedule_config: sched_config,
                last_refreshed_at: last_refreshed,
                created_at,
            });
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn create_ai_feed(state: State<AppState>, req: AiFeedCreateRequest) -> Result<AiFeedRecord, String> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("Feed name is required.".to_string());
    }
    let topics_json = serde_json::to_string(&req.topics).map_err(|e| e.to_string())?;
    let interval = req.refresh_interval_mins.unwrap_or(60).max(1);
    let schedule_config = req.schedule_config.clone().unwrap_or_default();

    // Compute next_run from schedule_config if provided
    let mut next_run = String::new();
    if !schedule_config.trim().is_empty() {
        let now = chrono::Local::now().naive_local();
        if let Some(computed) = crate::scheduler::next_run_from_schedule_config(&schedule_config, now) {
            next_run = computed.format("%Y-%m-%dT%H:%M:%S").to_string();
        }
    }

    with_db(&state, |db| {
        db.execute(
            "INSERT INTO ai_feeds (name, topics_json, refresh_interval_mins, schedule_config, next_run) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, topics_json, interval, schedule_config, next_run],
        )
        .map_err(|e| e.to_string())?;
        let id = db.last_insert_rowid();
        let (last_refreshed, created_at) = db
            .query_row(
                "SELECT last_refreshed_at, created_at FROM ai_feeds WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| e.to_string())?;
        Ok(AiFeedRecord {
            id,
            name: name.clone(),
            topics: req.topics.clone(),
            refresh_interval_mins: interval,
            schedule_config,
            last_refreshed_at: last_refreshed,
            created_at,
        })
    })
}

#[tauri::command]
pub fn delete_ai_feed(state: State<AppState>, req: AiFeedIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM ai_feeds WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn update_ai_feed(state: State<AppState>, req: AiFeedUpdateRequest) -> Result<AiFeedRecord, String> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("Feed name is required.".to_string());
    }
    let topics_json = serde_json::to_string(&req.topics).map_err(|e| e.to_string())?;
    let interval = req.refresh_interval_mins.unwrap_or(60).max(1);
    let schedule_config = req.schedule_config.clone().unwrap_or_default();

    // Compute next_run from schedule_config if provided
    let mut next_run = String::new();
    if !schedule_config.trim().is_empty() {
        let now = chrono::Local::now().naive_local();
        if let Some(computed) = crate::scheduler::next_run_from_schedule_config(&schedule_config, now) {
            next_run = computed.format("%Y-%m-%dT%H:%M:%S").to_string();
        }
    }

    with_db(&state, |db| {
        db.execute(
            "UPDATE ai_feeds SET name=?2, topics_json=?3, refresh_interval_mins=?4, \
             schedule_config=?5, next_run=?6 WHERE id=?1",
            params![req.id, name, topics_json, interval, schedule_config, next_run],
        )
        .map_err(|e| e.to_string())?;
        let (last_refreshed, created_at) = db
            .query_row(
                "SELECT last_refreshed_at, created_at FROM ai_feeds WHERE id = ?1",
                params![req.id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| e.to_string())?;
        Ok(AiFeedRecord {
            id: req.id,
            name: name.clone(),
            topics: req.topics.clone(),
            refresh_interval_mins: interval,
            schedule_config,
            last_refreshed_at: last_refreshed,
            created_at,
        })
    })
}

#[tauri::command]
pub fn get_ai_feed_items(state: State<AppState>, req: AiFeedIdRequest) -> Result<Vec<AiFeedItemRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT id, feed_id, title, summary, url, source_name, is_read, created_at, image_url, article \
                 FROM ai_feed_items WHERE feed_id = ?1 ORDER BY id DESC LIMIT 200",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![req.id], |row| {
                Ok(AiFeedItemRecord {
                    id: row.get(0)?,
                    feed_id: row.get(1)?,
                    title: row.get(2)?,
                    summary: row.get(3)?,
                    url: row.get(4)?,
                    source_name: row.get(5)?,
                    is_read: row.get::<_, i64>(6)? == 1,
                    created_at: row.get(7)?,
                    image_url: row.get(8)?,
                    article: row.get(9)?,
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
pub fn mark_ai_feed_item_read(state: State<AppState>, req: AiFeedItemIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("UPDATE ai_feed_items SET is_read = 1 WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub async fn generate_feed_item_image(app: AppHandle, state: State<'_, AppState>, req: AiFeedItemIdRequest) -> Result<String, String> {
    // Check if image already exists
    let existing = with_db(&state, |db| {
        db.query_row(
            "SELECT image_url FROM ai_feed_items WHERE id = ?1",
            params![req.id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| e.to_string())
    })?;
    if !existing.is_empty() {
        return Ok(existing);
    }

    // Get the item's title
    let title = with_db(&state, |db| {
        db.query_row(
            "SELECT title FROM ai_feed_items WHERE id = ?1",
            params![req.id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| e.to_string())
    })?;

    let local_path = crate::tools::generate_feed_thumbnail(&state, &title, &app).await?;

    with_db(&state, |db| {
        db.execute(
            "UPDATE ai_feed_items SET image_url = ?1 WHERE id = ?2",
            params![local_path, req.id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    Ok(local_path)
}

/// Lazily generate (and cache) a fuller AI article/overview for a single feed item.
/// Returns the cached article if one already exists, otherwise fetches the original
/// page, asks the LLM to write an overview, stores it, and returns it.
#[tauri::command]
pub async fn generate_feed_article(
    state: State<'_, AppState>,
    req: AiFeedItemIdRequest,
) -> Result<String, String> {
    // Return cached article if present.
    let (existing, title, summary, url, source_name) = with_db(&state, |db| {
        db.query_row(
            "SELECT article, title, summary, url, source_name FROM ai_feed_items WHERE id = ?1",
            params![req.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .map_err(|e| e.to_string())
    })?;
    if !existing.trim().is_empty() {
        return Ok(existing);
    }

    // Best-effort fetch of the original page so the article is grounded in real content.
    let source_text = if url.trim().is_empty() {
        String::new()
    } else {
        crate::tools::web_fetch_impl(&state, &url).await.unwrap_or_default()
    };

    let article = crate::llm::generate_feed_article_text(
        &state,
        &title,
        &summary,
        &url,
        &source_name,
        &source_text,
    )
    .await?;

    with_db(&state, |db| {
        db.execute(
            "UPDATE ai_feed_items SET article = ?1 WHERE id = ?2",
            params![article, req.id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    Ok(article)
}

#[tauri::command]
pub async fn refresh_ai_feed(
    app: AppHandle,
    state: State<'_, AppState>,
    req: AiFeedIdRequest,
) -> Result<AiFeedRefreshResult, String> {
    let items_added = crate::feeds::refresh_feed_impl(state.inner(), req.id, &app).await?;
    Ok(AiFeedRefreshResult { items_added })
}

/// AI-powered web search: researches a natural-language query and returns a synthesized
/// Markdown answer plus a list of cited source links.
#[tauri::command]
pub async fn ai_web_search(
    state: State<'_, AppState>,
    req: AiWebSearchRequest,
) -> Result<AiWebSearchResult, String> {
    crate::search::ai_web_search_impl(state.inner(), &req.query).await
}

#[tauri::command]
pub async fn geocode_user_location(
    state: State<'_, AppState>,
    location: String,
) -> Result<(), String> {
    let token = first_non_empty_setting(&state, &["maps/mapbox_api_key"])
        .map_err(|e| format!("DB error: {}", e))?
        .ok_or_else(|| "Mapbox API key not configured".to_string())?;

    let url = format!(
        "https://api.mapbox.com/search/geocode/v6/forward?q={}&limit=1&access_token={}",
        urlencoding::encode(&location),
        urlencoding::encode(&token),
    );
    let resp = state.http.get(&url).send().await
        .map_err(|e| format!("Geocoding failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Geocoding error: {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    let coords = body
        .get("features")
        .and_then(|f| f.as_array())
        .and_then(|a| a.first())
        .and_then(|f| f.get("geometry"))
        .and_then(|g| g.get("coordinates"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| format!("Could not geocode '{}'", location))?;

    let lon = coords.first().and_then(|v| v.as_f64()).ok_or("Invalid lon")?;
    let lat = coords.get(1).and_then(|v| v.as_f64()).ok_or("Invalid lat")?;

    // Store as "lon,lat" in settings
    let proximity = format!("{},{}", lon, lat);
    with_db(&state, |db| {
        db.execute(
            r#"INSERT INTO app_config (key, value, updated_at)
               VALUES ('user/location_coords', ?1, CURRENT_TIMESTAMP)
               ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP"#,
            params![proximity],
        ).map_err(|e| e.to_string())?;
        Ok(())
    })
}
