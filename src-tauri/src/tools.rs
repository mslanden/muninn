use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;
use crate::db::{add_feed_item, first_non_empty_setting, get_app_setting_value, with_db};
use crate::models::*;

#[derive(Clone)]
pub(crate) struct WorkspaceScope {
    pub root: PathBuf,
    pub root_canon: PathBuf,
}

impl WorkspaceScope {
    pub fn from_state(state: &AppState) -> Result<Self, String> {
        let root = state.workspace_root.lock().map_err(|_| "workspace lock poisoned".to_string())?.clone();
        let root_canon = state.workspace_root_canon.lock().map_err(|_| "workspace lock poisoned".to_string())?.clone();
        Ok(Self { root, root_canon })
    }

    pub fn from_path(path: &str) -> Result<Self, String> {
        let root = PathBuf::from(path);
        if !root.is_dir() {
            return Err(format!("project path does not exist: {}", path));
        }
        let root_canon = root.canonicalize().map_err(|e| format!("cannot canonicalize path: {}", e))?;
        Ok(Self { root, root_canon })
    }
}

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

pub(crate) fn enabled_tool_ids(state: &AppState) -> Result<HashSet<String>, String> {
    with_db(state, |db| {
        let mut stmt = db
            .prepare("SELECT tool_id FROM tools WHERE enabled = 1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    })
}

pub(crate) fn log_tool(state: &AppState, session_id: &str, kind: &str, input: &str, output: &str, status: &str) {
    use rusqlite::params;
    let _ = with_db(state, |db| {
        db.execute(
            "INSERT INTO tool_logs (session_id, kind, input, output, status) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, kind, input, output, status],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    });

    let icon_name = if status == "error" { "error" } else { "construction" };
    let tool_output = truncate_chars(output, 120);
    add_feed_item(
        state,
        "tool",
        &format!("Tool: {}", kind),
        &format!("{}: {}", status, tool_output),
        icon_name,
        session_id,
    );
}

pub(crate) fn is_dangerous_command(command: &str) -> SafetyCheck {
    let mut reasons = Vec::new();
    let lowered = command.to_lowercase();
    let patterns = [
        "rm -rf",
        "sudo",
        "mkfs",
        "dd if=",
        "shutdown",
        "reboot",
        "killall",
        "launchctl bootout",
        "chmod -r",
        "chown -r",
        ":(){",
    ];

    for pattern in patterns {
        if lowered.contains(pattern) {
            reasons.push(format!("Matched blocked pattern: {}", pattern));
        }
    }

    SafetyCheck {
        dangerous: !reasons.is_empty(),
        reasons,
    }
}

pub(crate) fn default_workspace_root() -> Result<(PathBuf, PathBuf), String> {
    let root = dirs::home_dir()
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "cannot determine home directory".to_string())?;
    let canon = root.canonicalize().map_err(|e| e.to_string())?;
    Ok((root, canon))
}

pub(crate) fn resolve_within_workspace(root: &Path, root_canon: &Path, path: &str, for_write: bool) -> Result<PathBuf, String> {
    let input = PathBuf::from(path);
    let candidate = if input.is_absolute() {
        input
    } else {
        root.join(input)
    };

    if for_write {
        let parent = candidate.parent().ok_or_else(|| "invalid target path".to_string())?;
        let parent_canon = parent.canonicalize().map_err(|e| format!("invalid parent path: {}", e))?;
        if !parent_canon.starts_with(root_canon) {
            return Err("path escapes workspace root".to_string());
        }
        return Ok(candidate);
    }

    let canon = candidate
        .canonicalize()
        .map_err(|e| format!("path error: {}", e))?;
    if !canon.starts_with(root_canon) {
        return Err("path escapes workspace root".to_string());
    }
    Ok(canon)
}

pub(crate) fn command_cwd(state: &AppState, cwd: Option<&str>) -> Result<PathBuf, String> {
    let root = state
        .workspace_root
        .lock()
        .map_err(|_| "workspace lock poisoned".to_string())?
        .clone();
    let root_canon = state
        .workspace_root_canon
        .lock()
        .map_err(|_| "workspace lock poisoned".to_string())?
        .clone();

    match cwd {
        Some(value) => resolve_within_workspace(&root, &root_canon, value, false),
        None => Ok(root),
    }
}

pub(crate) fn read_file_scoped(scope: &WorkspaceScope, req: &FileReadRequest) -> Result<FsResult, String> {
    let path = resolve_within_workspace(&scope.root, &scope.root_canon, &req.path, false)?;
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    let max_bytes = req.max_bytes.unwrap_or(32768);
    let clipped = if bytes.len() > max_bytes {
        &bytes[..max_bytes]
    } else {
        &bytes
    };
    let content = String::from_utf8_lossy(clipped).to_string();

    Ok(FsResult {
        path: path.display().to_string(),
        content,
    })
}

pub(crate) fn read_file_impl(state: &AppState, req: &FileReadRequest) -> Result<FsResult, String> {
    read_file_scoped(&WorkspaceScope::from_state(state)?, req)
}

pub(crate) fn write_file_scoped(scope: &WorkspaceScope, req: &FileWriteRequest) -> Result<FsResult, String> {
    let path = resolve_within_workspace(&scope.root, &scope.root_canon, &req.path, true)?;
    fs::write(&path, req.content.as_bytes()).map_err(|e| e.to_string())?;

    Ok(FsResult {
        path: path.display().to_string(),
        content: format!("Wrote {} bytes", req.content.len()),
    })
}

pub(crate) fn write_file_impl(state: &AppState, req: &FileWriteRequest) -> Result<FsResult, String> {
    write_file_scoped(&WorkspaceScope::from_state(state)?, req)
}

pub(crate) fn list_files_scoped(scope: &WorkspaceScope, req: &FileListRequest) -> Result<FsResult, String> {
    let dir = resolve_within_workspace(&scope.root, &scope.root_canon, &req.path, false)?;
    let mut out = String::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        let marker = if ty.is_dir() { "dir" } else { "file" };
        out.push_str(&format!("{}\t{}\n", marker, name));
    }

    Ok(FsResult {
        path: dir.display().to_string(),
        content: out,
    })
}

pub(crate) fn list_files_impl(state: &AppState, req: &FileListRequest) -> Result<FsResult, String> {
    list_files_scoped(&WorkspaceScope::from_state(state)?, req)
}

pub(crate) fn search_files_scoped(scope: &WorkspaceScope, req: &FileSearchRequest) -> Result<FsResult, String> {
    let dir = resolve_within_workspace(&scope.root, &scope.root_canon, &req.path, false)?;
    if which::which("rg").is_err() {
        return Err("`rg` is required for search".to_string());
    }

    let output = Command::new("rg")
        .arg("-n")
        .arg("--hidden")
        .arg("--glob")
        .arg("!.git")
        .arg(&req.pattern)
        .arg(&dir)
        .output()
        .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() && stdout.is_empty() {
        return Err(format!("rg failed: {}", stderr));
    }

    Ok(FsResult {
        path: dir.display().to_string(),
        content: if stdout.is_empty() {
            "No matches".to_string()
        } else {
            stdout
        },
    })
}

pub(crate) fn search_files_impl(state: &AppState, req: &FileSearchRequest) -> Result<FsResult, String> {
    search_files_scoped(&WorkspaceScope::from_state(state)?, req)
}

pub(crate) fn run_command_scoped(scope: &WorkspaceScope, command: &str, cwd: Option<&str>) -> Result<String, String> {
    let safety = is_dangerous_command(command);
    if safety.dangerous {
        return Err(format!(
            "Blocked dangerous command. Reasons: {}",
            safety.reasons.join("; ")
        ));
    }

    let cwd_path = match cwd {
        Some(value) => resolve_within_workspace(&scope.root, &scope.root_canon, value, false)?,
        None => scope.root.clone(),
    };
    let output = Command::new("/bin/zsh")
        .arg("-lc")
        .arg(command)
        .current_dir(cwd_path)
        .output()
        .map_err(|e| e.to_string())?;

    let mut merged = String::new();
    merged.push_str(&String::from_utf8_lossy(&output.stdout));
    merged.push_str(&String::from_utf8_lossy(&output.stderr));

    if merged.len() > 12000 {
        merged.truncate(12000);
        merged.push_str("\n...[truncated]");
    }

    Ok(merged)
}

#[allow(dead_code)]
pub(crate) fn run_command_capture(state: &AppState, command: &str, cwd: Option<&str>) -> Result<String, String> {
    let safety = is_dangerous_command(command);
    if safety.dangerous {
        return Err(format!(
            "Blocked dangerous command. Reasons: {}",
            safety.reasons.join("; ")
        ));
    }

    let cwd_path = command_cwd(state, cwd)?;
    let output = Command::new("/bin/zsh")
        .arg("-lc")
        .arg(command)
        .current_dir(cwd_path)
        .output()
        .map_err(|e| e.to_string())?;

    let mut merged = String::new();
    merged.push_str(&String::from_utf8_lossy(&output.stdout));
    merged.push_str(&String::from_utf8_lossy(&output.stderr));

    if merged.len() > 12000 {
        merged.truncate(12000);
        merged.push_str("\n...[truncated]");
    }

    Ok(merged)
}

pub(crate) async fn web_search_impl(state: &AppState, query: &str, count: usize) -> Result<String, String> {
    let api_key = first_non_empty_setting(state, &["search/brave_api_key"])
        .map_err(|e| e.to_string())?
        .or_else(|| std::env::var("BRAVE_API_KEY").ok())
        .ok_or_else(|| "Brave Search API key not configured. Set it in Settings.".to_string())?;

    let count = count.min(20).max(1);
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding::encode(query),
        count
    );

    let resp = state
        .http
        .get(&url)
        .header("Accept", "application/json")
        .header("X-Subscription-Token", &api_key)
        .send()
        .await
        .map_err(|e| format!("Brave search request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Brave search returned status {}", resp.status()));
    }

    let body: Value = resp.json().await.map_err(|e| format!("Failed to parse Brave response: {}", e))?;
    let mut out = String::new();
    if let Some(results) = body.get("web").and_then(|w| w.get("results")).and_then(|r| r.as_array()) {
        for (i, r) in results.iter().enumerate() {
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let desc = r.get("description").and_then(|v| v.as_str()).unwrap_or("");
            out.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, title, url, desc));
        }
    }
    if out.is_empty() {
        out = "No results found.".to_string();
    }
    Ok(out)
}

