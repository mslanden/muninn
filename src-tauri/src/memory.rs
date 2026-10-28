//! Model-managed long-term memory: a folder of markdown notes
//! (`AppState.memory_dir`, i.e. `app_data_dir()/memory`) that the model reads and
//! writes itself through the `memory` tool, and the user can browse/edit in the
//! Memory tile.
//!
//! Each note is `<name>.md` with a one-line `description:` in front matter. Every
//! turn the system prompt gets the user's profile settings, the pinned `about-me`
//! note in full, and a capped index of the other notes (name — description); the
//! model reads a note on demand when it needs the details. There is no background
//! extraction and no embedding retrieval — the model decides what to remember.
//!
//! Note names are sanitized to `[a-z0-9_-]`, so a note path can never escape the
//! memory folder.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::db::get_app_setting_value;
use crate::AppState;

/// The note that is always shown in full (facts about the user themselves).
pub(crate) const PINNED_NOTE: &str = "about-me";
/// Character budget for the note index injected into every prompt.
const INDEX_BUDGET: usize = 4_000;
/// The pinned note is truncated past this many characters in the prompt.
const PINNED_BUDGET: usize = 3_000;
/// Hard cap on a single note body; keeps notes notes, not documents.
const MAX_NOTE_CHARS: usize = 32_000;
const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 200;
const MAX_SEARCH_HITS: usize = 30;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteMeta {
    pub name: String,
    pub description: String,
    /// Last modified, unix seconds.
    pub updated_at: u64,
    pub chars: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    #[serde(flatten)]
    pub meta: NoteMeta,
    pub body: String,
}

/// Normalize a user/model-supplied note name to a safe file stem: lowercase,
/// whitespace → '-', only `[a-z0-9_-]`, no leading/trailing dashes, an optional
/// `.md` suffix stripped. Errors when nothing usable is left.
pub(crate) fn sanitize_name(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let stem = trimmed.strip_suffix(".md").unwrap_or(trimmed);
    let mut out = String::new();
    for c in stem.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if (c == '-' || c == '.' || c == '/' || c == '\\' || c.is_whitespace()) && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        return Err(format!("invalid note name {:?}: use letters, digits, '-' or '_'", raw));
    }
    if out.len() > MAX_NAME_LEN {
        return Err(format!("note name is too long (max {} characters)", MAX_NAME_LEN));
    }
    Ok(out)
}

fn note_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.md", name))
}

/// Split a note file into (description, body). Files without front matter are all body.
pub(crate) fn parse_note(text: &str) -> (String, String) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (String::new(), text.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (String::new(), text.to_string());
    };
    let header = &rest[..end];
    let body = rest[end + 4..].strip_prefix('\n').unwrap_or(&rest[end + 4..]);
    let description = header
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|d| d.trim().to_string())
        .unwrap_or_default();
    (description, body.trim_start_matches('\n').to_string())
}

pub(crate) fn render_note(description: &str, body: &str) -> String {
    format!("---\ndescription: {}\n---\n\n{}\n", clean_description(description), body.trim())
}

fn clean_description(raw: &str) -> String {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(MAX_DESCRIPTION_LEN).collect()
}

fn modified_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// All notes, most recently updated first. A missing folder is just "no notes".
pub(crate) fn list_notes(dir: &Path) -> Vec<NoteMeta> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut notes: Vec<NoteMeta> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            // Skip files a user dropped in by hand that don't round-trip as note names.
            if sanitize_name(&name).ok()? != name {
                return None;
            }
            let text = fs::read_to_string(&path).ok()?;
            let (description, body) = parse_note(&text);
            Some(NoteMeta { name, description, updated_at: modified_secs(&path), chars: body.chars().count() })
        })
        .collect();
    notes.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.name.cmp(&b.name)));
    notes
}

pub(crate) fn read_note(dir: &Path, name: &str) -> Result<Note, String> {
    let name = sanitize_name(name)?;
    let path = note_path(dir, &name);
    let text = fs::read_to_string(&path).map_err(|_| format!("no memory note named '{}'", name))?;
    let (description, body) = parse_note(&text);
    Ok(Note {
        meta: NoteMeta { name, description, updated_at: modified_secs(&path), chars: body.chars().count() },
        body,
    })
}

