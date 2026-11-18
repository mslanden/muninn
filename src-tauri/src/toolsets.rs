//! Tool diet: which tool schemas go to the model on each request.
//!
//! Sending every enabled tool's schema on every request cost ~8k tokens before the
//! conversation even started, plus ~2k tokens of per-tool prose in the system prompt. Now a
//! small CORE set is always sent and the rest are grouped into toolsets the model loads on
//! demand with `load_tools` (or by simply calling one of their tools — the group then loads
//! itself). A loaded group stays on for the rest of the chat (subagents share their parent's),
//! so the request prefix — and the provider's prompt cache — only changes when a group loads.
//!
//! Each group carries the guidance that used to live in the system prompt (Mail's draft-only
//! rules, how tasks work, which map tool to use…); it is returned when the group loads.
//!
//! Execution is unaffected: a call is allowed if the tool is enabled, loaded or not.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Always sent: what nearly every task needs.
pub const CORE: &[&str] = &[
    "run_command", "read_file", "write_file", "edit_file", "append_file", "list_files", "search_files",
    "process", "web_search", "web_fetch", "memory", "skill", "subagent", "write_todos", "ask_user",
];

/// The tool that loads a group. Only offered while groups are being deferred.
pub const LOAD_TOOL: &str = "load_tools";

/// With fewer deferrable tools than this enabled (a skill that narrowed its tools, or most
/// tools switched off), everything is sent: the saving isn't worth the extra step.
const MIN_DEFERRED: usize = 6;

pub struct Toolset {
    pub id: &'static str,
    /// One line for the system prompt's group list.
    pub summary: &'static str,
    pub tools: &'static [&'static str],
    /// How to use the group well; returned when it loads (or put in the prompt when nothing
    /// is deferred).
    pub guidance: &'static str,
}

