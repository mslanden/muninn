use rusqlite::{params, Connection};
use serde_json::Value;

use crate::models::HistoryMessage;
use crate::AppState;

/// Current schema version, stored in SQLite's `PRAGMA user_version`. Bump this and
/// add a `if version < N` block in `run_migrations` for every new schema change.
const SCHEMA_VERSION: i64 = 8;

fn get_schema_version(conn: &Connection) -> Result<i64, String> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| e.to_string())
}

fn set_schema_version(conn: &Connection, version: i64) -> Result<(), String> {
    // Pragmas must go through execute_batch (see commit 09fc056).
    conn.execute_batch(&format!("PRAGMA user_version = {}", version))
        .map_err(|e| e.to_string())
}

/// Run a migration statement, tolerating only errors that mean the change was already
/// applied (installs that predate schema versioning are at unknown intermediate states).
/// Any other failure is a real migration error and is propagated instead of being
/// silently swallowed.
fn exec_migration(conn: &Connection, sql: &str) -> Result<(), String> {
    match conn.execute(sql, []) {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("duplicate column name") || msg.contains("no such column") {
                Ok(())
            } else {
                Err(format!("migration failed: {} — {}", sql, msg))
            }
        }
    }
}

/// Versioned schema migrations. The baseline `CREATE TABLE IF NOT EXISTS` statements in
/// `init_db` always run; everything that changes an existing schema lives here, gated on
/// `PRAGMA user_version` so each step runs exactly once and real failures surface.
fn run_migrations(conn: &Connection) -> Result<(), String> {
    let version = get_schema_version(conn)?;

    if version < 1 {
        // v1 consolidates every pre-versioning ad-hoc migration. Installs from that era
        // may be at any intermediate state, so already-applied changes are tolerated
        // (see exec_migration); anything else aborts startup with a real error.
        let statements = [
            "ALTER TABLE sessions ADD COLUMN project_path TEXT NOT NULL DEFAULT ''",
            // Structured content (Anthropic-style blocks) so tool-calling turns survive
            // reload; hidden=1 marks intermediate tool rounds replayed to the model only.
            "ALTER TABLE messages ADD COLUMN content_json TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE messages ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE entities ADD COLUMN aliases_json TEXT NOT NULL DEFAULT '[]'",
            // Note: ALTER TABLE ADD COLUMN can't use CURRENT_TIMESTAMP (non-constant
            // default) — the pre-versioning code tried and silently failed forever.
            "ALTER TABLE entities ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE memory_profile ADD COLUMN identity TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE memory_profile ADD COLUMN personality TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE memory_profile ADD COLUMN goals TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE memory_profile ADD COLUMN preferences TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE memory_profile ADD COLUMN current_state TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE memory_profile ADD COLUMN patterns TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE tasks ADD COLUMN completed_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE tasks ADD COLUMN result TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE tasks ADD COLUMN run_count INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE tasks ADD COLUMN triggered_by TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE tasks ADD COLUMN chain_next TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE tasks ADD COLUMN conditions TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE tasks ADD COLUMN condition_status TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE tasks ADD COLUMN schedule_config TEXT NOT NULL DEFAULT ''",
            // Rename legacy projects.spino_md → agents_md for pre-rename DBs.
            "ALTER TABLE projects RENAME COLUMN spino_md TO agents_md",
            "ALTER TABLE ai_feeds ADD COLUMN schedule_config TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE ai_feeds ADD COLUMN next_run TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE ai_feed_items ADD COLUMN image_url TEXT NOT NULL DEFAULT ''",
            // Lazily-generated full AI article for a feed item (empty until requested).
            "ALTER TABLE ai_feed_items ADD COLUMN article TEXT NOT NULL DEFAULT ''",
            // Clear old remote http:// image URLs that can't load due to ATS.
            "UPDATE ai_feed_items SET image_url = '' WHERE image_url LIKE 'http://%'",
        ];
        for sql in statements {
            exec_migration(conn, sql)?;
        }
        set_schema_version(conn, 1)?;
    }

    if version < 2 {
        // Task chaining: a chain-triggered run reads `pending_input` (the upstream task's
        // full output, JSON-encoded) at start, injects it into its prompt, then clears it.
        let statements = [
            "ALTER TABLE tasks ADD COLUMN pending_input TEXT NOT NULL DEFAULT ''",
        ];
        for sql in statements {
            exec_migration(conn, sql)?;
        }
        set_schema_version(conn, 2)?;
    }

    if version < 3 {
        // Per-run task history (one row per scheduler execution) for chain observability.
        // The base CREATE TABLE in init_db already runs for fresh DBs; this is a no-op there.
        set_schema_version(conn, 3)?;
    }

    if version < 4 {
        // Session pinning + user-set titles for the chat session list.
        let statements = [
            "ALTER TABLE sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN title TEXT",
        ];
        for sql in statements {
            exec_migration(conn, sql)?;
        }
        set_schema_version(conn, 4)?;
    }

    if version < 5 {
        // Deferred-command queue for the auto-verify safety flow: when the verifier
        // escalates a dangerous command and the user is unavailable to approve it, the
        // command is parked here for later approval/dismissal.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS deferred_commands (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              session_id TEXT NOT NULL,
              command TEXT NOT NULL,
              cwd TEXT,
              workspace_root TEXT,
              reasons TEXT,
              verifier_note TEXT,
              status TEXT NOT NULL DEFAULT 'pending',
              created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
              resolved_at TEXT
            );
            "#,
        )
        .map_err(|e| e.to_string())?;
        set_schema_version(conn, 5)?;
    }

    if version < 6 {
        // Per-session goal for the "/goal" auto-continue loop: when set, the chat turn
        // loop evaluates whether the goal is met after each assistant reply and, if not,
        // re-prompts the agent to keep working. NULL/empty means no active goal.
        conn.execute_batch(
            r#"
            ALTER TABLE sessions ADD COLUMN goal TEXT;
            "#,
        )
        .map_err(|e| e.to_string())?;
        set_schema_version(conn, 6)?;
    }

    if version < 7 {
        // Per-source-video edit plan persistence for the Studio Video tab: keyed by the
        // video's absolute path so reopening the same file restores the last plan (AI or
        // manual) instead of starting over.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS video_plans (
              path TEXT PRIMARY KEY,
              plan_json TEXT NOT NULL,
              updated_at TEXT NOT NULL
            );
            "#,
        )
        .map_err(|e| e.to_string())?;
        set_schema_version(conn, 7)?;
    }

    if version < 8 {
        // Per-task workspace + custom-agent targeting: a task can pin its run to a
        // project folder (file/shell tools scoped there) and/or a custom agent
        // (system prompt + tool allowlist). Empty = current default behavior.
        let statements = [
            "ALTER TABLE tasks ADD COLUMN project_path TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE tasks ADD COLUMN agent_id TEXT NOT NULL DEFAULT ''",
        ];
        for sql in statements {
            exec_migration(conn, sql)?;
        }
        set_schema_version(conn, 8)?;
    }

    // When adding a migration: append `if version < N { ...; set_schema_version(conn, N)?; }`
    // and bump SCHEMA_VERSION.
    debug_assert!(SCHEMA_VERSION == 8, "add a migration block for the new version");

    Ok(())
}