/// Create or overwrite a note. Returns the sanitized name actually written.
pub(crate) fn write_note(dir: &Path, name: &str, description: &str, body: &str) -> Result<String, String> {
    let name = sanitize_name(name)?;
    if body.chars().count() > MAX_NOTE_CHARS {
        return Err(format!(
            "note is too long ({} chars, max {}); split it into smaller notes",
            body.chars().count(),
            MAX_NOTE_CHARS
        ));
    }
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    fs::write(note_path(dir, &name), render_note(description, body)).map_err(|e| e.to_string())?;
    Ok(name)
}

/// Replace exactly one occurrence of `old` in the note body (like `edit_file`).
pub(crate) fn edit_note(dir: &Path, name: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_text must not be empty".to_string());
    }
    let note = read_note(dir, name)?;
    match note.body.matches(old).count() {
        0 => Err(format!("old_text not found in '{}'", note.meta.name)),
        1 => write_note(dir, &note.meta.name, &note.meta.description, &note.body.replacen(old, new, 1)),
        n => Err(format!("old_text matches {} places in '{}'; include more context", n, note.meta.name)),
    }
}

pub(crate) fn delete_note(dir: &Path, name: &str) -> Result<String, String> {
    let name = sanitize_name(name)?;
    fs::remove_file(note_path(dir, &name)).map_err(|_| format!("no memory note named '{}'", name))?;
    Ok(name)
}

/// Case-insensitive substring search over note names, descriptions and body lines.
/// Returns `(name, line)` hits; a name/description match reports the description.
pub(crate) fn search_notes(dir: &Path, query: &str) -> Vec<(String, String)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for meta in list_notes(dir) {
        if meta.name.contains(&q) || meta.description.to_lowercase().contains(&q) {
            hits.push((meta.name.clone(), meta.description.clone()));
        }
        if let Ok(note) = read_note(dir, &meta.name) {
            for line in note.body.lines().filter(|l| l.to_lowercase().contains(&q)) {
                hits.push((meta.name.clone(), line.trim().to_string()));
                if hits.len() >= MAX_SEARCH_HITS {
                    return hits;
                }
            }
        }
        if hits.len() >= MAX_SEARCH_HITS {
            break;
        }
    }
    hits
}

/// Index lines (`- name — description`) for every note except the pinned one,
/// stopping at `budget` characters with a pointer to `memory search` for the rest.
pub(crate) fn format_index(notes: &[NoteMeta], budget: usize) -> String {
    let others: Vec<&NoteMeta> = notes.iter().filter(|n| n.name != PINNED_NOTE).collect();
    let mut out = String::new();
    for (i, n) in others.iter().enumerate() {
        let line = if n.description.is_empty() {
            format!("- {}\n", n.name)
        } else {
            format!("- {} — {}\n", n.name, n.description)
        };
        if out.len() + line.len() > budget {
            out.push_str(&format!(
                "- …and {} more (older) — use `memory` search/list to find them\n",
                others.len() - i
            ));
            break;
        }
        out.push_str(&line);
    }
    out
}

const MEMORY_GUIDANCE: &str = "You have a long-term memory: markdown notes that persist across chats, \
managed with the `memory` tool (list, read, write, edit, delete, search). The index below shows each \
note's name and description — read a note before relying on details that aren't shown here.\n\
Save a memory when you learn something durable that will help in future chats: who the user is, \
their preferences and how they like to work, people, projects and places that matter to them, \
decisions made, and corrections to how you behave. Do not save small talk, one-off task details, \
anything already in a project's AGENTS.md, or secrets (passwords, keys, tokens).\n\
Prefer updating an existing note over creating a near-duplicate. Keep notes short and factual, and \
give each one a specific one-line description. Facts about the user themselves belong in `about-me`. \
Delete or fix notes that turn out to be wrong. Don't announce routine saves.";