pub const TOOLSETS: &[Toolset] = &[
    Toolset {
        id: "code",
        summary: "git status/diff/log/commit, code outline, find references, run tests, raw HTTP requests",
        tools: &["git_status", "git_diff", "git_log", "git_commit", "code_outline", "find_references", "run_tests", "http_request"],
        guidance: "Prefer these over run_command for git and tests: they return structured, size-capped output. git_commit commits only what you name.",
    },
    Toolset {
        id: "screen",
        summary: "check a web page or local dev server in a headless browser, screenshot an app or the screen, open an app",
        tools: &["capture_page", "capture_screen", "open_app"],
        guidance: "To verify web work, use capture_page (headless — nothing appears on screen) and check both the screenshot and the console before saying it works; start a dev server with run_command background: true and wait for it with process first. For a native app, capture_screen with `app` grabs its window in the background without focusing it — prefer that. open_app with background: true opens an app behind the user's window; only bring an app to the front when the user asked to see it.",
    },
    Toolset {
        id: "media",
        summary: "generate images, make PDFs (reports, letters, slides) from HTML",
        tools: &["generate_image", "create_pdf"],
        guidance: "Generated images and PDFs display in the chat automatically — never say you can't show them. For a PDF, write complete HTML with inline CSS and set the page with @page (e.g. `size: A4; margin: 16mm`, or `size: 1280px 720px; margin: 0` for slides).",
    },
    Toolset {
        id: "mail",
        summary: "Apple Mail: list the inbox, search, read messages, draft emails and replies (never sends)",
        tools: &["list_inbox", "search_mail", "read_mail", "compose_draft", "reply_draft"],
        guidance: "You can read the user's email through Apple Mail on this Mac, across all configured accounts (iCloud, Gmail, work IMAP, etc.). Writing is DRAFT-ONLY: compose_draft and reply_draft save an unsent message to Drafts for the user to review and send — you can NEVER send, delete, mark read, or otherwise modify mail. After drafting, tell the user a draft is ready in Mail; do not imply it was sent.\n\nTypical flow: list_inbox for \"what's new\", search_mail when the user names a person or topic, then read_mail on a specific message id to quote or summarize it. The `id` from list/search is an opaque handle — pass it verbatim to read_mail. Handles are session-local to Mail.app (if Mail restarts, re-list to get fresh ones; read_mail says so when that happens).\n\nDates are ISO-8601 local time (YYYY-MM-DDTHH:MM:SS). For \"today's mail\" or \"last week\", put the range in `since` / `until` instead of listing everything and filtering — Mail filters server-side and reaches further back. `until` is exclusive: for \"today only\" set `since` to today and `until` to tomorrow.\n\nsearch_mail matches Subject + From only, not the body. For body questions (\"what did Bob say about the budget?\"), narrow with search_mail, then read_mail each candidate.\n\nThe first mail call may trigger a one-time macOS prompt (\"Muninn wants to control Mail\"); if a call fails with an authorization error, tell the user to grant Automation access in System Settings.",
    },
    Toolset {
        id: "calendar",
        summary: "add events to Apple Calendar",
        tools: &["create_event"],
        guidance: "Use the user's local time. The first call may trigger a one-time macOS Automation prompt for Calendar.",
    },
    Toolset {
        id: "maps",
        summary: "Apple Maps: find a named place, browse nearby places by kind, get directions (maps show in the chat)",
        tools: &["search_places", "search_nearby", "get_directions"],
        guidance: "Maps display in the chat automatically. search_nearby is the tool for \"find X near me\" / \"what's around here\" — describe the kind of place in plain words (\"mexican restaurant\", \"gas station\"); it uses the user's saved location unless you pass `near`. search_places pins a KNOWN place by name (\"Arriba's Mexican Grill\", \"LAX\"). For reviews or \"best of\" picks, combine with web_search. ALWAYS use get_directions instead of text-only directions: driving, walking and cycling give a route; transit gives travel time only; cycling and transit exist only where Apple Maps covers them — fall back to driving or walking if they fail.",
    },
    Toolset {
        id: "tasks",
        summary: "scheduled automations: create, list, update, delete or run tasks",
        tools: &["create_task", "list_tasks", "update_task", "delete_task", "run_task"],
        guidance: "Tasks are scheduled LLM runs — when one fires, the scheduler starts a fresh Muninn session and sends the task's `description` as the user message; you then act autonomously with no human in the loop.\n\nWrite `description` as a direct instruction to future-you, in the imperative (\"Summarize the top 5 HN posts and save to notes/hn.md\"), specific enough to act on without clarification.\n\nTasks are automations, not to-dos: don't create one for \"remind me to call mom\". Do create one for recurring work Muninn should handle on its own — digests, periodic checks, cron-style jobs. Schedules: 'every 30m', 'daily at 09:00', 'every monday at 10:00', 'hourly'; omit the schedule for a one-shot. Call list_tasks before editing or deleting to get the id.\n\nrun_task fires a task now regardless of its schedule. A task with no schedule and no next_run is manual/chain-only: it runs only via run_task or when chained from another task's chain_next. A failed one-shot gets status 'failed'; re-fire it with run_task after fixing the cause. Set `project_path` (an existing absolute folder) to scope a task's file/shell tools to that project.",
    },
    Toolset {
        id: "settings",
        summary: "read and change Muninn's own settings, switch theme, list models, test the model connection",
        tools: &["get_settings", "set_setting", "set_theme", "list_models", "test_llm_connection"],
        guidance: "Only allowlisted settings can be read or changed; secrets never can. Safety settings need the user's approval. Use list_models / test_llm_connection before switching models.",
    },
];

/// The group a tool belongs to, if any.
pub fn toolset_of(tool: &str) -> Option<&'static Toolset> {
    TOOLSETS.iter().find(|ts| ts.tools.contains(&tool))
}

/// Groups are tracked per chat; a subagent (`<chat>::sub-<n>`) shares its chat's.
fn base_session(session_id: &str) -> &str {
    session_id.split_once("::sub-").map_or(session_id, |(parent, _)| parent)
}

/// Whether some tools are held back for on-demand loading with this enabled set.
pub fn defers(enabled: &HashSet<String>) -> bool {
    enabled.iter().filter(|t| !CORE.contains(&t.as_str()) && toolset_of(t).is_some()).count() >= MIN_DEFERRED
}

/// Per-chat loaded groups. Lives in `AppState`; in memory only (a restart starts lean again).
#[derive(Default)]
pub struct LoadedToolsets(Mutex<HashMap<String, HashSet<&'static str>>>);

impl LoadedToolsets {
    fn get(&self, session_id: &str) -> HashSet<&'static str> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(base_session(session_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Mark a group loaded; true if it wasn't already.
    fn insert(&self, session_id: &str, id: &'static str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(base_session(session_id).to_string())
            .or_default()
            .insert(id)
    }

    /// Forget a chat's groups (the chat was deleted).
    pub fn clear(&self, session_id: &str) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(base_session(session_id));
    }
}

