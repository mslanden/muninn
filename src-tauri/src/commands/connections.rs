//! Connections: everything Muninn can reach, grouped for the Connections window —
//! model providers, apps on this Mac, keyed services, MCP servers, and Muninn's own
//! built-in tools. A connection's switch flips all of its tools in the `tools` table
//! (the enabled set `tool_defs` already honours), so turning Mail off removes the mail
//! tools from every chat. Key status is reported as set/unset only — never the value.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::config_tools::LLM_PROVIDERS;
use crate::db::{get_app_setting_value, with_db};
use crate::AppState;

/// A connection that owns a fixed set of built-in tools.
struct ToolGroup {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    icon: &'static str,
    description: &'static str,
    /// Setting that must hold a key for the connection to work (None = no key needed).
    key_setting: Option<&'static str>,
    tools: &'static [&'static str],
}

const TOOL_GROUPS: &[ToolGroup] = &[
    ToolGroup {
        id: "mail",
        name: "Apple Mail",
        kind: "mac",
        icon: "mail",
        description: "Read, search and draft email in Mail.app. Nothing is ever sent automatically.",
        key_setting: None,
        tools: &["list_inbox", "search_mail", "read_mail", "compose_draft", "reply_draft"],
    },
    ToolGroup {
        id: "calendar",
        name: "Calendar",
        kind: "mac",
        icon: "calendar",
        description: "Add events to Calendar.app.",
        key_setting: None,
        tools: &["create_event"],
    },
    ToolGroup {
        id: "files",
        name: "Files and shell",
        kind: "mac",
        icon: "folder",
        description: "Read and edit files, run commands and background processes, git, tests and headless page previews — always inside the chat's workspace folder.",
        key_setting: None,
        tools: &[
            "read_file", "write_file", "edit_file", "append_file", "list_files", "search_files",
            "run_command", "git_status", "git_diff", "git_log", "git_commit", "code_outline",
            "find_references", "run_tests", "process", "capture_page",
        ],
    },
    ToolGroup {
        id: "screen",
        name: "Screen and apps",
        kind: "mac",
        icon: "screenshot",
        description: "Take screenshots so Muninn can see your screen, and open apps.",
        key_setting: None,
        tools: &["capture_screen", "open_app"],
    },
    ToolGroup {
        id: "web",
        name: "Web",
        kind: "service",
        icon: "search",
        description: "Search the web (needs a search key), fetch pages and call HTTP APIs.",
        key_setting: Some("search/brave_api_key"),
        tools: &["web_search", "web_fetch", "http_request"],
    },
    ToolGroup {
        id: "images",
        name: "Image generation",
        kind: "service",
        icon: "photo",
        description: "Generate images from a description.",
        key_setting: Some("imagegen/together_api_key"),
        tools: &["generate_image"],
    },
    ToolGroup {
        id: "maps",
        name: "Maps",
        kind: "service",
        icon: "map-pin",
        description: "Find places, search nearby and get directions with Mapbox.",
        key_setting: Some("maps/mapbox_api_key"),
        tools: &["search_places", "search_nearby", "get_directions"],
    },
    ToolGroup {
        id: "muninn",
        name: "Built in",
        kind: "builtin",
        icon: "sparkles",
        description: "Memory, skills, subagents, plans, tasks and self-configuration.",
        key_setting: None,
        tools: &[
            "memory", "skill", "subagent", "write_todos", "ask_user", "create_task", "list_tasks",
            "update_task", "delete_task", "run_task", "get_settings", "set_setting", "set_theme",
            "list_models", "test_llm_connection",
        ],
    },
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTool {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub id: String,
    pub name: String,
    /// "provider" | "mac" | "service" | "mcp" | "builtin"
    pub kind: String,
    pub icon: String,
    pub description: String,
    /// "in_use" | "ready" | "needs_key" | "off" | "error" | "disabled"
    pub status: String,
    pub detail: String,
    pub enabled: bool,
    pub key_setting: Option<String>,
    pub key_set: bool,
    pub tools: Vec<ConnectionTool>,
}

fn setting(state: &AppState, key: &str) -> Option<String> {
    get_app_setting_value(state, key).ok().flatten().filter(|v| !v.trim().is_empty())
}

fn provider_label(p: &str) -> &'static str {
    match p {
        "minimax" => "MiniMax",
        "openai" => "OpenAI",
        "anthropic" => "Anthropic",
        "groq" => "Groq",
        "deepseek" => "DeepSeek",
        "xai" => "xAI",
        "google" => "Google Gemini",
        "mistral" => "Mistral",
        "together" => "Together",
        "fireworks" => "Fireworks",
        "openrouter" => "OpenRouter",
        "cohere" => "Cohere",
        "meta" => "Meta",
        "custom" => "Custom (OpenAI-compatible)",
        _ => "Provider",
    }
}