pub(crate) async fn web_fetch_impl(state: &AppState, url: &str) -> Result<String, String> {
    let resp = state
        .http
        .get(url)
        .header("User-Agent", "Spino/1.0")
        .send()
        .await
        .map_err(|e| format!("Fetch failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let text = resp.text().await.map_err(|e| format!("Failed to read body: {}", e))?;
    let plain = strip_html_tags(&text);
    let max = 12000;
    if plain.len() > max {
        Ok(format!("{}...[truncated]", &plain[..max]))
    } else {
        Ok(plain)
    }
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        if !in_tag && chars[i] == '<' {
            in_tag = true;
            let rest: String = lower_chars[i..].iter().take(10).collect();
            if rest.starts_with("<script") {
                in_script = true;
            } else if rest.starts_with("<style") {
                in_style = true;
            } else if rest.starts_with("</script") {
                in_script = false;
            } else if rest.starts_with("</style") {
                in_style = false;
            }
            i += 1;
            continue;
        }
        if in_tag {
            if chars[i] == '>' {
                in_tag = false;
            }
            i += 1;
            continue;
        }
        if in_script || in_style {
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }

    // Collapse whitespace
    let mut result = String::with_capacity(out.len());
    let mut prev_ws = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                result.push(if ch == '\n' { '\n' } else { ' ' });
            }
            prev_ws = true;
        } else {
            result.push(ch);
            prev_ws = false;
        }
    }
    result.trim().to_string()
}

