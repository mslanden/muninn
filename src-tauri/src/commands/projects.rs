//! Project CRUD and the init_project onboarding flow.

use rusqlite::params;
use std::fs;
use tauri::{AppHandle, Emitter, State};


use crate::AppState;
use crate::db::*;
use crate::models::*;
use crate::tools::*;

#[tauri::command]
pub fn get_projects(state: State<AppState>) -> Result<Vec<ProjectRecord>, String> {
    with_db(&state, |db| {
        let mut stmt = db
            .prepare("SELECT path, name, agents_md, created_at, last_opened_at FROM projects ORDER BY last_opened_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    agents_md: row.get(2)?,
                    created_at: row.get(3)?,
                    last_opened_at: row.get(4)?,
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
pub fn add_project(state: State<AppState>, req: ProjectAddRequest) -> Result<ProjectRecord, String> {
    let path = req.path.trim().to_string();
    if path.is_empty() {
        return Err("path is required".to_string());
    }
    let p = std::path::Path::new(&path);
    if !p.is_dir() {
        return Err(format!("directory does not exist: {}", path));
    }
    let name = req.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&path)
            .to_string()
    });

    // One-time migration: if AGENTS.md doesn't exist but a legacy spino.md does, rename it.
    let agents_path = p.join("AGENTS.md");
    let legacy_path = p.join("spino.md");
    if !agents_path.is_file() && legacy_path.is_file() {
        let _ = fs::rename(&legacy_path, &agents_path);
    }

    // Read AGENTS.md if present
    let agents_md = if agents_path.is_file() {
        fs::read_to_string(&agents_path).unwrap_or_default()
    } else {
        String::new()
    };

    with_db(&state, |db| {
        db.execute(
            r#"
            INSERT INTO projects (path, name, agents_md, created_at, last_opened_at)
            VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(path) DO UPDATE SET name = ?2, agents_md = ?3, last_opened_at = CURRENT_TIMESTAMP
            "#,
            params![path, name, agents_md],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    with_db(&state, |db| {
        db.query_row(
            "SELECT path, name, agents_md, created_at, last_opened_at FROM projects WHERE path = ?1",
            params![path],
            |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    agents_md: row.get(2)?,
                    created_at: row.get(3)?,
                    last_opened_at: row.get(4)?,
                })
            },
        )
        .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub fn remove_project(state: State<AppState>, req: ProjectPathRequest) -> Result<(), String> {
    with_db(&state, |db| {
        db.execute("DELETE FROM projects WHERE path = ?1", params![req.path])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub fn open_project(state: State<AppState>, req: ProjectPathRequest) -> Result<ProjectRecord, String> {
    let path = req.path.trim().to_string();
    let p = std::path::Path::new(&path);

    // One-time migration: if AGENTS.md doesn't exist but a legacy spino.md does, rename it.
    let agents_path = p.join("AGENTS.md");
    let legacy_path = p.join("spino.md");
    if !agents_path.is_file() && legacy_path.is_file() {
        let _ = fs::rename(&legacy_path, &agents_path);
    }

    // Re-read AGENTS.md from disk
    let agents_md = if agents_path.is_file() {
        fs::read_to_string(&agents_path).unwrap_or_default()
    } else {
        String::new()
    };

    with_db(&state, |db| {
        db.execute(
            "UPDATE projects SET last_opened_at = CURRENT_TIMESTAMP, agents_md = ?2 WHERE path = ?1",
            params![path, agents_md],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    with_db(&state, |db| {
        db.query_row(
            "SELECT path, name, agents_md, created_at, last_opened_at FROM projects WHERE path = ?1",
            params![path],
            |row| {
                Ok(ProjectRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    agents_md: row.get(2)?,
                    created_at: row.get(3)?,
                    last_opened_at: row.get(4)?,
                })
            },
        )
        .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub async fn init_project(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ProjectInitRequest,
) -> Result<String, String> {
    let path = req.path.trim().to_string();
    let p = std::path::Path::new(&path);
    if !p.is_dir() {
        return Err(format!("directory does not exist: {}", path));
    }

    let scope = WorkspaceScope::from_path(&path)?;

    // Create a temporary session for streaming events
    let session_id = uuid::Uuid::new_v4().to_string();

    let prompt = format!(
        "You are analyzing the project at: {path}\n\n\
        Use the provided tools to explore the project thoroughly:\n\
        - Use list_files to understand the directory structure (check subdirectories too)\n\
        - Use read_file to read key files (README, config files, entry points, important source files)\n\
        - Use run_command to check git log, dependency info, etc.\n\
        - Use search_files to find patterns if needed\n\n\
        After exploring, generate a concise AGENTS.md file (under 100 lines, markdown) that describes:\n\
        1. What the project is (name, purpose)\n\
        2. Tech stack and key dependencies\n\
        3. Project structure overview\n\
        4. How to build/run/test\n\
        5. Key files and their purposes\n\n\
        Output the final AGENTS.md content as your last message, with no preamble like \"Here is the AGENTS.md\"."
    );

    // Init tools — read-only exploration only
    let init_tools: std::collections::HashSet<String> = [
        "read_file", "list_files", "search_files", "run_command",
    ].iter().map(|s| s.to_string()).collect();

    let tools_json = vec![
        serde_json::json!({
            "name": "run_command",
            "description": "Run a shell command in the project directory and return output.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string"}
                },
                "required": ["command"]
            }
        }),
        serde_json::json!({
            "name": "read_file",
            "description": "Read a file within the project.",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "list_files",
            "description": "List files in a project directory.",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "search_files",
            "description": "Search text recursively for a regex pattern in workspace files.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "pattern": {"type": "string"}
                },
                "required": ["path", "pattern"]
            }
        }),
    ];

    // Determine provider/model
    let configured_provider = get_app_setting_value(&state, "llm/provider")?.unwrap_or_else(|| "minimax".to_string());
    let model = get_app_setting_value(&state, "llm/model")?
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| if configured_provider == "minimax" { "MiniMax-M2.7".to_string() } else { "gpt-4.1-mini".to_string() });

    let is_anthropic = configured_provider == "minimax" || configured_provider == "anthropic" || model.starts_with("MiniMax-");

    let system_prompt = format!(
        "You are Muninn, a project analyzer. You explore projects using tools and generate an AGENTS.md description file. \
        The project root is: {path}. All file paths are relative to this root. Be thorough but efficient — \
        explore the structure, read key files, then write a concise summary."
    );

    let result = if is_anthropic {
        let api_key = first_non_empty_setting(&state, &["llm/minimax_api_key", "llm/anthropic_api_key"])?
            .or_else(|| std::env::var("MINIMAX_API_KEY").ok())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| "API key not configured".to_string())?;
        let base = std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimax.io/anthropic".to_string());
        let endpoint = if base.ends_with("/v1/messages") { base.clone() } else { format!("{}/v1/messages", base.trim_end_matches('/')) };

        let mut messages: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": prompt}]})
        ];

        let final_text;

        loop {
            let request_body = serde_json::json!({
                "model": model,
                "max_tokens": 4096,
                "system": system_prompt,
                "messages": messages,
                "tools": tools_json,
                "stream": true
            });

            let response = state.http.post(&endpoint)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&request_body)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let status = response.status();
            if !status.is_success() {
                let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
                return Err(format!("API error {}: {}", status, body));
            }

            // Parse SSE stream
            let mut content_acc = String::new();
            let mut tool_uses: Vec<(String, String, String)> = Vec::new();
            let mut current_event_type = String::new();
            let mut line_buf = String::new();

            let mut stream = response;
            while let Some(bytes) = stream.chunk().await.map_err(|e| e.to_string())? {
                let text = String::from_utf8_lossy(&bytes);
                line_buf.push_str(&text);

                while let Some(newline_pos) = line_buf.find('\n') {
                    let line = line_buf[..newline_pos].trim().to_string();
                    line_buf = line_buf[newline_pos + 1..].to_string();

                    if line.starts_with("event: ") {
                        current_event_type = line[7..].to_string();
                        continue;
                    }
                    if line.is_empty() || !line.starts_with("data: ") { continue; }
                    let data: serde_json::Value = match serde_json::from_str(&line[6..]) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    match current_event_type.as_str() {
                        "content_block_start" => {
                            if let Some(block) = data.get("content_block") {
                                if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    tool_uses.push((id, name, String::new()));
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = data.get("delta") {
                                let dt = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                if dt == "text_delta" {
                                    if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                        content_acc.push_str(t);
                                        let _ = app.emit("chat-token", ChatTokenEvent {
                                            session_id: session_id.clone(),
                                            token: t.to_string(),
                                        });
                                    }
                                } else if dt == "input_json_delta" {
                                    if let Some(chunk) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                        if let Some(last) = tool_uses.last_mut() {
                                            last.2.push_str(chunk);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            // If tool calls, execute them in parallel and continue the loop
            if !tool_uses.is_empty() {
                let mut content_blocks: Vec<serde_json::Value> = Vec::new();
                if !content_acc.is_empty() {
                    content_blocks.push(serde_json::json!({"type": "text", "text": content_acc}));
                }
                for (id, name, input_str) in &tool_uses {
                    let input: serde_json::Value = serde_json::from_str(input_str).unwrap_or_else(|_| serde_json::json!({}));
                    content_blocks.push(serde_json::json!({
                        "type": "tool_use", "id": id, "name": name, "input": input
                    }));
                }
                messages.push(serde_json::json!({"role": "assistant", "content": content_blocks}));

                // Emit "running" for all tools first
                for (_, tool_name, input_str) in &tool_uses {
                    let args: serde_json::Value = serde_json::from_str(input_str).unwrap_or_else(|_| serde_json::json!({}));
                    let _ = app.emit("tool-executed", serde_json::json!({
                        "session_id": session_id,
                        "tool": tool_name,
                        "args": args.to_string(),
                        "status": "running"
                    }));
                }

                // Execute all tools in parallel
                let futures: Vec<_> = tool_uses.iter().map(|(_, tool_name, input_str)| {
                    let args: serde_json::Value = serde_json::from_str(input_str).unwrap_or_else(|_| serde_json::json!({}));
                    let app = app.clone();
                    let state_ref = &state;
                    let session_id = session_id.clone();
                    let init_tools = init_tools.clone();
                    let scope = scope.clone();
                    let tool_name = tool_name.clone();
                    async move {
                        // Project-init analyzer is internal, not driven by a present user.
                        let output = match execute_agent_tool(&app, state_ref, &session_id, &init_tools, &tool_name, &args, Some(&scope), false).await {
                            Ok(o) => {
                                let _ = app.emit("tool-executed", serde_json::json!({
                                    "session_id": session_id,
                                    "tool": tool_name,
                                    "args": args.to_string(),
                                    "status": "ok"
                                }));
                                o
                            }
                            Err(e) => {
                                let _ = app.emit("tool-executed", serde_json::json!({
                                    "session_id": session_id,
                                    "tool": tool_name,
                                    "args": args.to_string(),
                                    "status": "error"
                                }));
                                format!("Tool error: {}", e)
                            }
                        };
                        output
                    }
                }).collect();

                let results = futures::future::join_all(futures).await;

                for (i, output) in results.into_iter().enumerate() {
                    let tool_id = &tool_uses[i].0;
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": output}]
                    }));
                }
                continue;
            }

            // No tool calls — this is the final response
            final_text = content_acc;
            break;
        }

        let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.clone() });
        final_text
    } else {
        // OpenAI-compatible path — same streaming tool loop
        let api_key = first_non_empty_setting(&state, &["llm/openai_api_key"])?
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| "OpenAI API key not configured".to_string())?;

        let openai_tools: Vec<serde_json::Value> = tools_json.iter().map(|t| serde_json::json!({
            "type": "function",
            "function": {
                "name": t["name"],
                "description": t["description"],
                "parameters": t["input_schema"]
            }
        })).collect();

        let mut messages: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "system", "content": system_prompt}),
            serde_json::json!({"role": "user", "content": prompt}),
        ];

        let final_text;

        loop {
            let request_body = serde_json::json!({
                "model": model,
                "messages": messages,
                "tools": openai_tools,
                "stream": true
            });

            let response = state.http.post("https://api.openai.com/v1/chat/completions")
                .bearer_auth(&api_key)
                .json(&request_body)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let status = response.status();
            if !status.is_success() {
                let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
                return Err(format!("API error {}: {}", status, body));
            }

            let mut content_acc = String::new();
            let mut tool_calls: Vec<(String, String, String)> = Vec::new(); // (id, name, args)
            let mut line_buf = String::new();

            let mut stream = response;
            while let Some(bytes) = stream.chunk().await.map_err(|e| e.to_string())? {
                let text = String::from_utf8_lossy(&bytes);
                line_buf.push_str(&text);

                while let Some(newline_pos) = line_buf.find('\n') {
                    let line = line_buf[..newline_pos].trim().to_string();
                    line_buf = line_buf[newline_pos + 1..].to_string();

                    if !line.starts_with("data: ") || line == "data: [DONE]" { continue; }
                    let data: serde_json::Value = match serde_json::from_str(&line[6..]) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
                        for choice in choices {
                            let delta = match choice.get("delta") {
                                Some(d) => d,
                                None => continue,
                            };
                            if let Some(t) = delta.get("content").and_then(|v| v.as_str()) {
                                content_acc.push_str(t);
                                let _ = app.emit("chat-token", ChatTokenEvent {
                                    session_id: session_id.clone(),
                                    token: t.to_string(),
                                });
                            }
                            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                                for tc in tcs {
                                    let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                                    while tool_calls.len() <= idx {
                                        tool_calls.push((String::new(), String::new(), String::new()));
                                    }
                                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                        tool_calls[idx].0 = id.to_string();
                                    }
                                    if let Some(f) = tc.get("function") {
                                        if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                                            tool_calls[idx].1 = name.to_string();
                                        }
                                        if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                                            tool_calls[idx].2.push_str(args);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !tool_calls.is_empty() {
                let tc_json: Vec<serde_json::Value> = tool_calls.iter().map(|(id, name, args)| {
                    serde_json::json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
                }).collect();
                let mut assistant_msg = serde_json::json!({"role": "assistant", "tool_calls": tc_json});
                if !content_acc.is_empty() {
                    assistant_msg["content"] = serde_json::json!(content_acc);
                }
                messages.push(assistant_msg);

                // Emit "running" for all tools first
                for (_, tool_name, args_str) in &tool_calls {
                    let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or_else(|_| serde_json::json!({}));
                    let _ = app.emit("tool-executed", serde_json::json!({
                        "session_id": session_id,
                        "tool": tool_name,
                        "args": args.to_string(),
                        "status": "running"
                    }));
                }

                // Execute all tools in parallel
                let futures: Vec<_> = tool_calls.iter().map(|(_, tool_name, args_str)| {
                    let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or_else(|_| serde_json::json!({}));
                    let app = app.clone();
                    let state_ref = &state;
                    let session_id = session_id.clone();
                    let init_tools = init_tools.clone();
                    let scope = scope.clone();
                    let tool_name = tool_name.clone();
                    async move {
                        // Project-init analyzer is internal, not driven by a present user.
                        let output = match execute_agent_tool(&app, state_ref, &session_id, &init_tools, &tool_name, &args, Some(&scope), false).await {
                            Ok(o) => {
                                let _ = app.emit("tool-executed", serde_json::json!({
                                    "session_id": session_id,
                                    "tool": tool_name,
                                    "args": args.to_string(),
                                    "status": "ok"
                                }));
                                o
                            }
                            Err(e) => {
                                let _ = app.emit("tool-executed", serde_json::json!({
                                    "session_id": session_id,
                                    "tool": tool_name,
                                    "args": args.to_string(),
                                    "status": "error"
                                }));
                                format!("Tool error: {}", e)
                            }
                        };
                        output
                    }
                }).collect();

                let results = futures::future::join_all(futures).await;

                for (i, output) in results.into_iter().enumerate() {
                    let tool_id = &tool_calls[i].0;
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": tool_id,
                        "content": output
                    }));
                }
                continue;
            }

            final_text = content_acc;
            break;
        }

        let _ = app.emit("chat-done", ChatDoneEvent { session_id: session_id.clone() });
        final_text
    };

    if result.is_empty() {
        return Err("Init completed but no content was generated".to_string());
    }

    // Write AGENTS.md to project directory
    let agents_path = p.join("AGENTS.md");
    fs::write(&agents_path, &result).map_err(|e| format!("Failed to write AGENTS.md: {}", e))?;

    // Update projects table cache
    with_db(&state, |db| {
        db.execute(
            "UPDATE projects SET agents_md = ?2 WHERE path = ?1",
            params![path, result],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })?;

    Ok(result)
}
