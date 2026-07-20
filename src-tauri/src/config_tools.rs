//! Agent-facing settings tools: a declarative schema of the app_config keys the LLM is
//! allowed to read/change (`get_settings` / `set_setting`), plus `set_theme`,
//! `list_models`, and `test_llm_connection`.
//!
//! Deliberate exclusions: secret keys (`*_api_key`/`*_token`/`*_secret`) and
//! `mcp/servers` are not in the schema at all — the agent can never read or write them.
//! Safety-relevant keys are writable only after an explicit `ask_user` approval in
//! interactive chat and are hard-blocked in scheduled/background runs, mirroring the
//! dangerous-command flow in tools.rs.

use rusqlite::params;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::db::{get_app_setting_value, with_db};
use crate::AppState;

/// How a setting's value is validated. Bool settings keep the on-disk encoding the
/// Settings UI already writes ("1"/"0" vs "true"/"false") so both writers stay compatible.
pub(crate) enum SettingKind {
    /// One of a fixed set of values (matched case-insensitively, stored canonically).
    Choice(&'static [&'static str]),
    /// Integer within [min, max]. `allow_empty` lets "" through to mean "reset to default".
    Int { min: i64, max: i64, allow_empty: bool },
    /// Boolean stored as "1"/"0".
    Bool01,
    /// Boolean stored as "true"/"false".
    BoolTrueFalse,
    /// Free text.
    Text,
}

pub(crate) struct SettingDef {
    pub key: &'static str,
    pub description: &'static str,
    pub kind: SettingKind,
    /// Requires ask_user approval in interactive chat; blocked in background runs.
    pub gated: bool,
}

/// Chat providers as offered in the Settings UI. Shared by the llm/provider choice and
/// the key-status section of get_settings.
pub(crate) const LLM_PROVIDERS: &[&str] = &[
    "minimax", "openai", "anthropic", "groq", "deepseek", "xai", "google", "mistral",
    "together", "fireworks", "openrouter", "cohere", "meta", "custom",
];

/// LLM_PROVIDERS plus the empty string, for optional-provider settings like
/// llm/fallback_provider ("" = disabled).
pub(crate) const LLM_PROVIDERS_WITH_NONE: &[&str] = &[
    "", "minimax", "openai", "anthropic", "groq", "deepseek", "xai", "google", "mistral",
    "together", "fireworks", "openrouter", "cohere", "meta", "custom",
];

/// Theme ids must match THEMES in src/theme.js — the value is applied by the frontend.
pub(crate) const THEME_IDS: &[&str] = &[
    "dark", "light", "terminal", "abyss", "slate", "ember", "paper", "ocean", "system",
];