pub(crate) async fn generate_image_impl(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    prompt: &str,
    style: Option<&str>,
) -> Result<String, String> {
    let provider = get_app_setting_value(state, "imagegen/provider")?
        .unwrap_or_else(|| "openai".to_string());
    let model_setting = get_app_setting_value(state, "imagegen/model")?
        .unwrap_or_default();

    let enhanced_prompt = match style {
        Some("realistic") => format!("Photorealistic, high quality: {}", prompt),
        Some("artistic") => format!("Artistic, painterly style: {}", prompt),
        Some("cartoon") => format!("Cartoon style, vibrant colors: {}", prompt),
        Some("abstract") => format!("Abstract, modern art: {}", prompt),
        Some(other) if !other.is_empty() => format!("{} style: {}", other, prompt),
        _ => prompt.to_string(),
    };

    let (api_url, body, api_key) = if provider == "together" {
        let key = first_non_empty_setting(state, &["imagegen/together_api_key", "llm/together_api_key"])?
            .ok_or_else(|| "Together AI API key not configured. Set it in Settings.".to_string())?;
        let model = if model_setting.is_empty() {
            "black-forest-labs/FLUX.1-schnell".to_string()
        } else {
            model_setting
        };
        (
            "https://api.together.xyz/v1/images/generations".to_string(),
            serde_json::json!({
                "model": model,
                "prompt": enhanced_prompt,
                "width": 1024,
                "height": 1024,
                "n": 1
            }),
            key,
        )
    } else {
        let key = first_non_empty_setting(state, &["llm/openai_api_key"])?
            .ok_or_else(|| "OpenAI API key not configured. Set it in Settings.".to_string())?;
        let model = if model_setting.is_empty() {
            "dall-e-3".to_string()
        } else {
            model_setting
        };
        (
            "https://api.openai.com/v1/images/generations".to_string(),
            serde_json::json!({
                "model": model,
                "prompt": enhanced_prompt,
                "n": 1,
                "size": "1024x1024"
            }),
            key,
        )
    };

    let resp = state
        .http
        .post(&api_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Image generation request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(format!("Image API returned {}: {}", status, err_body));
    }

    let resp_json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse image response: {}", e))?;

    let image_url = resp_json
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|item| {
            item.get("url")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("b64_json").and_then(|v| v.as_str()))
        })
        .ok_or_else(|| "No image data in response".to_string())?;

    let image_bytes = if image_url.starts_with("http") {
        state
            .http
            .get(image_url)
            .send()
            .await
            .map_err(|e| format!("Failed to download image: {}", e))?
            .bytes()
            .await
            .map_err(|e| format!("Failed to read image bytes: {}", e))?
            .to_vec()
    } else {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(image_url)
            .map_err(|e| format!("Failed to decode base64 image: {}", e))?
    };

    let dest_dir = content_dir(app)?;
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let filename = format!("spinosaur-{}.png", timestamp);
    let dest_path = dest_dir.join(&filename);
    fs::write(&dest_path, &image_bytes).map_err(|e| format!("Failed to save image: {}", e))?;

    let _ = app.emit("content-changed", ());
    let _ = app.emit(
        "image-generated",
        serde_json::json!({
            "session_id": session_id,
            "path": dest_path.display().to_string(),
            "prompt": prompt
        }),
    );

    Ok("Image generated and saved to content folder.".to_string())
}