/// The tools whose schemas go to the model this round: core + loaded groups (+ `load_tools`),
/// or every enabled tool when nothing is deferred.
pub fn active_tools(loaded: &LoadedToolsets, session_id: &str, enabled: &HashSet<String>) -> HashSet<String> {
    if !defers(enabled) {
        return enabled.clone();
    }
    let groups = loaded.get(session_id);
    let mut active: HashSet<String> = enabled
        .iter()
        .filter(|t| CORE.contains(&t.as_str()) || toolset_of(t).map_or(true, |ts| groups.contains(ts.id)))
        .cloned()
        .collect();
    active.insert(LOAD_TOOL.to_string());
    active
}

/// Tools of `ts` the user has enabled (search_nearby rides on search_places' switch).
fn enabled_in(ts: &Toolset, enabled: &HashSet<String>) -> Vec<&'static str> {
    ts.tools
        .iter()
        .copied()
        .filter(|t| enabled.contains(*t) || (*t == "search_nearby" && enabled.contains("search_places")))
        .collect()
}

/// `load_tools`: load the named groups for this chat. Returns what is now available plus each
/// group's guidance.
pub fn load(loaded: &LoadedToolsets, session_id: &str, groups: &[String], enabled: &HashSet<String>) -> Result<String, String> {
    if groups.is_empty() {
        return Err(format!("name at least one group: {}", TOOLSETS.iter().map(|t| t.id).collect::<Vec<_>>().join(", ")));
    }
    let mut out = String::new();
    for name in groups {
        let name = name.trim().to_lowercase();
        let Some(ts) = TOOLSETS.iter().find(|t| t.id == name) else {
            return Err(format!(
                "unknown tool group '{}' (groups: {})",
                name,
                TOOLSETS.iter().map(|t| t.id).collect::<Vec<_>>().join(", ")
            ));
        };
        let tools = enabled_in(ts, enabled);
        if tools.is_empty() {
            out.push_str(&format!("{}: all of its tools are switched off in Connections.\n\n", ts.id));
            continue;
        }
        loaded.insert(session_id, ts.id);
        out.push_str(&format!("Loaded {}: {}. Call them directly from now on.\n{}\n\n", ts.id, tools.join(", "), ts.guidance));
    }
    Ok(out.trim_end().to_string())
}

/// Called after any tool runs: if it belongs to a group that wasn't loaded (the model called
/// it by name), load the group so its schemas are sent from the next round, and return the
/// group's guidance to append to this result.
pub fn note_used(loaded: &LoadedToolsets, session_id: &str, tool: &str, enabled: &HashSet<String>) -> Option<String> {
    if !defers(enabled) {
        return None;
    }
    let ts = toolset_of(tool)?;
    loaded
        .insert(session_id, ts.id)
        .then(|| format!("[The {} tools are now loaded for this chat.]\n{}", ts.id, ts.guidance))
}

/// The system prompt's tool section. With deferral: the core is described by its schemas, and
/// the loadable groups are listed (only ones with enabled tools). Without: each group's
/// guidance, since every schema is already present.
pub fn prompt_section(enabled: &HashSet<String>) -> String {
    let groups: Vec<(&Toolset, Vec<&str>)> = TOOLSETS
        .iter()
        .map(|ts| (ts, enabled_in(ts, enabled)))
        .filter(|(_, tools)| !tools.is_empty())
        .collect();
    if groups.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if defers(enabled) {
        out.push_str(
            "\n\n## More tools\n\
Besides the tools you can see, these groups load on demand. When a request needs one, call \
`load_tools` with the group name first (or call one of its tools by name — the group then \
loads itself). Loaded groups stay available for the rest of this chat.\n",
        );
        for (ts, tools) in &groups {
            out.push_str(&format!("- {} — {} ({})\n", ts.id, ts.summary, tools.join(", ")));
        }
    } else {
        for (ts, _) in &groups {
            out.push_str(&format!("\n\n## {} tools\n{}", ts.id, ts.guidance));
        }
    }
    out
}