pub(crate) fn setting_schema() -> &'static [SettingDef] {
    use SettingKind::*;
    static SCHEMA: &[SettingDef] = &[
        SettingDef {
            key: "llm/provider",
            description: "Chat LLM provider. Check the API-key status section before switching — a provider without a usable key will fail on the next turn.",
            kind: Choice(LLM_PROVIDERS),
            gated: false,
        },
        SettingDef {
            key: "llm/model",
            description: "Model id for the current provider (e.g. 'MiniMax-M3', 'gpt-4.1-mini'). Use list_models to find valid ids. Empty = provider default.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "llm/max_tokens",
            description: "Max response tokens per LLM call. Empty resets to the default (40960).",
            kind: Int { min: 1, max: 1_000_000, allow_empty: true },
            gated: false,
        },
        SettingDef {
            key: "llm/custom_base_url",
            description: "Base URL for the 'custom' OpenAI-compatible provider (e.g. http://localhost:11434/v1). Only used when llm/provider is 'custom'.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "llm/fallback_provider",
            description: "Provider to retry a chat/agent turn with when the primary provider's first request fails (down, rate-limited, bad key). Empty disables failover.",
            kind: Choice(LLM_PROVIDERS_WITH_NONE),
            gated: false,
        },
        SettingDef {
            key: "llm/fallback_model",
            description: "Model id used with llm/fallback_provider. Empty falls back to that provider's remembered model; failover is skipped if neither is set.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "llm/key_fallback",
            description: "1 (default) = a provider with no key of its own may fall back to the OpenAI key. That cross-provider key is usually rejected with a 401; set 0 to get a clear 'no key configured' error instead.",
            kind: Bool01,
            gated: false,
        },
        SettingDef {
            key: "imagegen/provider",
            description: "Image generation provider. Empty string disables the default provider selection.",
            kind: Choice(&["", "minimax", "together", "openai"]),
            gated: false,
        },
        SettingDef {
            key: "imagegen/model",
            description: "Image model id. Empty = provider default.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "tts/provider",
            description: "Text-to-speech provider for voice replies.",
            kind: Choice(&["minimax", "fishspeech"]),
            gated: false,
        },
        SettingDef {
            key: "tts/model",
            description: "TTS model id. Empty = provider default.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "tts/voice_id",
            description: "Voice id for TTS output.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "stt/provider",
            description: "Cloud speech-to-text provider: 'auto' prefers Groq when its key is set, else OpenAI.",
            kind: Choice(&["auto", "groq", "openai"]),
            gated: false,
        },
        SettingDef {
            key: "video/local_transcription",
            description: "1 = try on-device transcription first for video work, falling back to cloud whisper on failure.",
            kind: Bool01,
            gated: false,
        },
        SettingDef {
            key: "video/hw_encode",
            description: "1 = use hardware (VideoToolbox) encoding for video renders.",
            kind: Bool01,
            gated: false,
        },
        SettingDef {
            key: "notifications/enabled",
            description: "Whether Muninn may post macOS notifications.",
            kind: BoolTrueFalse,
            gated: false,
        },
        SettingDef {
            key: "user/name",
            description: "The user's name, included in system context.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "user/about",
            description: "Free-text notes about the user, included in system context.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "user/location",
            description: "The user's location as text (e.g. 'Mountain Center, CA'). Note: map geocoding of a changed location happens the next time settings are saved in the Settings UI.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "quickask/shortcut",
            description: "Global Quick Ask keyboard shortcut (e.g. 'Cmd+Shift+Space'). May need an app restart to re-register.",
            kind: Text,
            gated: false,
        },
        SettingDef {
            key: "safety/auto_verify",
            description: "1 = an LLM safety verifier reviews flagged commands before interrupting the user.",
            kind: Bool01,
            gated: true,
        },
        SettingDef {
            key: "safety/approval_timeout_secs",
            description: "Seconds to wait for the user to answer a dangerous-command approval prompt (15-3600). Empty resets to the default (120).",
            kind: Int { min: 15, max: 3600, allow_empty: true },
            gated: true,
        },
        SettingDef {
            key: "mobile/server_port",
            description: "Port for the opt-in mobile companion server (1024-65535). Takes effect the next time the mobile server is started.",
            kind: Int { min: 1024, max: 65535, allow_empty: true },
            gated: true,
        },
    ];
    SCHEMA
}

pub(crate) fn find_setting_def(key: &str) -> Option<&'static SettingDef> {
    setting_schema().iter().find(|d| d.key == key)
}