pub(crate) fn init_db(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
          id TEXT PRIMARY KEY,
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          project_path TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS messages (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          session_id TEXT NOT NULL,
          role TEXT NOT NULL,
          content TEXT NOT NULL,
          content_json TEXT NOT NULL DEFAULT '',
          hidden INTEGER NOT NULL DEFAULT 0,
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS tool_logs (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          session_id TEXT NOT NULL,
          kind TEXT NOT NULL,
          input TEXT NOT NULL,
          output TEXT NOT NULL,
          status TEXT NOT NULL,
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS tools (
          tool_id TEXT PRIMARY KEY,
          name TEXT NOT NULL,
          description TEXT NOT NULL,
          icon_name TEXT NOT NULL,
          enabled INTEGER NOT NULL DEFAULT 1,
          updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS feed_items (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          kind TEXT NOT NULL,
          title TEXT NOT NULL,
          message TEXT NOT NULL,
          icon_name TEXT NOT NULL,
          is_read INTEGER NOT NULL DEFAULT 0,
          metadata TEXT NOT NULL DEFAULT '',
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS tasks (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          name TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          schedule TEXT NOT NULL DEFAULT '',
          next_run TEXT NOT NULL DEFAULT '',
          status TEXT NOT NULL DEFAULT 'scheduled',
          pending_input TEXT NOT NULL DEFAULT '',
          project_path TEXT NOT NULL DEFAULT '',
          agent_id TEXT NOT NULL DEFAULT '',
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS task_runs (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id INTEGER NOT NULL,
          session_id TEXT NOT NULL DEFAULT '',
          trigger_kind TEXT NOT NULL DEFAULT 'scheduled',
          triggered_by INTEGER NOT NULL DEFAULT 0,
          status TEXT NOT NULL DEFAULT '',
          output TEXT NOT NULL DEFAULT '',
          note TEXT NOT NULL DEFAULT '',
          started_at TEXT NOT NULL DEFAULT '',
          finished_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_task_runs_task ON task_runs(task_id, id DESC);

        CREATE TABLE IF NOT EXISTS session_todos (
          session_id TEXT PRIMARY KEY,
          todos_json TEXT NOT NULL DEFAULT '[]',
          updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS agents (
          agent_id TEXT PRIMARY KEY,
          name TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          slash_command TEXT NOT NULL DEFAULT '',
          system_prompt TEXT NOT NULL DEFAULT '',
          max_iterations INTEGER NOT NULL DEFAULT 5,
          enabled INTEGER NOT NULL DEFAULT 1,
          allowed_tools_json TEXT NOT NULL DEFAULT '[]',
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS projects (
          path TEXT PRIMARY KEY,
          name TEXT NOT NULL DEFAULT '',
          agents_md TEXT NOT NULL DEFAULT '',
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          last_opened_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS app_config (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL DEFAULT '',
          updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS memory_profile (
          id INTEGER PRIMARY KEY CHECK (id = 1),
          user_name TEXT NOT NULL DEFAULT '',
          user_about TEXT NOT NULL DEFAULT '',
          profile_text TEXT NOT NULL DEFAULT '',
          identity TEXT NOT NULL DEFAULT '',
          personality TEXT NOT NULL DEFAULT '',
          goals TEXT NOT NULL DEFAULT '',
          preferences TEXT NOT NULL DEFAULT '',
          current_state TEXT NOT NULL DEFAULT '',
          patterns TEXT NOT NULL DEFAULT '',
          updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS entities (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          kind TEXT NOT NULL,
          name TEXT NOT NULL,
          summary TEXT NOT NULL DEFAULT '',
          aliases_json TEXT NOT NULL DEFAULT '[]',
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS relationships (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          from_entity_id INTEGER NOT NULL,
          to_entity_id INTEGER NOT NULL,
          label TEXT NOT NULL,
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          FOREIGN KEY(from_entity_id) REFERENCES entities(id) ON DELETE CASCADE,
          FOREIGN KEY(to_entity_id) REFERENCES entities(id) ON DELETE CASCADE,
          UNIQUE(from_entity_id, to_entity_id, label)
        );

        CREATE TABLE IF NOT EXISTS episodes (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          description TEXT NOT NULL,
          timestamp TEXT NOT NULL DEFAULT '',
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS episode_entities (
          episode_id INTEGER NOT NULL,
          entity_id INTEGER NOT NULL,
          PRIMARY KEY(episode_id, entity_id),
          FOREIGN KEY(episode_id) REFERENCES episodes(id) ON DELETE CASCADE,
          FOREIGN KEY(entity_id) REFERENCES entities(id) ON DELETE CASCADE
        );
        "#,
    )
    .map_err(|e| e.to_string())?;

    // Embeddings table for semantic memory retrieval
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS embeddings (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          source_type TEXT NOT NULL,
          source_id INTEGER NOT NULL,
          content TEXT NOT NULL,
          embedding BLOB NOT NULL,
          updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          UNIQUE(source_type, source_id)
        );

        CREATE TABLE IF NOT EXISTS ai_feeds (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          name TEXT NOT NULL,
          topics_json TEXT NOT NULL DEFAULT '[]',
          refresh_interval_mins INTEGER NOT NULL DEFAULT 60,
          last_refreshed_at TEXT NOT NULL DEFAULT '',
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS ai_feed_items (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          feed_id INTEGER NOT NULL,
          title TEXT NOT NULL,
          summary TEXT NOT NULL DEFAULT '',
          url TEXT NOT NULL DEFAULT '',
          source_name TEXT NOT NULL DEFAULT '',
          is_read INTEGER NOT NULL DEFAULT 0,
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          FOREIGN KEY(feed_id) REFERENCES ai_feeds(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS deferred_commands (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          session_id TEXT NOT NULL,
          command TEXT NOT NULL,
          cwd TEXT,
          workspace_root TEXT,
          reasons TEXT,
          verifier_note TEXT,
          status TEXT NOT NULL DEFAULT 'pending',
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          resolved_at TEXT
        );

        CREATE TABLE IF NOT EXISTS video_plans (
          path TEXT PRIMARY KEY,
          plan_json TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        "#,
    )
    .map_err(|e| e.to_string())?;

    run_migrations(conn)?;

    conn.execute(
        r#"
        INSERT INTO memory_profile (id, user_name, user_about, profile_text, identity, personality, goals, preferences, current_state, patterns, updated_at)
        VALUES (1, '', '', '', '', '', '', '', '', '', CURRENT_TIMESTAMP)
        ON CONFLICT(id) DO NOTHING
        "#,
        [],
    )
    .map_err(|e| e.to_string())?;

    let default_tools = [
        (
            "run_command",
            "Run Command",
            "Run a shell command scoped to the current workspace.",
            "terminal",
        ),
        ("read_file", "Read File", "Read a file from workspace.", "file"),
        ("write_file", "Write File", "Write content to a workspace file.", "edit"),
        ("edit_file", "Edit File", "Edit part of a file by exact-string replacement.", "edit"),
        ("append_file", "Append File", "Append or prepend content to a workspace file.", "edit"),
        ("capture_screen", "Capture Screen", "Take a screenshot of the screen or frontmost window so the model can see it.", "image"),
        ("open_app", "Open App", "Launch or focus a macOS app, optionally opening a URL in it.", "terminal"),
        ("list_files", "List Files", "List files in a workspace directory.", "folder"),
        (
            "search_files",
            "Search Files",
            "Search text recursively in workspace files.",
            "search",
        ),
        (
            "web_search",
            "Web Search",
            "Search the web using Brave Search API.",
            "search",
        ),
        (
            "web_fetch",
            "Web Fetch",
            "Fetch the text content of a URL.",
            "search",
        ),
        (
            "generate_image",
            "Generate Image",
            "Generate images from text descriptions.",
            "image",
        ),
        (
            "probe_video",
            "Probe Video",
            "Read a video's metadata (duration, resolution, fps, audio).",
            "image",
        ),
        (
            "analyze_video",
            "Analyze Video",
            "Detect silences and produce a timestamped transcript to plan video edits.",
            "image",
        ),
        (
            "edit_video",
            "Edit Video",
            "Cut, filter, caption, reframe, and re-render videos with ffmpeg.",
            "image",
        ),
        (
            "generate_shorts",
            "Generate Shorts",
            "Auto-clip a long recording into captioned 9:16 vertical shorts (great for scheduled runs).",
            "image",
        ),
        (
            "compile_videos",
            "Compile Videos",
            "Combine several clips into one video, keeping the good parts (best takes or a montage).",
            "image",
        ),
        (
            "search_places",
            "Search Places",
            "Find places, businesses, and points of interest.",
            "map",
        ),
        (
            "get_directions",
            "Get Directions",
            "Get directions between two locations.",
            "map",
        ),
        (
            "git_status",
            "Git Status",
            "Show branch, last commit, and changed files in the workspace repo.",
            "wrench",
        ),
        (
            "git_diff",
            "Git Diff",
            "Show working-tree, staged, or commit-range diffs.",
            "wrench",
        ),
        (
            "git_log",
            "Git Log",
            "Show recent commit history for the workspace repo.",
            "wrench",
        ),
        (
            "git_commit",
            "Git Commit",
            "Commit tracked changes (or specific files) in the workspace repo.",
            "wrench",
        ),
        (
            "code_outline",
            "Code Outline",
            "List declarations with line numbers for a file or directory.",
            "file-text",
        ),
        (
            "find_references",
            "Find References",
            "Find whole-word uses of a symbol across the workspace with context.",
            "search",
        ),
        (
            "http_request",
            "HTTP Request",
            "Make an HTTP request to a public URL (GET/POST/etc) for API testing.",
            "globe",
        ),
        (
            "run_tests",
            "Run Tests",
            "Run the project's test suite (cargo/npm/pytest, auto-detected).",
            "terminal",
        ),
        (
            "create_task",
            "Create Task",
            "Create a scheduled or one-off task.",
            "clock",
        ),
        (
            "list_tasks",
            "List Tasks",
            "List existing tasks with status, schedule, and next run.",
            "clock",
        ),
        (
            "update_task",
            "Update Task",
            "Edit an existing task's name, description, schedule, or status.",
            "clock",
        ),
        (
            "delete_task",
            "Delete Task",
            "Delete a task by id.",
            "clock",
        ),
        (
            "run_task",
            "Run Task",
            "Fire a task immediately, regardless of its schedule.",
            "clock",
        ),
        (
            "list_inbox",
            "List Inbox",
            "Read recent messages from Apple Mail's unified inbox.",
            "envelope",
        ),
        (
            "search_mail",
            "Search Mail",
            "Search Apple Mail by subject or sender (not body).",
            "envelope",
        ),
        (
            "read_mail",
            "Read Mail",
            "Fetch the full body of a specific message by Message-ID.",
            "envelope",
        ),
        (
            "compose_draft",
            "Compose Draft",
            "Create an unsent email draft in Apple Mail for the user to review/send.",
            "envelope",
        ),
        (
            "reply_draft",
            "Reply Draft",
            "Create an unsent draft reply to a message, addressed to its sender.",
            "envelope",
        ),
        (
            "create_event",
            "Create Calendar Event",
            "Add an event to the user's Apple Calendar.",
            "clock",
        ),
        (
            "write_todos",
            "Write Todos",
            "Maintain a visible plan/checklist for the current chat session.",
            "checklist",
        ),
        (
            "ask_user",
            "Ask User",
            "Pause mid-turn and ask the user a clarifying question (interactive sessions only).",
            "question",
        ),
    ];

    for (tool_id, name, description, icon_name) in default_tools {
        conn.execute(
            r#"
            INSERT INTO tools (tool_id, name, description, icon_name, enabled)
            VALUES (?1, ?2, ?3, ?4, 1)
            ON CONFLICT(tool_id) DO NOTHING
            "#,
            params![tool_id, name, description, icon_name],
        )
        .map_err(|e| e.to_string())?;
    }

    // Seed default Research agent
    conn.execute(
        r#"
        INSERT INTO agents (agent_id, name, description, slash_command, system_prompt, max_iterations, enabled, allowed_tools_json, updated_at)
        VALUES (
            'default-research-agent',
            'Research Agent',
            'Autonomous research agent that investigates topics using available tools.',
            'research',
            'You are a focused research agent. When given a query, systematically investigate it by reading files, running commands, and searching the codebase. Summarize your findings clearly and concisely. Break complex questions into steps and use tools to gather evidence before answering.',
            5,
            1,
            '["run_command","read_file","search_files"]',
            CURRENT_TIMESTAMP
        )
        ON CONFLICT(agent_id) DO NOTHING
        "#,
        [],
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}

/// Run a closure against a pooled SQLite connection. WAL mode allows concurrent
/// readers, so parallel chat turns / scheduler ticks no longer serialize on a single
/// connection mutex; writers coordinate via the 5s busy_timeout set at pool init.
pub(crate) fn with_db<T, F>(state: &AppState, f: F) -> Result<T, String>
where
    F: FnOnce(&Connection) -> Result<T, String>,
{
    let conn = state.db.get().map_err(|e| format!("db pool: {}", e))?;
    f(&conn)
}

pub(crate) fn get_app_setting_value(state: &AppState, key: &str) -> Result<Option<String>, String> {
    with_db(state, |db| {
        db.query_row(
            "SELECT value FROM app_config WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    })
}

pub(crate) fn first_non_empty_setting(state: &AppState, keys: &[&str]) -> Result<Option<String>, String> {
    for key in keys {
        if let Some(value) = get_app_setting_value(state, key)? {
            if !value.trim().is_empty() {
                return Ok(Some(value));
            }
        }
    }
    Ok(None)
}

pub(crate) fn add_feed_item(
    state: &AppState,
    kind: &str,
    title: &str,
    message: &str,
    icon_name: &str,
    metadata: &str,
) {
    let _ = with_db(state, |db| {
        db.execute(
            r#"
            INSERT INTO feed_items (kind, title, message, icon_name, metadata, is_read)
            VALUES (?1, ?2, ?3, ?4, ?5, 0)
            "#,
            params![kind, title, message, icon_name, metadata],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    });
}

/// Record one task execution and prune to the most recent 50 runs for that task.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_task_run(
    state: &AppState,
    task_id: i64,
    session_id: &str,
    trigger_kind: &str,
    triggered_by: i64,
    status: &str,
    output: &str,
    note: &str,
    started_at: &str,
) {
    let _ = with_db(state, |db| {
        db.execute(
            r#"
            INSERT INTO task_runs (task_id, session_id, trigger_kind, triggered_by, status, output, note, started_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![task_id, session_id, trigger_kind, triggered_by, status, output, note, started_at],
        )
        .map_err(|e| e.to_string())?;
        db.execute(
            "DELETE FROM task_runs WHERE task_id = ?1 AND id NOT IN \
             (SELECT id FROM task_runs WHERE task_id = ?1 ORDER BY id DESC LIMIT 50)",
            params![task_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    });
}

pub(crate) fn normalize_project_path(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

use rusqlite::OptionalExtension;

/// Insert a chat message, persisting both the flat display text and the structured content blocks.
///
/// `blocks` is the Anthropic-style content array for model replay. `hidden=true` marks intermediate
/// tool-round turns that should be invisible in the UI but replayed to the model so it doesn't
/// think it hallucinated its own prior tool calls.
pub(crate) fn insert_message(
    state: &AppState,
    session_id: &str,
    role: &str,
    display_text: &str,
    blocks: &[Value],
    hidden: bool,
) -> Result<(), String> {
    let content_json = if blocks.is_empty() {
        String::new()
    } else {
        serde_json::to_string(blocks).map_err(|e| e.to_string())?
    };
    with_db(state, |db| {
        db.execute(
            "INSERT INTO messages (session_id, role, content, content_json, hidden) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, role, display_text, content_json, if hidden { 1i64 } else { 0i64 }],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// Remove orphan tool_use/tool_result pairs after a trim so the provider API accepts the
/// history. Anthropic requires every tool_use block to be followed by a tool_result block with
/// the matching id — trimming from the front can leave a dangling tool_result whose tool_use
/// was dropped (or vice versa).
pub(crate) fn sanitize_tool_pairs(history: &mut Vec<HistoryMessage>) {
    // 1. Drop any leading user turns that are pure tool_results — their matching tool_use was
    //    trimmed away.
    while history.first().map_or(false, |m| {
        m.role == "user"
            && !m.blocks.is_empty()
            && m.blocks.iter().all(|b| {
                b.get("type").and_then(Value::as_str) == Some("tool_result")
            })
    }) {
        history.remove(0);
    }

    // 2. Walk user turns and drop any tool_result blocks whose matching tool_use isn't in the
    //    immediately-preceding assistant turn.
    let mut i = 0;
    while i < history.len() {
        if history[i].role == "user" {
            let prev_tool_use_ids: std::collections::HashSet<String> = if i > 0
                && history[i - 1].role == "assistant"
            {
                history[i - 1]
                    .blocks
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                            b.get("id").and_then(Value::as_str).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
            let kept: Vec<Value> = history[i]
                .blocks
                .iter()
                .filter(|b| {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = b
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        prev_tool_use_ids.contains(id)
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            history[i].blocks = kept;
            if history[i].blocks.is_empty() {
                history.remove(i);
                continue;
            }
        }
        i += 1;
    }

    // 3. Drop tool_use blocks from assistant turns whose matching tool_result isn't in the
    //    next user turn. If the assistant turn becomes empty, drop it.
    let mut i = 0;
    while i < history.len() {
        if history[i].role == "assistant" {
            let next_result_ids: std::collections::HashSet<String> = if i + 1 < history.len()
                && history[i + 1].role == "user"
            {
                history[i + 1]
                    .blocks
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                            b.get("tool_use_id")
                                .and_then(Value::as_str)
                                .map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
            let kept: Vec<Value> = history[i]
                .blocks
                .iter()
                .filter(|b| {
                    if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                        next_result_ids.contains(id)
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            history[i].blocks = kept;
            if history[i].blocks.is_empty() {
                history.remove(i);
                continue;
            }
        }
        i += 1;
    }
}

/// Load the full structured history for a session, including hidden tool-round turns.
/// Legacy rows (content_json empty) are wrapped as a single text block so they still replay.
pub(crate) fn load_history_structured(
    state: &AppState,
    session_id: &str,
    limit: usize,
) -> Result<Vec<HistoryMessage>, String> {
    with_db(state, |db| {
        let mut stmt = db
            .prepare(
                "SELECT role, content, content_json FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session_id, limit as i64], |row| {
                let role: String = row.get(0)?;
                let content: String = row.get(1)?;
                let content_json: String = row.get(2)?;
                Ok((role, content, content_json))
            })
            .map_err(|e| e.to_string())?;

        let mut items: Vec<HistoryMessage> = Vec::new();
        for row in rows {
            let (role, content, content_json) = row.map_err(|e| e.to_string())?;
            let blocks: Vec<Value> = if content_json.trim().is_empty() {
                // Legacy row — treat as a single text block. Skip if the text is empty (shouldn't
                // happen, but some older rows with tool output placeholders could be blank).
                if content.is_empty() {
                    Vec::new()
                } else {
                    vec![serde_json::json!({"type": "text", "text": content})]
                }
            } else {
                serde_json::from_str(&content_json).unwrap_or_else(|_| {
                    vec![serde_json::json!({"type": "text", "text": content})]
                })
            };
            items.push(HistoryMessage { role, blocks });
        }
        items.reverse();
        Ok(items)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, blocks: Vec<Value>) -> HistoryMessage {
        HistoryMessage {
            role: role.to_string(),
            blocks,
        }
    }

    fn tool_use(id: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": "read_file", "input": {"path": "x"}})
    }

    fn tool_result(id: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": "ok"})
    }

    fn text(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }

    // ---- sanitize_tool_pairs ----

    #[test]
    fn well_formed_history_is_untouched() {
        let mut history = vec![
            msg("user", vec![text("hi")]),
            msg("assistant", vec![text("calling tool"), tool_use("t1")]),
            msg("user", vec![tool_result("t1")]),
            msg("assistant", vec![text("done")]),
        ];
        sanitize_tool_pairs(&mut history);
        assert_eq!(history.len(), 4);
        assert_eq!(history[1].blocks.len(), 2);
    }

    #[test]
    fn leading_orphan_tool_result_is_dropped() {
        // Trimming from the front can leave a tool_result whose tool_use was cut.
        let mut history = vec![
            msg("user", vec![tool_result("t0")]),
            msg("user", vec![text("hi")]),
            msg("assistant", vec![text("hello")]),
        ];
        sanitize_tool_pairs(&mut history);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].blocks[0]["text"], "hi");
    }

    #[test]
    fn orphan_tool_use_without_result_is_dropped() {
        let mut history = vec![
            msg("user", vec![text("hi")]),
            msg("assistant", vec![text("calling"), tool_use("t1")]),
            // No tool_result follows (e.g. trimmed away).
            msg("user", vec![text("are you there?")]),
        ];
        sanitize_tool_pairs(&mut history);
        // tool_use should be stripped but the assistant text kept.
        assert_eq!(history.len(), 3);
        assert_eq!(history[1].blocks.len(), 1);
        assert_eq!(history[1].blocks[0]["type"], "text");
    }

    #[test]
    fn assistant_turn_with_only_orphan_tool_use_is_removed() {
        let mut history = vec![
            msg("user", vec![text("hi")]),
            msg("assistant", vec![tool_use("t1")]),
        ];
        sanitize_tool_pairs(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "user");
    }

    #[test]
    fn tool_result_not_matching_previous_assistant_is_dropped() {
        let mut history = vec![
            msg("user", vec![text("hi")]),
            msg("assistant", vec![tool_use("t1")]),
            // Result id doesn't match t1 — both sides become orphans.
            msg("user", vec![tool_result("OTHER"), text("extra")]),
        ];
        sanitize_tool_pairs(&mut history);
        // The mismatched tool_result is gone; the text block survives. The orphan
        // tool_use assistant turn is then removed entirely.
        let user_turn = history.iter().find(|m| {
            m.role == "user" && m.blocks.iter().any(|b| b["type"] == "text" && b["text"] == "extra")
        });
        assert!(user_turn.is_some());
        for m in &history {
            for b in &m.blocks {
                assert_ne!(b["type"], "tool_use", "orphan tool_use must not survive");
                assert_ne!(b["type"], "tool_result", "orphan tool_result must not survive");
            }
        }
    }

    #[test]
    fn multiple_parallel_tool_uses_are_kept_when_all_results_present() {
        let mut history = vec![
            msg("user", vec![text("hi")]),
            msg("assistant", vec![tool_use("a"), tool_use("b")]),
            msg("user", vec![tool_result("a"), tool_result("b")]),
            msg("assistant", vec![text("done")]),
        ];
        sanitize_tool_pairs(&mut history);
        assert_eq!(history.len(), 4);
        assert_eq!(history[1].blocks.len(), 2);
        assert_eq!(history[2].blocks.len(), 2);
    }

    #[test]
    fn empty_history_is_fine() {
        let mut history: Vec<HistoryMessage> = Vec::new();
        sanitize_tool_pairs(&mut history);
        assert!(history.is_empty());
    }

    // ---- migrations ----

    #[test]
    fn init_db_on_fresh_database_sets_schema_version() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn init_db_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        init_db(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn legacy_database_is_migrated_to_v1() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a pre-versioning install: minimal old-shape tables, user_version 0.
        conn.execute_batch(
            r#"
            CREATE TABLE sessions (id TEXT PRIMARY KEY, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
            CREATE TABLE messages (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              session_id TEXT NOT NULL,
              role TEXT NOT NULL,
              content TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE projects (
              path TEXT PRIMARY KEY,
              name TEXT NOT NULL DEFAULT '',
              spino_md TEXT NOT NULL DEFAULT '',
              created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
              last_opened_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            "#,
        )
        .unwrap();

        init_db(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Old columns must have been added / renamed.
        conn.execute(
            "INSERT INTO sessions (id, project_path) VALUES ('s1', '/tmp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, content_json, hidden) VALUES ('s1', 'user', 'hi', '', 0)",
            [],
        )
        .unwrap();
        let agents_md: String = conn
            .query_row("SELECT agents_md FROM projects WHERE 0=1 UNION SELECT ''", [], |r| r.get(0))
            .unwrap();
        assert_eq!(agents_md, "");
    }

    #[test]
    fn v4_adds_session_pinned_and_title_columns() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        // pinned defaults to 0 and title defaults to NULL for new rows.
        conn.execute(
            "INSERT INTO sessions (id, project_path) VALUES ('s1', '/tmp')",
            [],
        )
        .unwrap();
        let (pinned, title): (i64, Option<String>) = conn
            .query_row(
                "SELECT pinned, title FROM sessions WHERE id = 's1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pinned, 0);
        assert_eq!(title, None);
    }

    #[test]
    fn v5_adds_deferred_commands_table() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        // Insert a row relying on column defaults (status pending, created_at now).
        conn.execute(
            "INSERT INTO deferred_commands (session_id, command, cwd, workspace_root, reasons, verifier_note) \
             VALUES ('s1', 'rm -rf build', '/tmp/build', '/tmp', 'rm -rf', 'inside build dir')",
            [],
        )
        .unwrap();
        let (status, resolved): (String, Option<String>) = conn
            .query_row(
                "SELECT status, resolved_at FROM deferred_commands WHERE session_id = 's1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(resolved, None);
    }

    #[test]
    fn v6_adds_session_goal_column() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        // A fresh session has a NULL goal, and the column accepts writes/clears.
        conn.execute("INSERT INTO sessions (id) VALUES ('s1')", [])
            .unwrap();
        let goal: Option<String> = conn
            .query_row("SELECT goal FROM sessions WHERE id = 's1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(goal, None);
        conn.execute(
            "UPDATE sessions SET goal = 'ship the feature' WHERE id = 's1'",
            [],
        )
        .unwrap();
        let goal: Option<String> = conn
            .query_row("SELECT goal FROM sessions WHERE id = 's1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(goal.as_deref(), Some("ship the feature"));
        conn.execute("UPDATE sessions SET goal = NULL WHERE id = 's1'", [])
            .unwrap();
        let goal: Option<String> = conn
            .query_row("SELECT goal FROM sessions WHERE id = 's1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(goal, None);
    }

    #[test]
    fn legacy_v4_database_gains_deferred_commands_on_migrate() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a DB at schema v4 (no deferred_commands table). A real v4 DB has
        // sessions and tasks tables (later migrations ALTER them), so create minimal ones.
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE tasks (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL);
             PRAGMA user_version = 4;",
        )
        .unwrap();
        // deferred_commands does not exist yet.
        assert!(conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='deferred_commands'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap()
            .is_none());
        run_migrations(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
        // Now it exists and is usable.
        conn.execute(
            "INSERT INTO deferred_commands (session_id, command) VALUES ('s1', 'rm -rf /')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn legacy_v7_database_gains_task_project_path_and_agent_id_on_migrate() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a DB at schema v7: tasks table without the two new columns.
        conn.execute_batch(
            "CREATE TABLE tasks (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               name TEXT NOT NULL,
               description TEXT NOT NULL DEFAULT '',
               schedule TEXT NOT NULL DEFAULT '',
               next_run TEXT NOT NULL DEFAULT '',
               status TEXT NOT NULL DEFAULT 'scheduled',
               pending_input TEXT NOT NULL DEFAULT '',
               created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
               updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             PRAGMA user_version = 7;",
        )
        .unwrap();
        conn.execute("INSERT INTO tasks (name) VALUES ('pre-existing')", [])
            .unwrap();

        run_migrations(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Existing rows default to empty (default behavior); new values persist.
        let (pp, aid): (String, String) = conn
            .query_row(
                "SELECT project_path, agent_id FROM tasks WHERE name = 'pre-existing'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pp, "");
        assert_eq!(aid, "");
        conn.execute(
            "INSERT INTO tasks (name, project_path, agent_id) VALUES ('scoped', '/tmp/proj', 'default-research-agent')",
            [],
        )
        .unwrap();
        let (pp, aid): (String, String) = conn
            .query_row(
                "SELECT project_path, agent_id FROM tasks WHERE name = 'scoped'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pp, "/tmp/proj");
        assert_eq!(aid, "default-research-agent");
    }

    #[test]
    fn legacy_v6_database_gains_video_plans_on_migrate() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a DB at schema v6 (no video_plans table yet). A real v6 DB has a
        // tasks table (the v8 migration ALTERs it), so create a minimal one here.
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE tasks (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL);
             PRAGMA user_version = 6;",
        )
        .unwrap();
        assert!(conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='video_plans'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap()
            .is_none());
        run_migrations(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
        // Now it exists, is usable, and enforces path uniqueness (upsert target).
        conn.execute(
            "INSERT INTO video_plans (path, plan_json, updated_at) VALUES ('/tmp/a.mp4', '{}', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO video_plans (path, plan_json, updated_at) VALUES ('/tmp/a.mp4', '{}', '2026-01-02T00:00:00Z') \
             ON CONFLICT(path) DO UPDATE SET plan_json = excluded.plan_json, updated_at = excluded.updated_at",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM video_plans", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