pub(crate) async fn search_places_impl(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    query: &str,
    limit: usize,
) -> Result<String, String> {
    let limit = limit.min(20).max(1);
    let url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=json&limit={}&addressdetails=1",
        urlencoding::encode(query),
        limit
    );

    let resp = state
        .http
        .get(&url)
        .header("User-Agent", "Spino/0.1.0")
        .send()
        .await
        .map_err(|e| format!("Nominatim request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Nominatim returned status {}", resp.status()));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Nominatim response: {}", e))?;

    let items = body.as_array().ok_or_else(|| "unexpected response format".to_string())?;

    let mut results = Vec::new();
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        let name = item
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or("Unknown");
        let lat = item.get("lat").and_then(Value::as_str).unwrap_or("0");
        let lon = item.get("lon").and_then(Value::as_str).unwrap_or("0");
        let place_type = item.get("type").and_then(Value::as_str).unwrap_or("place");

        results.push(serde_json::json!({
            "name": name,
            "lat": lat.parse::<f64>().unwrap_or(0.0),
            "lon": lon.parse::<f64>().unwrap_or(0.0),
            "type": place_type,
        }));

        out.push_str(&format!("{}. {} ({}) [{}, {}]\n", i + 1, name, place_type, lat, lon));
    }

    if results.is_empty() {
        out = "No places found.".to_string();
    }

    let _ = app.emit(
        "map-places",
        serde_json::json!({
            "session_id": session_id,
            "query": query,
            "results": results,
        }),
    );

    Ok(out)
}