/// Validate + normalize a raw value against a setting's kind. Returns the exact string
/// to store.
pub(crate) fn validate_setting_value(def: &SettingDef, raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    match &def.kind {
        SettingKind::Choice(options) => options
            .iter()
            .find(|o| o.eq_ignore_ascii_case(raw))
            .map(|o| o.to_string())
            .ok_or_else(|| {
                format!(
                    "invalid value '{}' for {} — must be one of: {}",
                    raw,
                    def.key,
                    options
                        .iter()
                        .map(|o| if o.is_empty() { "(empty)" } else { o })
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
        SettingKind::Int { min, max, allow_empty } => {
            if raw.is_empty() {
                if *allow_empty {
                    return Ok(String::new());
                }
                return Err(format!("{} requires an integer value", def.key));
            }
            let n: i64 = raw
                .parse()
                .map_err(|_| format!("invalid value '{}' for {} — must be an integer", raw, def.key))?;
            if n < *min || n > *max {
                return Err(format!(
                    "{} must be between {} and {} (got {})",
                    def.key, min, max, n
                ));
            }
            Ok(n.to_string())
        }
        SettingKind::Bool01 => parse_bool(raw)
            .map(|b| if b { "1" } else { "0" }.to_string())
            .ok_or_else(|| format!("invalid value '{}' for {} — use 1 or 0", raw, def.key)),
        SettingKind::BoolTrueFalse => parse_bool(raw)
            .map(|b| if b { "true" } else { "false" }.to_string())
            .ok_or_else(|| format!("invalid value '{}' for {} — use true or false", raw, def.key)),
        SettingKind::Text => Ok(raw.to_string()),
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

fn kind_hint(kind: &SettingKind) -> String {
    match kind {
        SettingKind::Choice(options) => format!(
            "one of: {}",
            options
                .iter()
                .map(|o| if o.is_empty() { "(empty)" } else { o })
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        SettingKind::Int { min, max, allow_empty } => {
            if *allow_empty {
                format!("integer {}-{}, or empty for default", min, max)
            } else {
                format!("integer {}-{}", min, max)
            }
        }
        SettingKind::Bool01 => "1 or 0".to_string(),
        SettingKind::BoolTrueFalse => "true or false".to_string(),
        SettingKind::Text => "text".to_string(),
    }
}

fn upsert_setting(state: &AppState, key: &str, value: &str) -> Result<(), String> {
    with_db(state, |db| {
        db.execute(
            r#"
            INSERT INTO app_config (key, value, updated_at)
            VALUES (?1, ?2, CURRENT_TIMESTAMP)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP
            "#,
            params![key, value],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// Whether a provider has its OWN key configured (setting or env var). Deliberately
/// stricter than `resolve_provider_key`, whose generic-provider fallback to the OpenAI
/// key would make e.g. Groq look usable when only an OpenAI key exists — a request
/// signed with the wrong provider's key just 401s.
fn provider_key_status(state: &AppState, provider: &str) -> Result<&'static str, String> {
    let from_settings = |keys: &[&str]| -> Result<bool, String> {
        Ok(crate::db::first_non_empty_setting(state, keys)?.is_some())
    };
    let has_env = |vars: &[&str]| vars.iter().any(|v| std::env::var(v).map_or(false, |s| !s.is_empty()));
    let configured = match provider {
        "minimax" => {
            from_settings(&["llm/minimax_api_key", "llm/anthropic_api_key"])?
                || has_env(&["MINIMAX_API_KEY", "ANTHROPIC_API_KEY"])
        }
        "anthropic" => from_settings(&["llm/anthropic_api_key"])? || has_env(&["ANTHROPIC_API_KEY"]),
        "openai" => from_settings(&["llm/openai_api_key"])? || has_env(&["OPENAI_API_KEY"]),
        "custom" => return Ok("no key required (local servers use a dummy key)"),
        other => from_settings(&[&format!("llm/{}_api_key", other)])?,
    };
    Ok(if configured { "key configured" } else { "no key" })
}

// --- get_settings ---

pub(crate) fn get_settings_tool(state: &AppState) -> Result<String, String> {
    let mut out = String::from("Agent-configurable settings (change with set_setting):\n\n");
    for def in setting_schema() {
        let current = get_app_setting_value(state, def.key)?.unwrap_or_default();
        let shown = if current.is_empty() { "(not set)".to_string() } else { format!("'{}'", current) };
        let gate = if def.gated { "  [requires user approval]" } else { "" };
        out.push_str(&format!(
            "{} = {}  [{}]{}\n    {}\n",
            def.key,
            shown,
            kind_hint(&def.kind),
            gate,
            def.description
        ));
    }

    out.push_str("\nProvider API-key status (read-only — keys themselves are never shown):\n");
    for provider in LLM_PROVIDERS {
        out.push_str(&format!("  {}: {}\n", provider, provider_key_status(state, provider)?));
    }

    out.push_str(
        "\nNotes:\n\
         - API keys, tokens, and mcp/servers are managed only by the user in the Settings UI — you cannot read or write them.\n\
         - The UI theme is changed with the set_theme tool, not set_setting.\n",
    );
    Ok(out)
}

// --- set_setting ---

pub(crate) async fn set_setting_tool(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    arguments: &Value,
    interactive: bool,
) -> Result<String, String> {
    let key = arguments
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or_else(|| "missing key".to_string())?;

    // Accept string/number/bool JSON values; everything is stored as a string.
    let raw_value = match arguments.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Null) | None => return Err("missing value".to_string()),
        Some(other) => return Err(format!("value must be a scalar, got: {}", other)),
    };

    let def = find_setting_def(key).ok_or_else(|| {
        format!(
            "'{}' is not an agent-configurable setting. Valid keys: {}. \
             API keys and mcp/servers can only be changed by the user in the Settings UI.",
            key,
            setting_schema().iter().map(|d| d.key).collect::<Vec<_>>().join(", ")
        )
    })?;

    let value = validate_setting_value(def, &raw_value)?;
    let previous = get_app_setting_value(state, key)?.unwrap_or_default();
    if previous == value {
        return Ok(format!("{} is already '{}' — no change made.", key, value));
    }

    if def.gated {
        if !interactive {
            return Err(format!(
                "Changing {} requires user approval, and no user is available in this \
                 scheduled/background run. Leave it unchanged.",
                key
            ));
        }
        let question = format!(
            "Muninn wants to change the setting {} from '{}' to '{}'.\n\n{}\n\nAllow this change?",
            key,
            if previous.is_empty() { "(not set)" } else { &previous },
            if value.is_empty() { "(default)" } else { &value },
            def.description
        );
        let answer = crate::interactive::ask_user(
            app,
            state,
            session_id,
            &question,
            vec!["Change it".to_string(), "Keep current".to_string()],
        )
        .await?;
        if !answer.trim().eq_ignore_ascii_case("change it") {
            return Err(format!(
                "The user declined this settings change (answered: {}). Do not retry it; \
                 leave the setting as is or ask the user what they want.",
                answer
            ));
        }
    }

    upsert_setting(state, key, &value)?;

    // Mirror the Settings UI's per-provider model memory so switching providers in the
    // UI later restores the model the agent picked.
    if key == "llm/model" {
        let provider = get_app_setting_value(state, "llm/provider")?
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "minimax".to_string());
        upsert_setting(state, &format!("llm/model/{}", provider), &value)?;
    }

    let _ = app.emit("settings-updated", serde_json::json!({ "key": key }));

    let restart_note = match key {
        "mobile/server_port" => " Takes effect the next time the mobile server starts.",
        "quickask/shortcut" => " May need an app restart to re-register the shortcut.",
        _ => "",
    };
    Ok(format!(
        "Set {} = '{}' (was {}).{}",
        key,
        value,
        if previous.is_empty() { "(not set)".to_string() } else { format!("'{}'", previous) },
        restart_note
    ))
}

// --- set_theme ---

pub(crate) fn set_theme_tool(app: &AppHandle, arguments: &Value) -> Result<String, String> {
    let raw = arguments
        .get("theme")
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or_else(|| "missing theme".to_string())?;
    let theme = THEME_IDS
        .iter()
        .find(|t| t.eq_ignore_ascii_case(raw))
        .ok_or_else(|| {
            format!(
                "unknown theme '{}' — must be one of: {}",
                raw,
                THEME_IDS.join(", ")
            )
        })?;
    app.emit("theme-changed", serde_json::json!({ "theme": theme }))
        .map_err(|e| e.to_string())?;
    Ok(format!("Theme changed to '{}'.", theme))
}

// --- list_models ---

pub(crate) async fn list_models_tool(state: &AppState, arguments: &Value) -> Result<String, String> {
    let provider = match arguments.get("provider").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => get_app_setting_value(state, "llm/provider")?
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "minimax".to_string()),
    };

    let models = crate::commands::fetch_models_impl(state, &provider).await?;
    if models.is_empty() {
        return Ok(format!("Provider '{}' returned no models.", provider));
    }
    let mut out = format!("{} models available on '{}':\n", models.len(), provider);
    for m in &models {
        out.push_str("  ");
        out.push_str(&m.id);
        if let Some(label) = &m.label {
            if label != &m.id {
                out.push_str(&format!("  ({})", label));
            }
        }
        if let Some(t) = &m.type_ {
            out.push_str(&format!("  [{}]", t));
        }
        out.push('\n');
    }
    out.push_str("\nUse set_setting with key 'llm/model' to switch (after llm/provider matches).");
    Ok(out)
}

// --- test_llm_connection ---

pub(crate) async fn test_llm_connection_tool(
    state: &AppState,
    arguments: &Value,
) -> Result<String, String> {
    let provider = match arguments.get("provider").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => get_app_setting_value(state, "llm/provider")?
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "minimax".to_string()),
    };
    let model = match arguments.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => {
            // Fall back to the per-provider remembered model, then the active model.
            crate::db::first_non_empty_setting(
                state,
                &[&format!("llm/model/{}", provider), "llm/model"],
            )?
            .ok_or_else(|| {
                format!(
                    "no model configured for '{}' — pass a model id (see list_models)",
                    provider
                )
            })?
        }
    };

    match crate::commands::test_llm_connection_impl(state, &provider, &model).await {
        Ok(msg) => Ok(format!("{} ({} / {})", msg, provider, model)),
        Err(e) => Err(format!("Connection test failed for {} / {}: {}", provider, model, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(key: &str) -> &'static SettingDef {
        find_setting_def(key).expect("setting def")
    }

    #[test]
    fn schema_excludes_secrets_and_mcp() {
        for d in setting_schema() {
            assert!(
                !d.key.contains("api_key") && !d.key.ends_with("_token") && !d.key.ends_with("_secret"),
                "secret key {} must not be in the agent schema",
                d.key
            );
            assert_ne!(d.key, "mcp/servers", "mcp/servers must not be agent-configurable");
        }
    }

    #[test]
    fn safety_and_server_keys_are_gated() {
        assert!(def("safety/auto_verify").gated);
        assert!(def("safety/approval_timeout_secs").gated);
        assert!(def("mobile/server_port").gated);
        assert!(!def("llm/model").gated);
        assert!(!def("user/name").gated);
    }

    #[test]
    fn choice_validation_normalizes_case() {
        let d = def("llm/provider");
        assert_eq!(validate_setting_value(d, "OpenAI").unwrap(), "openai");
        assert_eq!(validate_setting_value(d, " groq ").unwrap(), "groq");
        assert!(validate_setting_value(d, "gpt4").is_err());
    }

    #[test]
    fn choice_allows_empty_option_when_listed() {
        let d = def("imagegen/provider");
        assert_eq!(validate_setting_value(d, "").unwrap(), "");
        assert!(validate_setting_value(def("tts/provider"), "").is_err());
    }

    #[test]
    fn int_validation_bounds_and_empty() {
        let d = def("safety/approval_timeout_secs");
        assert_eq!(validate_setting_value(d, "120").unwrap(), "120");
        assert_eq!(validate_setting_value(d, "").unwrap(), "");
        assert!(validate_setting_value(d, "5").is_err());
        assert!(validate_setting_value(d, "999999").is_err());
        assert!(validate_setting_value(d, "abc").is_err());
    }

    #[test]
    fn bool_encodings_match_settings_ui() {
        // The Settings UI writes "1"/"0" for these...
        assert_eq!(validate_setting_value(def("safety/auto_verify"), "true").unwrap(), "1");
        assert_eq!(validate_setting_value(def("video/hw_encode"), "off").unwrap(), "0");
        // ...but "true"/"false" for notifications.
        assert_eq!(validate_setting_value(def("notifications/enabled"), "1").unwrap(), "true");
        assert_eq!(validate_setting_value(def("notifications/enabled"), "no").unwrap(), "false");
        assert!(validate_setting_value(def("safety/auto_verify"), "maybe").is_err());
    }

    #[test]
    fn fallback_settings_validate() {
        let d = def("llm/fallback_provider");
        assert_eq!(validate_setting_value(d, "").unwrap(), "");
        assert_eq!(validate_setting_value(d, "Groq").unwrap(), "groq");
        assert!(validate_setting_value(d, "gpt").is_err());
        assert_eq!(validate_setting_value(def("llm/key_fallback"), "false").unwrap(), "0");
    }

    #[test]
    fn provider_lists_stay_in_sync() {
        assert_eq!(LLM_PROVIDERS_WITH_NONE[0], "");
        assert_eq!(&LLM_PROVIDERS_WITH_NONE[1..], LLM_PROVIDERS);
    }

    #[test]
    fn theme_ids_cover_expected_set() {
        assert!(THEME_IDS.contains(&"dark"));
        assert!(THEME_IDS.contains(&"system"));
        assert!(!THEME_IDS.contains(&"neon"));
    }
}