/// The `## About the user` + `## Memory` prompt sections. With the tool disabled the
/// model still sees the profile and pinned note, but not the index or guidance.
pub(crate) fn prompt_section(state: &AppState, tool_enabled: bool) -> String {
    let mut out = String::new();

    let setting = |k: &str| get_app_setting_value(state, k).ok().flatten().filter(|v| !v.trim().is_empty());
    let profile: Vec<String> = [("Name", "user/name"), ("About", "user/about"), ("Location", "user/location")]
        .iter()
        .filter_map(|(label, key)| setting(key).map(|v| format!("{}: {}", label, v.trim())))
        .collect();
    if !profile.is_empty() {
        out.push_str("\n\n## About the user\n");
        out.push_str(&profile.join("\n"));
    }

    let dir = &state.memory_dir;
    let notes = list_notes(dir);
    if tool_enabled {
        out.push_str("\n\n## Memory\n");
        out.push_str(MEMORY_GUIDANCE);
    }
    if let Ok(pinned) = read_note(dir, PINNED_NOTE) {
        if !pinned.body.trim().is_empty() {
            let body: String = pinned.body.chars().take(PINNED_BUDGET).collect();
            out.push_str(&format!("\n\n### {} (pinned)\n{}", PINNED_NOTE, body.trim()));
            if pinned.meta.chars > PINNED_BUDGET {
                out.push_str("\n…(truncated — read the note for the rest)");
            }
        }
    }
    if tool_enabled {
        let index = format_index(&notes, INDEX_BUDGET);
        out.push_str("\n\n### Notes\n");
        if index.is_empty() {
            out.push_str("No memories saved yet.");
        } else {
            out.push_str(index.trim_end());
        }
    }
    out
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn required<'a>(args: &'a Value, key: &str, action: &str) -> Result<&'a str, String> {
    arg(args, key).ok_or_else(|| format!("`{}` is required for action '{}'", key, action))
}