pub(crate) async fn get_directions_impl(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    from: &str,
    to: &str,
    _mode: Option<&str>,
) -> Result<String, String> {
    // Geocode "from"
    let from_url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=json&limit=1",
        urlencoding::encode(from)
    );
    let from_resp = state
        .http
        .get(&from_url)
        .header("User-Agent", "Spino/0.1.0")
        .send()
        .await
        .map_err(|e| format!("Geocoding 'from' failed: {}", e))?;
    let from_body: Value = from_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse geocode response: {}", e))?;
    let from_item = from_body
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| format!("Could not geocode '{}'", from))?;
    let from_lat: f64 = from_item
        .get("lat")
        .and_then(Value::as_str)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let from_lon: f64 = from_item
        .get("lon")
        .and_then(Value::as_str)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let from_name = from_item
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or(from);

    // Rate limit delay
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Geocode "to"
    let to_url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=json&limit=1",
        urlencoding::encode(to)
    );
    let to_resp = state
        .http
        .get(&to_url)
        .header("User-Agent", "Spino/0.1.0")
        .send()
        .await
        .map_err(|e| format!("Geocoding 'to' failed: {}", e))?;
    let to_body: Value = to_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse geocode response: {}", e))?;
    let to_item = to_body
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| format!("Could not geocode '{}'", to))?;
    let to_lat: f64 = to_item
        .get("lat")
        .and_then(Value::as_str)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let to_lon: f64 = to_item
        .get("lon")
        .and_then(Value::as_str)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let to_name = to_item
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or(to);

    // Get route from OSRM (public server only supports driving)
    let route_url = format!(
        "https://router.project-osrm.org/route/v1/driving/{},{};{},{}?overview=full&geometries=geojson&steps=true",
        from_lon, from_lat, to_lon, to_lat
    );
    let route_resp = state
        .http
        .get(&route_url)
        .header("User-Agent", "Spino/0.1.0")
        .send()
        .await
        .map_err(|e| format!("OSRM request failed: {}", e))?;
    let route_body: Value = route_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse OSRM response: {}", e))?;

    let route = route_body
        .get("routes")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| "No route found".to_string())?;

    let distance_m = route
        .get("distance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let duration_s = route
        .get("duration")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let distance_km = distance_m / 1000.0;
    let duration_min = duration_s / 60.0;

    // Extract geometry coordinates
    let geometry_coords = route
        .get("geometry")
        .and_then(|g| g.get("coordinates"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Extract step instructions
    let mut steps = Vec::new();
    if let Some(legs) = route.get("legs").and_then(Value::as_array) {
        for leg in legs {
            if let Some(leg_steps) = leg.get("steps").and_then(Value::as_array) {
                for step in leg_steps {
                    let maneuver = step.get("maneuver");
                    let instruction = maneuver
                        .and_then(|m| m.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("continue");
                    let modifier = maneuver
                        .and_then(|m| m.get("modifier"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let step_name = step.get("name").and_then(Value::as_str).unwrap_or("");
                    let step_dist = step.get("distance").and_then(Value::as_f64).unwrap_or(0.0);

                    let instr_text = if step_name.is_empty() {
                        if modifier.is_empty() {
                            instruction.to_string()
                        } else {
                            format!("{} {}", instruction, modifier)
                        }
                    } else if modifier.is_empty() {
                        format!("{} on {}", instruction, step_name)
                    } else {
                        format!("{} {} on {}", instruction, modifier, step_name)
                    };

                    steps.push(serde_json::json!({
                        "instruction": instr_text,
                        "distance": step_dist as i64,
                    }));
                }
            }
        }
    }

    let _ = app.emit(
        "map-route",
        serde_json::json!({
            "session_id": session_id,
            "from": from_name,
            "to": to_name,
            "distance_km": distance_km,
            "duration_min": duration_min,
            "geometry": geometry_coords,
            "steps": steps,
        }),
    );

    // Build text summary for LLM
    let dist_text = if distance_km < 1.0 {
        format!("{}m", (distance_km * 1000.0) as i64)
    } else {
        format!("{:.1}km", distance_km)
    };
    let dur_text = if duration_min < 60.0 {
        format!("{} min", duration_min as i64)
    } else {
        format!("{}h {}m", (duration_min / 60.0) as i64, (duration_min % 60.0) as i64)
    };

    let mut out = format!(
        "Directions from {} to {}\nDistance: {} | Duration: {}\n\nSteps:\n",
        from_name, to_name, dist_text, dur_text
    );
    for (i, step) in steps.iter().enumerate() {
        let instr = step.get("instruction").and_then(Value::as_str).unwrap_or("");
        let dist = step.get("distance").and_then(Value::as_i64).unwrap_or(0);
        out.push_str(&format!("{}. {} ({}m)\n", i + 1, instr, dist));
    }

    Ok(out)
}

pub(crate) async fn execute_agent_tool(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    enabled: &HashSet<String>,
    tool_name: &str,
    arguments: &Value,
    scope: Option<&WorkspaceScope>,
) -> Result<String, String> {
    if !enabled.contains(tool_name) {
        return Err(format!("tool '{}' is disabled", tool_name));
    }

    let args_text = arguments.to_string();
    let _ = app.emit(
        "tool-start",
        ToolStartEvent {
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments: args_text.clone(),
        },
    );

    let tool_name = match tool_name {
        "shell" | "bash" | "zsh" => "run_command",
        "cat" | "read" => "read_file",
        "write" | "save" => "write_file",
        "ls" | "dir" => "list_files",
        "grep" | "rg" | "find" => "search_files",
        "search" | "brave" | "brave_search" => "web_search",
        "fetch" | "curl" | "wget" => "web_fetch",
        "imagine" | "image" | "draw" => "generate_image",
        "places" | "find_places" | "nearby" => "search_places",
        "directions" | "route" | "navigate" => "get_directions",
        other => other,
    };

    if !enabled.contains(tool_name) {
        return Err(format!("tool '{}' is disabled", tool_name));
    }

    // If scope is provided, use scoped variants for file/command tools
    let effective_scope = match scope {
        Some(s) => s.clone(),
        None => WorkspaceScope::from_state(state)?,
    };

    let result = match tool_name {
        "run_command" => {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing command".to_string())?;
            let cwd = arguments.get("cwd").and_then(Value::as_str);
            run_command_scoped(&effective_scope, command, cwd)
        }
        "read_file" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing path".to_string())?;
            let result = read_file_scoped(
                &effective_scope,
                &FileReadRequest {
                    path: path.to_string(),
                    max_bytes: Some(32000),
                },
            )?;
            Ok(result.content)
        }
        "write_file" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing path".to_string())?;
            let content = arguments
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            let result = write_file_scoped(
                &effective_scope,
                &FileWriteRequest {
                    path: path.to_string(),
                    content: content.to_string(),
                },
            )?;
            Ok(result.content)
        }
        "list_files" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing path".to_string())?;
            let result = list_files_scoped(
                &effective_scope,
                &FileListRequest {
                    path: path.to_string(),
                },
            )?;
            Ok(result.content)
        }
        "search_files" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing path".to_string())?;
            let pattern = arguments
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing pattern".to_string())?;
            let result = search_files_scoped(
                &effective_scope,
                &FileSearchRequest {
                    path: path.to_string(),
                    pattern: pattern.to_string(),
                },
            )?;
            Ok(result.content)
        }
        "web_search" => {
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing query".to_string())?;
            let count = arguments
                .get("count")
                .and_then(Value::as_u64)
                .unwrap_or(5) as usize;
            web_search_impl(state, query, count).await
        }
        "web_fetch" => {
            let url = arguments
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing url".to_string())?;
            web_fetch_impl(state, url).await
        }
        "generate_image" => {
            let prompt = arguments
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing prompt".to_string())?;
            let style = arguments.get("style").and_then(Value::as_str);
            generate_image_impl(app, state, session_id, prompt, style).await
        }
        "search_places" => {
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing query".to_string())?;
            let limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(5) as usize;
            search_places_impl(app, state, session_id, query, limit).await
        }
        "get_directions" => {
            let from = arguments
                .get("from")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'from' address".to_string())?;
            let to = arguments
                .get("to")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing 'to' address".to_string())?;
            let mode = arguments.get("mode").and_then(Value::as_str);
            get_directions_impl(app, state, session_id, from, to, mode).await
        }
        _ => Err(format!("unknown tool: {}", tool_name)),
    };

    let status = if result.is_ok() { "ok" } else { "error" };
    let output = result.as_ref().unwrap_or(&String::new()).clone();

    let _ = app.emit(
        "tool-complete",
        ToolCompleteEvent {
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            output,
            status: status.to_string(),
        },
    );

    result
}

pub(crate) fn parse_tool_code_block(text: &str) -> Option<(String, String)> {
    let start = text.find("<tool_code>")?;
    let end = text.find("</tool_code>")?;
    let inner = &text[start + 11..end];

    let tool_start = inner.find("tool => ")? + 8;
    let args_start = inner.find("args => ")? + 8;

    let tool_name_raw = inner[tool_start..].trim().trim_matches('\'');
    let args = inner[args_start..].trim().trim_matches('\'').trim();

    let tool_name = match tool_name_raw {
        "shell" | "bash" | "zsh" => "run_command",
        "cat" | "read" => "read_file",
        "write" | "save" => "write_file",
        "ls" | "dir" => "list_files",
        "grep" | "rg" | "find" => "search_files",
        "search" | "brave" | "brave_search" => "web_search",
        "fetch" | "curl" | "wget" => "web_fetch",
        "imagine" | "image" | "draw" => "generate_image",
        "places" | "find_places" | "nearby" => "search_places",
        "directions" | "route" | "navigate" => "get_directions",
        other => other,
    };

    Some((tool_name.to_string(), args.to_string()))
}

pub(crate) fn gallery_dirs(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let images_dir = data_dir.join("media").join("images");
    let audio_dir = data_dir.join("media").join("audio");
    fs::create_dir_all(&images_dir).map_err(|e| e.to_string())?;
    fs::create_dir_all(&audio_dir).map_err(|e| e.to_string())?;
    Ok((images_dir, audio_dir))
}

pub(crate) fn content_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let content = data_dir.join("content");
    fs::create_dir_all(&content).map_err(|e| e.to_string())?;
    Ok(content)
}