/// JSON schema parameters for `load_tools`.
pub fn load_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "groups": {
                "type": "array",
                "items": {"type": "string", "enum": TOOLSETS.iter().map(|t| t.id).collect::<Vec<_>>()},
                "description": "Tool groups to load, e.g. [\"mail\"] or [\"maps\", \"calendar\"]."
            }
        },
        "required": ["groups"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn all_enabled() -> HashSet<String> {
        let mut s = set(CORE);
        for ts in TOOLSETS {
            s.extend(ts.tools.iter().map(|t| t.to_string()));
        }
        s
    }

    #[test]
    fn groups_are_disjoint_from_core_and_each_other() {
        let mut seen = HashSet::new();
        for t in CORE {
            assert!(seen.insert(*t), "{} twice", t);
        }
        for ts in TOOLSETS {
            for t in ts.tools {
                assert!(seen.insert(*t), "{} is in core or two groups", t);
            }
        }
    }

    #[test]
    fn a_new_chat_gets_core_plus_load_tools_and_groups_add_on() {
        let loaded = LoadedToolsets::default();
        let enabled = all_enabled();
        let active = active_tools(&loaded, "chat1", &enabled);
        assert!(active.contains("read_file") && active.contains(LOAD_TOOL));
        assert!(!active.contains("list_inbox") && !active.contains("get_directions"));

        let reply = load(&loaded, "chat1", &["mail".into()], &enabled).unwrap();
        assert!(reply.contains("Loaded mail: list_inbox") && reply.contains("DRAFT-ONLY"));
        let active = active_tools(&loaded, "chat1", &enabled);
        assert!(active.contains("list_inbox") && !active.contains("get_directions"));
        // Subagents share their chat's groups; other chats don't.
        assert!(active_tools(&loaded, "chat1::sub-2", &enabled).contains("read_mail"));
        assert!(!active_tools(&loaded, "chat2", &enabled).contains("read_mail"));
        loaded.clear("chat1");
        assert!(!active_tools(&loaded, "chat1", &enabled).contains("read_mail"));
    }

    #[test]
    fn calling_an_unloaded_tool_loads_its_group_once() {
        let loaded = LoadedToolsets::default();
        let enabled = all_enabled();
        let note = note_used(&loaded, "c", "get_directions", &enabled).unwrap();
        assert!(note.contains("maps tools are now loaded"));
        assert!(note_used(&loaded, "c", "search_places", &enabled).is_none(), "already loaded");
        assert!(note_used(&loaded, "c", "read_file", &enabled).is_none(), "core");
        assert!(active_tools(&loaded, "c", &enabled).contains("search_nearby"));
    }

    #[test]
    fn narrow_tool_sets_are_not_deferred() {
        // A skill limited to a few tools (or most tools switched off): send them all.
        let loaded = LoadedToolsets::default();
        let enabled = set(&["read_file", "memory", "skill", "list_inbox", "read_mail"]);
        assert!(!defers(&enabled));
        assert_eq!(active_tools(&loaded, "c", &enabled), enabled);
        assert!(note_used(&loaded, "c", "list_inbox", &enabled).is_none());
        let prompt = prompt_section(&enabled);
        assert!(prompt.contains("## mail tools") && prompt.contains("DRAFT-ONLY") && !prompt.contains("load_tools"));
    }

    #[test]
    fn load_reports_unknown_and_switched_off_groups() {
        let loaded = LoadedToolsets::default();
        let mut enabled = all_enabled();
        enabled.remove("create_event");
        assert!(load(&loaded, "c", &["weather".into()], &enabled).unwrap_err().contains("unknown tool group 'weather'"));
        assert!(load(&loaded, "c", &[], &enabled).is_err());
        assert!(load(&loaded, "c", &["calendar".into()], &enabled).unwrap().contains("switched off"));
        assert!(!active_tools(&loaded, "c", &enabled).contains("create_event"));
    }

    #[test]
    fn prompt_lists_only_groups_with_enabled_tools() {
        let mut enabled = all_enabled();
        for t in ["create_event"] {
            enabled.remove(t);
        }
        let p = prompt_section(&enabled);
        assert!(p.contains("- mail — ") && p.contains("- maps — ") && p.contains("load_tools"));
        assert!(!p.contains("- calendar — "));
        assert!(!p.contains("DRAFT-ONLY"), "guidance arrives with the group, not in the prompt");
    }
}
