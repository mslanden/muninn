use rusqlite::{params, Connection};

use crate::AppState;

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

        CREATE TABLE IF NOT EXISTS saved_workspaces (
          path TEXT PRIMARY KEY,
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
          last_used_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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
          created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
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
          spino_md TEXT NOT NULL DEFAULT '',
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
        "#,
    )
    .map_err(|e| e.to_string())?;

    let _ = conn.execute(
        "ALTER TABLE sessions ADD COLUMN project_path TEXT NOT NULL DEFAULT ''",
        [],
    );

    // Migrate entities: add aliases_json, updated_at columns
    let _ = conn.execute(
        "ALTER TABLE entities ADD COLUMN aliases_json TEXT NOT NULL DEFAULT '[]'",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE entities ADD COLUMN updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
        [],
    );

    // Migrate memory_profile: add structured fields
    let _ = conn.execute(
        "ALTER TABLE memory_profile ADD COLUMN identity TEXT NOT NULL DEFAULT ''",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE memory_profile ADD COLUMN personality TEXT NOT NULL DEFAULT ''",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE memory_profile ADD COLUMN goals TEXT NOT NULL DEFAULT ''",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE memory_profile ADD COLUMN preferences TEXT NOT NULL DEFAULT ''",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE memory_profile ADD COLUMN current_state TEXT NOT NULL DEFAULT ''",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE memory_profile ADD COLUMN patterns TEXT NOT NULL DEFAULT ''",
        [],
    );

    // Create relationships table if not exists (migration for existing DBs)
    let _ = conn.execute_batch(
        r#"
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

        CREATE TABLE IF NOT EXISTS episode_entities (
          episode_id INTEGER NOT NULL,
          entity_id INTEGER NOT NULL,
          PRIMARY KEY(episode_id, entity_id),
          FOREIGN KEY(episode_id) REFERENCES episodes(id) ON DELETE CASCADE,
          FOREIGN KEY(entity_id) REFERENCES entities(id) ON DELETE CASCADE
        );
        "#,
    );

    conn.execute(
        r#"
        INSERT INTO memory_profile (id, user_name, user_about, profile_text, identity, personality, goals, preferences, current_state, patterns, updated_at)
        VALUES (1, '', '', '', '', '', '', '', '', '', CURRENT_TIMESTAMP)
        ON CONFLICT(id) DO NOTHING
        "#,
        [],
    )
    .map_err(|e| e.to_string())?;

    // Task schema migrations
    let _ = conn.execute("ALTER TABLE tasks ADD COLUMN completed_at TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE tasks ADD COLUMN result TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE tasks ADD COLUMN run_count INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE tasks ADD COLUMN triggered_by TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE tasks ADD COLUMN chain_next TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE tasks ADD COLUMN conditions TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE tasks ADD COLUMN condition_status TEXT NOT NULL DEFAULT ''", []);

    let default_tools = [
        (
            "run_command",
            "Run Command",
            "Run a shell command scoped to the current workspace.",
            "terminal",
        ),
        ("read_file", "Read File", "Read a file from workspace.", "file"),
        ("write_file", "Write File", "Write content to a workspace file.", "edit"),
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

pub(crate) fn with_db<T, F>(state: &AppState, f: F) -> Result<T, String>
where
    F: FnOnce(&Connection) -> Result<T, String>,
{
    let guard = state.db.lock().map_err(|_| "db lock poisoned".to_string())?;
    f(&guard)
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

pub(crate) fn normalize_project_path(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

use rusqlite::OptionalExtension;