/// Dispatch one `memory` tool call. Returns `(result_text, changed)`, where
/// `changed` tells the caller to notify the UI.
pub(crate) fn run_tool(dir: &Path, args: &Value) -> Result<(String, bool), String> {
    let action = arg(args, "action").ok_or_else(|| "missing `action`".to_string())?;
    match action {
        "list" => {
            let notes = list_notes(dir);
            if notes.is_empty() {
                return Ok(("No memory notes yet.".to_string(), false));
            }
            let lines: Vec<String> = notes
                .iter()
                .map(|n| format!("- {} — {} ({} chars)", n.name, n.description, n.chars))
                .collect();
            Ok((lines.join("\n"), false))
        }
        "read" => {
            let note = read_note(dir, required(args, "name", action)?)?;
            Ok((format!("# {}\n{}\n\n{}", note.meta.name, note.meta.description, note.body), false))
        }
        "write" => {
            let name = required(args, "name", action)?;
            let content = required(args, "content", action)?;
            let existing = read_note(dir, name).ok();
            // Omitting the description on an update keeps the old one.
            let description = arg(args, "description")
                .map(str::to_string)
                .or_else(|| existing.as_ref().map(|n| n.meta.description.clone()))
                .unwrap_or_default();
            let saved = write_note(dir, name, &description, content)?;
            let verb = if existing.is_some() { "Updated" } else { "Saved" };
            Ok((format!("{} memory note '{}'.", verb, saved), true))
        }
        "edit" => {
            let saved = edit_note(
                dir,
                required(args, "name", action)?,
                required(args, "old_text", action)?,
                arg(args, "new_text").unwrap_or(""),
            )?;
            Ok((format!("Edited memory note '{}'.", saved), true))
        }
        "delete" => {
            let gone = delete_note(dir, required(args, "name", action)?)?;
            Ok((format!("Deleted memory note '{}'.", gone), true))
        }
        "search" => {
            let query = required(args, "query", action)?;
            let hits = search_notes(dir, query);
            if hits.is_empty() {
                return Ok((format!("No memory notes match '{}'.", query), false));
            }
            let lines: Vec<String> = hits.iter().map(|(n, l)| format!("{}: {}", n, l)).collect();
            Ok((lines.join("\n"), false))
        }
        other => Err(format!(
            "unknown memory action '{}' (use list, read, write, edit, delete or search)",
            other
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muninn-memory-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_name_normalizes_and_blocks_traversal() {
        assert_eq!(sanitize_name("About Me").unwrap(), "about-me");
        assert_eq!(sanitize_name("work-history.md").unwrap(), "work-history");
        assert_eq!(sanitize_name("../../etc/passwd").unwrap(), "etc-passwd");
        assert_eq!(sanitize_name("/abs/path").unwrap(), "abs-path");
        assert_eq!(sanitize_name("  Project_X  ").unwrap(), "project_x");
        assert!(sanitize_name("../..").is_err());
        assert!(sanitize_name("").is_err());
        assert!(sanitize_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn parse_and_render_round_trip() {
        let text = render_note("Prefers  dark\nmode", "Uses dark mode everywhere.");
        assert!(text.starts_with("---\ndescription: Prefers dark mode\n---\n"));
        let (desc, body) = parse_note(&text);
        assert_eq!(desc, "Prefers dark mode");
        assert_eq!(body.trim(), "Uses dark mode everywhere.");
    }

    #[test]
    fn parse_note_without_front_matter_is_all_body() {
        let (desc, body) = parse_note("just some text\n---\nmore");
        assert_eq!(desc, "");
        assert_eq!(body, "just some text\n---\nmore");
    }

    #[test]
    fn write_read_edit_delete_cycle() {
        let dir = temp_dir("cycle");
        assert_eq!(write_note(&dir, "Coffee Order", "How they take coffee", "Oat flat white.").unwrap(), "coffee-order");
        let note = read_note(&dir, "coffee-order").unwrap();
        assert_eq!(note.meta.description, "How they take coffee");
        assert_eq!(note.body.trim(), "Oat flat white.");

        edit_note(&dir, "coffee-order", "Oat", "Almond").unwrap();
        assert_eq!(read_note(&dir, "coffee-order").unwrap().body.trim(), "Almond flat white.");
        assert!(edit_note(&dir, "coffee-order", "missing", "x").is_err());

        write_note(&dir, "dup", "", "a a").unwrap();
        assert!(edit_note(&dir, "dup", "a", "b").unwrap_err().contains("2 places"));

        delete_note(&dir, "coffee-order").unwrap();
        assert!(read_note(&dir, "coffee-order").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_rejects_oversized_notes() {
        let dir = temp_dir("big");
        let err = write_note(&dir, "huge", "", &"x".repeat(MAX_NOTE_CHARS + 1)).unwrap_err();
        assert!(err.contains("too long"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_skips_non_note_files_and_search_finds_lines() {
        let dir = temp_dir("list");
        write_note(&dir, "projects", "Active projects", "Muninn: Tauri app\nKrinon: CRM").unwrap();
        fs::write(dir.join("Not A Note.md"), "ignored").unwrap();
        fs::write(dir.join("readme.txt"), "ignored").unwrap();
        let names: Vec<String> = list_notes(&dir).into_iter().map(|n| n.name).collect();
        assert_eq!(names, vec!["projects".to_string()]);

        let hits = search_notes(&dir, "krinon");
        assert_eq!(hits, vec![("projects".to_string(), "Krinon: CRM".to_string())]);
        assert!(search_notes(&dir, "  ").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_skips_pinned_and_respects_budget() {
        let notes: Vec<NoteMeta> = (0..50)
            .map(|i| NoteMeta {
                name: if i == 0 { PINNED_NOTE.to_string() } else { format!("note-{}", i) },
                description: "a fairly ordinary one-line description".to_string(),
                updated_at: 0,
                chars: 10,
            })
            .collect();
        let index = format_index(&notes, 400);
        assert!(!index.contains(PINNED_NOTE));
        assert!(index.len() < 520, "index overflowed budget: {}", index.len());
        assert!(index.contains("more (older)"));
        assert_eq!(format_index(&notes[..1], 400), "");
    }

    #[test]
    fn run_tool_reports_changes_and_errors() {
        let dir = temp_dir("tool");
        let (msg, changed) = run_tool(&dir, &json!({"action": "write", "name": "about-me", "description": "The user", "content": "Lives in CA."})).unwrap();
        assert!(changed && msg.starts_with("Saved"));
        let (msg, _) = run_tool(&dir, &json!({"action": "write", "name": "about-me", "content": "Lives in CA."})).unwrap();
        assert!(msg.starts_with("Updated"));
        assert_eq!(read_note(&dir, "about-me").unwrap().meta.description, "The user", "update without description keeps it");
        let (msg, changed) = run_tool(&dir, &json!({"action": "read", "name": "about-me"})).unwrap();
        assert!(!changed && msg.contains("Lives in CA."));
        assert!(run_tool(&dir, &json!({"action": "write", "name": "x"})).unwrap_err().contains("content"));
        assert!(run_tool(&dir, &json!({"action": "nope"})).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
