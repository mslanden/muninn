//! Memory: profile, entities, episodes, relationships.

use rusqlite::{params, OptionalExtension};
use tauri::State;


use crate::AppState;
use crate::db::*;
use crate::models::*;

#[tauri::command]
pub fn get_memory_profile(state: State<AppState>) -> Result<MemoryProfileRecord, String> {
    with_db(&state, |db| {
        db.query_row(
            "SELECT user_name, user_about, profile_text, identity, personality, goals, preferences, current_state, patterns, updated_at FROM memory_profile WHERE id = 1",
            [],
            |row| {
                Ok(MemoryProfileRecord {
                    user_name: row.get(0)?,
                    user_about: row.get(1)?,
                    profile_text: row.get(2)?,
                    identity: row.get(3)?,
                    personality: row.get(4)?,
                    goals: row.get(5)?,
                    preferences: row.get(6)?,
                    current_state: row.get(7)?,
                    patterns: row.get(8)?,
                    updated_at: row.get(9)?,
                })
            },
        )
        .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub fn set_memory_profile(state: State<AppState>, req: MemoryProfileUpdateRequest) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute(
            r#"
            UPDATE memory_profile
            SET user_name = COALESCE(?1, user_name),
                user_about = COALESCE(?2, user_about),
                profile_text = COALESCE(?3, profile_text),
                identity = COALESCE(?4, identity),
                personality = COALESCE(?5, personality),
                goals = COALESCE(?6, goals),
                preferences = COALESCE(?7, preferences),
                current_state = COALESCE(?8, current_state),
                patterns = COALESCE(?9, patterns),
                updated_at = CURRENT_TIMESTAMP
            WHERE id = 1
            "#,
            params![req.user_name, req.user_about, req.profile_text,
                    req.identity, req.personality, req.goals,
                    req.preferences, req.current_state, req.patterns],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn get_entities(state: State<AppState>) -> Result<Vec<EntityRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare("SELECT id, kind, name, summary, aliases_json, created_at, updated_at FROM entities ORDER BY updated_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let aliases_json: String = row.get(4)?;
                let aliases: Vec<String> = serde_json::from_str(&aliases_json).unwrap_or_default();
                Ok(EntityRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    summary: row.get(3)?,
                    aliases,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
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
pub fn upsert_entity(state: State<AppState>, req: EntityUpsertRequest) -> Result<bool, String> {
    let kind = req.kind.trim().to_string();
    let name = req.name.trim().to_string();
    if kind.is_empty() || name.is_empty() {
        return Err("entity kind and name are required".to_string());
    }
    let aliases_json = serde_json::to_string(&req.aliases.unwrap_or_default())
        .unwrap_or_else(|_| "[]".to_string());
    with_db(&state, |db| {
        if let Some(id) = req.id {
            let changed = db
                .execute(
                    "UPDATE entities SET kind = ?2, name = ?3, summary = ?4, aliases_json = ?5, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
                    params![id, kind, name, req.summary.unwrap_or_default(), aliases_json],
                )
                .map_err(|e| e.to_string())?;
            Ok(changed > 0)
        } else {
            db.execute(
                "INSERT INTO entities (kind, name, summary, aliases_json) VALUES (?1, ?2, ?3, ?4)",
                params![kind, name, req.summary.unwrap_or_default(), aliases_json],
            )
            .map_err(|e| e.to_string())?;
            Ok(true)
        }
    })
}

#[tauri::command]
pub fn delete_entity(state: State<AppState>, req: EntityIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        // Clean up relationships and episode links (SQLite FK cascade may not be enabled)
        let _ = db.execute("DELETE FROM relationships WHERE from_entity_id = ?1 OR to_entity_id = ?1", params![req.id]);
        let _ = db.execute("DELETE FROM episode_entities WHERE entity_id = ?1", params![req.id]);
        let changed = db
            .execute("DELETE FROM entities WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

#[tauri::command]
pub fn get_episodes(state: State<AppState>) -> Result<Vec<EpisodeRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare("SELECT id, description, timestamp, created_at FROM episodes ORDER BY id DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, description, timestamp, created_at) = row.map_err(|e| e.to_string())?;
            // Get linked entity names
            let entity_names: Vec<String> = {
                let mut name_stmt = db
                    .prepare("SELECT e.name FROM episode_entities ee JOIN entities e ON ee.entity_id = e.id WHERE ee.episode_id = ?1")
                    .map_err(|e| e.to_string())?;
                let name_rows = name_stmt
                    .query_map(params![id], |row| row.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                let mut names = Vec::new();
                for nr in name_rows {
                    if let Ok(n) = nr {
                        names.push(n);
                    }
                }
                names
            };
            out.push(EpisodeRecord { id, description, timestamp, entity_names, created_at });
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn add_episode(state: State<AppState>, req: EpisodeCreateRequest) -> Result<bool, String> {
    let description = req.description.trim().to_string();
    if description.is_empty() {
        return Err("episode description is required".to_string());
    }
    with_db(&state, |db| {
        db.execute(
            "INSERT INTO episodes (description, timestamp) VALUES (?1, ?2)",
            params![description, req.timestamp.unwrap_or_default()],
        )
        .map_err(|e| e.to_string())?;
        let episode_id = db.last_insert_rowid();

        // Link entity names if provided
        if let Some(names) = &req.entity_names {
            for name in names {
                let entity_id: Option<i64> = db
                    .query_row(
                        "SELECT id FROM entities WHERE name = ?1 COLLATE NOCASE",
                        params![name.trim()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some(eid) = entity_id {
                    let _ = db.execute(
                        "INSERT OR IGNORE INTO episode_entities (episode_id, entity_id) VALUES (?1, ?2)",
                        params![episode_id, eid],
                    );
                }
            }
        }
        Ok(true)
    })
}

#[tauri::command]
pub fn delete_episode(state: State<AppState>, req: EpisodeIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let _ = db.execute("DELETE FROM episode_entities WHERE episode_id = ?1", params![req.id]);
        let changed = db
            .execute("DELETE FROM episodes WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

// ========== RELATIONSHIPS ==========

#[tauri::command]
pub fn get_relationships(state: State<AppState>) -> Result<Vec<RelationshipRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare(
                r#"SELECT r.id, r.from_entity_id, r.to_entity_id,
                          COALESCE(ef.name, '?'), COALESCE(et.name, '?'),
                          r.label, r.created_at
                   FROM relationships r
                   LEFT JOIN entities ef ON r.from_entity_id = ef.id
                   LEFT JOIN entities et ON r.to_entity_id = et.id
                   ORDER BY r.id DESC"#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(RelationshipRecord {
                    id: row.get(0)?,
                    from_entity_id: row.get(1)?,
                    to_entity_id: row.get(2)?,
                    from_name: row.get(3)?,
                    to_name: row.get(4)?,
                    label: row.get(5)?,
                    created_at: row.get(6)?,
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
pub fn upsert_relationship(state: State<AppState>, req: RelationshipUpsertRequest) -> Result<bool, String> {
    let label = req.label.trim().to_string();
    if label.is_empty() {
        return Err("relationship label is required".to_string());
    }
    with_db(&state, |db| {
        db.execute(
            "INSERT OR IGNORE INTO relationships (from_entity_id, to_entity_id, label) VALUES (?1, ?2, ?3)",
            params![req.from_entity_id, req.to_entity_id, label],
        )
        .map_err(|e| e.to_string())?;
        Ok(true)
    })
}

#[tauri::command]
pub fn delete_relationship(state: State<AppState>, req: RelationshipIdRequest) -> Result<bool, String> {
    with_db(&state, |db| {
        let changed = db
            .execute("DELETE FROM relationships WHERE id = ?1", params![req.id])
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    })
}

// ========== MESSAGE SEARCH ==========