/// The setting holding a provider's key (Settings' `providerKeyName`); `custom` is
/// identified by its base URL since its key is optional.
pub(crate) fn provider_key_setting(p: &str) -> String {
    if p == "custom" {
        "llm/custom_base_url".to_string()
    } else {
        format!("llm/{}_api_key", p)
    }
}

fn provider_env_key(p: &str) -> Option<&'static str> {
    match p {
        "openai" => Some("OPENAI_API_KEY"),
        "minimax" => Some("MINIMAX_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

/// Assign every known tool to its group; anything not listed lands in "Other" so a
/// newly added tool is never invisible in the UI.
fn group_tools(
    rows: &[(String, String, String, bool)],
) -> (HashMap<&'static str, Vec<ConnectionTool>>, Vec<ConnectionTool>) {
    let mut owner: HashMap<&str, &'static str> = HashMap::new();
    for g in TOOL_GROUPS {
        for t in g.tools {
            owner.insert(t, g.id);
        }
    }
    let mut grouped: HashMap<&'static str, Vec<ConnectionTool>> = HashMap::new();
    let mut other = Vec::new();
    for (id, name, description, enabled) in rows {
        let tool = ConnectionTool {
            id: id.clone(),
            name: name.clone(),
            description: description.clone(),
            enabled: *enabled,
        };
        match owner.get(id.as_str()) {
            Some(g) => grouped.entry(g).or_default().push(tool),
            None => other.push(tool),
        }
    }
    (grouped, other)
}

fn load_tools(state: &AppState) -> Result<Vec<(String, String, String, bool)>, String> {
    with_db(state, |db| {
        let mut stmt = db
            .prepare("SELECT tool_id, name, description, enabled FROM tools ORDER BY tool_id")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? == 1)))
            .map_err(|e| e.to_string())?;
        Ok(rows.filter_map(Result::ok).collect())
    })
}

/// Status of a tool-owning connection from its key + switch state.
fn group_status(enabled: bool, key_needed: bool, key_set: bool) -> (&'static str, String) {
    if key_needed && !key_set {
        ("needs_key", "Add a key to use it".to_string())
    } else if !enabled {
        ("off", "Off".to_string())
    } else {
        ("ready", String::new())
    }
}