fn ext_to_kind(ext: &str) -> &'static str {
    match ext {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg" | "bmp" | "ico" => "image",
        "wav" | "mp3" | "m4a" | "aac" | "flac" | "ogg" | "wma" => "audio",
        "mp4" | "mov" | "avi" | "mkv" | "webm" => "video",
        "pdf" => "pdf",
        "txt" | "md" | "csv" | "json" | "xml" | "yaml" | "yml" | "toml" => "text",
        _ => "file",
    }
}

pub(crate) fn list_content_files(dir: &Path) -> Result<Vec<MediaItemRecord>, String> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        let kind = ext_to_kind(&ext).to_string();
        let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        out.push(MediaItemRecord {
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            path: path.display().to_string(),
            kind,
            size: metadata.len() as i64,
            modified_at: modified,
        });
    }
    out.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    Ok(out)
}

pub(crate) fn list_media_in_dir(dir: &Path, extensions: &[&str]) -> Result<Vec<MediaItemRecord>, String> {
    let mut out = Vec::new();
    let set: HashSet<String> = extensions.iter().map(|e| e.to_string()).collect();
    let entries = fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        if !set.contains(&ext) {
            continue;
        }
        let kind = ext_to_kind(&ext).to_string();
        let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        out.push(MediaItemRecord {
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            path: path.display().to_string(),
            kind,
            size: metadata.len() as i64,
            modified_at: modified,
        });
    }
    out.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    Ok(out)
}

pub(crate) fn upsert_saved_workspace(state: &AppState, path: &str) -> Result<(), String> {
    use rusqlite::params;
    with_db(state, |db| {
        db.execute(
            r#"
            INSERT INTO saved_workspaces (path, last_used_at)
            VALUES (?1, CURRENT_TIMESTAMP)
            ON CONFLICT(path) DO UPDATE SET last_used_at = CURRENT_TIMESTAMP
            "#,
            params![path],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}
