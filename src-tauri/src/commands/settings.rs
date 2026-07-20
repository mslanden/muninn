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

/// List the models a provider exposes via its `/models` endpoint. The API key is
/// resolved the same way the chat path does (`resolve_provider_key`) and is used only to
/// sign the outbound request — it is never returned to the caller. Results are sorted by
/// id. `minimax` has no public models API, so the frontend keeps a static list for it.
#[tauri::command]
pub async fn fetch_models(
    state: State<'_, AppState>,
    provider: String,
) -> Result<Vec<ModelInfo>, String> {
    fetch_models_impl(&state, provider.trim()).await
}

/// Shared implementation, also called by the agent's `list_models` tool (which has a
/// bare `&AppState` rather than a tauri `State`).
pub(crate) async fn fetch_models_impl(
    state: &AppState,
    provider: &str,
) -> Result<Vec<ModelInfo>, String> {
    let provider = provider.trim().to_string();
    if provider == "minimax" {
        return Err("MiniMax has no public models API".to_string());
    }

    let url = match provider.as_str() {
        "openai" => "https://api.openai.com/v1/models".to_string(),
        "groq" => "https://api.groq.com/openai/v1/models".to_string(),
        "together" => "https://api.together.xyz/v1/models".to_string(),
        "deepseek" => "https://api.deepseek.com/models".to_string(),
        "xai" => "https://api.x.ai/v1/models".to_string(),
        "google" => "https://generativelanguage.googleapis.com/v1beta/openai/models".to_string(),
        "mistral" => "https://api.mistral.ai/v1/models".to_string(),
        "fireworks" => "https://api.fireworks.ai/inference/v1/models".to_string(),
        "openrouter" => "https://openrouter.ai/api/v1/models".to_string(),
        "cohere" => "https://api.cohere.com/v1/models".to_string(),
        "anthropic" => "https://api.anthropic.com/v1/models".to_string(),
        "meta" => "https://api.meta.ai/v1/models".to_string(),
        "custom" => {
            let base = get_app_setting_value(&state, "llm/custom_base_url")?
                .filter(|b| !b.trim().is_empty())
                .ok_or_else(|| "No custom base URL configured".to_string())?;
            format!("{}/models", base.trim_end_matches('/'))
        }
        other => return Err(format!("Unknown provider '{}'", other)),
    };

    let api_key = crate::llm::resolve_provider_key(&state, &provider)?;

    let mut req = state.http.get(&url);
    if provider == "anthropic" {
        req = req
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.bearer_auth(&api_key);
    }

    let resp = req.send().await.map_err(|e| format!("models request failed: {e}"))?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(body);
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

    let mut models = parse_models(&provider, &json);
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// Map a provider's `/models` response into `ModelInfo`s. Most providers return the
/// OpenAI shape `{"data":[{"id":...}]}`; the exceptions (together's top-level array,
/// cohere's `{"models":[{"name"}]}`, and the display-name/label fields) are handled here.
fn parse_models(provider: &str, json: &serde_json::Value) -> Vec<ModelInfo> {
    let empty: Vec<serde_json::Value> = Vec::new();
    match provider {
        // Top-level JSON array; objects carry id, display_name, and type.
        "together" => json
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|v| v.as_str())?.to_string();
                Some(ModelInfo {
                    id,
                    label: m.get("display_name").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    type_: m.get("type").and_then(|v| v.as_str()).map(|s| s.to_string()),
                })
            })
            .collect(),
        // {"models":[{"name":...}]} — name is the id.
        "cohere" => json
            .get("models")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty)
            .iter()
            .filter_map(|m| {
                let id = m.get("name").and_then(|v| v.as_str())?.to_string();
                Some(ModelInfo { id, label: None, type_: None })
            })
            .collect(),
        // {"data":[{"id","name"}]} — name is a display label.
        "openrouter" => json
            .get("data")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty)
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|v| v.as_str())?.to_string();
                Some(ModelInfo {
                    id,
                    label: m.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    type_: None,
                })
            })
            .collect(),
        // {"data":[{"id","display_name"}]}
        "anthropic" => json
            .get("data")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty)
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|v| v.as_str())?.to_string();
                Some(ModelInfo {
                    id,
                    label: m.get("display_name").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    type_: None,
                })
            })
            .collect(),
        // Default OpenAI shape: {"data":[{"id":...}]}.
        _ => json
            .get("data")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty)
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|v| v.as_str())?.to_string();
                Some(ModelInfo {
                    id,
                    label: None,
                    type_: m.get("type").and_then(|v| v.as_str()).map(|s| s.to_string()),
                })
            })
            .collect(),
    }
}

/// Send a minimal request to verify a provider/model pair works with the configured
/// key. Returns a friendly Ok string on any 2xx; on failure returns the HTTP status
/// plus response body. The key is never logged or returned.
#[tauri::command]
pub async fn test_llm_connection(
    state: State<'_, AppState>,
    provider: String,
    model: String,
) -> Result<String, String> {
    test_llm_connection_impl(&state, provider.trim(), &model).await
}

/// Shared implementation, also called by the agent's `test_llm_connection` tool.
pub(crate) async fn test_llm_connection_impl(
    state: &AppState,
    provider: &str,
    model: &str,
) -> Result<String, String> {
    let (is_anthropic, endpoint, api_key) =
        crate::llm::resolve_provider_wire(state, provider.trim())?;

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });

    let mut req = state.http.post(&endpoint);
    if is_anthropic {
        req = req
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.bearer_auth(&api_key);
    }

    let resp = req
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("connection failed: {e}"))?;

    let status = resp.status();
    if status.is_success() {
        Ok("Connected — model responded".to_string())
    } else {
        let text = resp.text().await.unwrap_or_default();
        Err(format!("{}: {}", status, text))
    }
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