#[tauri::command]
pub async fn get_connections(state: State<'_, AppState>) -> Result<Vec<Connection>, String> {
    let mut out = Vec::new();

    // Model providers.
    let active = setting(&state, "llm/provider").unwrap_or_else(|| "minimax".to_string());
    let active_model = setting(&state, "llm/model").unwrap_or_default();
    for &p in LLM_PROVIDERS {
        let key_setting = provider_key_setting(p);
        let key_set = setting(&state, &key_setting).is_some()
            || provider_env_key(p).is_some_and(|v| std::env::var(v).is_ok_and(|s| !s.trim().is_empty()));
        let remembered = setting(&state, &format!("llm/model/{}", p)).unwrap_or_default();
        let (status, detail) = if p == active {
            ("in_use", if active_model.is_empty() { "In use".to_string() } else { active_model.clone() })
        } else if key_set {
            ("ready", if remembered.is_empty() { "Key saved".to_string() } else { remembered })
        } else {
            ("needs_key", "Not set up".to_string())
        };
        out.push(Connection {
            id: format!("provider:{}", p),
            name: provider_label(p).to_string(),
            kind: "provider".to_string(),
            icon: "sparkles".to_string(),
            description: if p == "custom" {
                "Any OpenAI-compatible server — Ollama, LM Studio, vLLM. Set its base URL.".to_string()
            } else {
                format!("Chat models from {}.", provider_label(p))
            },
            status: status.to_string(),
            detail,
            enabled: true,
            key_setting: Some(key_setting),
            key_set,
            tools: Vec::new(),
        });
    }

    // Tool groups (Mac apps, services, built-ins) + catch-all.
    let rows = load_tools(&state)?;
    let (mut grouped, other) = group_tools(&rows);
    for g in TOOL_GROUPS {
        let tools = grouped.remove(g.id).unwrap_or_default();
        if tools.is_empty() {
            continue;
        }
        let enabled = tools.iter().any(|t| t.enabled);
        let key_set = g.key_setting.is_some_and(|k| setting(&state, k).is_some());
        // Web works without a search key (fetch/http), so only flag the key as missing.
        let key_needed = g.key_setting.is_some() && g.id != "web";
        let (status, mut detail) = group_status(enabled, key_needed, key_set);
        if status == "ready" {
            let on = tools.iter().filter(|t| t.enabled).count();
            detail = format!("{} of {} tools on", on, tools.len());
            if g.id == "web" && !key_set {
                detail = format!("{} — add a search key for web search", detail);
            }
        }
        out.push(Connection {
            id: g.id.to_string(),
            name: g.name.to_string(),
            kind: g.kind.to_string(),
            icon: g.icon.to_string(),
            description: g.description.to_string(),
            status: status.to_string(),
            detail,
            enabled,
            key_setting: g.key_setting.map(str::to_string),
            key_set,
            tools,
        });
    }
    if !other.is_empty() {
        let enabled = other.iter().any(|t| t.enabled);
        out.push(Connection {
            id: "other".to_string(),
            name: "Other tools".to_string(),
            kind: "builtin".to_string(),
            icon: "tool".to_string(),
            description: "Tools that don't belong to a connection yet.".to_string(),
            status: if enabled { "ready" } else { "off" }.to_string(),
            detail: format!("{} tools", other.len()),
            enabled,
            key_setting: None,
            key_set: false,
            tools: other,
        });
    }

    // MCP servers: configured (incl. disabled) + live status for the enabled ones.
    let configured: Vec<serde_json::Value> = setting(&state, "mcp/servers")
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if !configured.is_empty() {
        state.mcp.connected_tools(state.inner()).await;
        let live: HashMap<String, (bool, usize, Option<String>)> = state
            .mcp
            .status(state.inner())
            .await
            .into_iter()
            .map(|(name, connected, n, err)| (name, (connected, n, err)))
            .collect();
        let mut seen = HashSet::new();
        for cfg in configured {
            let Some(name) = cfg.get("name").and_then(|v| v.as_str()).map(str::to_string) else { continue };
            if !seen.insert(name.clone()) {
                continue;
            }
            let disabled = cfg.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);
            let command = cfg.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let (status, detail) = if disabled {
                ("disabled", "Off".to_string())
            } else {
                match live.get(&name) {
                    Some((true, n, _)) => ("ready", format!("Running · {} tools", n)),
                    Some((false, _, Some(err))) => ("error", err.clone()),
                    _ => ("error", "Not connected".to_string()),
                }
            };
            out.push(Connection {
                id: format!("mcp:{}", name),
                name,
                kind: "mcp".to_string(),
                icon: "plug".to_string(),
                description: format!("MCP server · {}", command),
                status: status.to_string(),
                detail,
                enabled: !disabled,
                key_setting: None,
                key_set: false,
                tools: Vec::new(),
            });
        }
    }
    Ok(out)
}

/// Turn a connection on or off. Tool groups flip every tool they own; MCP servers
/// get `"disabled": true/false` in their `mcp/servers` entry and the servers reload.
#[tauri::command]
pub async fn set_connection_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    if let Some(name) = id.strip_prefix("mcp:") {
        let raw = setting(&state, "mcp/servers").unwrap_or_else(|| "[]".to_string());
        let mut servers: Vec<serde_json::Value> =
            serde_json::from_str(&raw).map_err(|e| format!("mcp/servers isn't valid JSON: {}", e))?;
        let mut found = false;
        for s in servers.iter_mut() {
            if s.get("name").and_then(|v| v.as_str()) == Some(name) {
                s["disabled"] = serde_json::json!(!enabled);
                found = true;
            }
        }
        if !found {
            return Err(format!("no MCP server named '{}'", name));
        }
        let json = serde_json::to_string_pretty(&servers).map_err(|e| e.to_string())?;
        crate::db::set_app_setting_value(&state, "mcp/servers", &json)?;
        state.mcp.reset().await;
        let _ = app.emit("settings-updated", ());
        return Ok(());
    }
    let tools: Vec<String> = if id == "other" {
        let rows = load_tools(&state)?;
        group_tools(&rows).1.into_iter().map(|t| t.id).collect()
    } else {
        let g = TOOL_GROUPS
            .iter()
            .find(|g| g.id == id)
            .ok_or_else(|| format!("unknown connection '{}'", id))?;
        g.tools.iter().map(|t| t.to_string()).collect()
    };
    with_db(&state, |db| {
        for t in &tools {
            db.execute(
                "UPDATE tools SET enabled = ?2, updated_at = CURRENT_TIMESTAMP WHERE tool_id = ?1",
                rusqlite::params![t, if enabled { 1 } else { 0 }],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    })?;
    let _ = app.emit("connections-changed", ());
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppAccess {
    /// "ok" | "denied" | "busy" | "error"
    pub status: String,
    pub detail: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppAccessRequest {
    /// "mail" | "calendar"
    pub app: String,
}

/// On-demand check that Muninn can drive Mail / Calendar (Automation permission).
/// Runs a trivial read so it can't change anything; never automatic, because the
/// first check may show the macOS permission prompt.
#[tauri::command]
pub async fn check_app_access(req: AppAccessRequest) -> Result<AppAccess, String> {
    let (script, noun) = match req.app.as_str() {
        "mail" => (r#"tell application "Mail" to return count of accounts"#, "account"),
        "calendar" => (r#"tell application "Calendar" to return count of calendars"#, "calendar"),
        other => return Err(format!("unknown app '{}'", other)),
    };
    let result = tauri::async_runtime::spawn_blocking(move || {
        crate::mail::run_applescript_with_deadline(script, std::time::Duration::from_secs(20))
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(match result {
        Ok(out) => {
            let n = out.trim().parse::<u32>().unwrap_or(0);
            AppAccess { status: "ok".into(), detail: format!("{} {}{}", n, noun, if n == 1 { "" } else { "s" }) }
        }
        Err(e) if e.contains("Automation permission") => AppAccess { status: "denied".into(), detail: e },
        Err(e) if e.starts_with(crate::mail::TIMEOUT_MARKER) => AppAccess {
            status: "busy".into(),
            detail: "The app didn't answer within 20s — it may be busy. Try again shortly.".into(),
        },
        Err(e) => AppAccess { status: "error".into(), detail: e },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_belongs_to_exactly_one_group() {
        let mut seen = HashSet::new();
        for g in TOOL_GROUPS {
            for t in g.tools {
                assert!(seen.insert(*t), "tool '{}' is in more than one connection", t);
            }
        }
    }

    #[test]
    fn unknown_tools_fall_into_other() {
        let rows = vec![
            ("read_mail".to_string(), "Read".to_string(), String::new(), true),
            ("brand_new_tool".to_string(), "New".to_string(), String::new(), false),
        ];
        let (grouped, other) = group_tools(&rows);
        assert_eq!(grouped["mail"].len(), 1);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].id, "brand_new_tool");
    }

    #[test]
    fn group_status_flags_missing_keys_before_switch_state() {
        assert_eq!(group_status(true, true, false).0, "needs_key");
        assert_eq!(group_status(false, true, false).0, "needs_key");
        assert_eq!(group_status(false, false, false).0, "off");
        assert_eq!(group_status(true, true, true).0, "ready");
    }

    #[test]
    fn provider_key_settings_match_the_settings_ui() {
        assert_eq!(provider_key_setting("openai"), "llm/openai_api_key");
        assert_eq!(provider_key_setting("meta"), "llm/meta_api_key");
        assert_eq!(provider_key_setting("custom"), "llm/custom_base_url");
    }
}
