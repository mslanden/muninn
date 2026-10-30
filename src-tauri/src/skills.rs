//! Skills: reusable instruction packs the model loads on demand
//! (`AppState.skills_dir/<name>/SKILL.md`, the standard Agent Skills layout, so
//! skills written for other tools can be dropped in).
//!
//! `SKILL.md` carries front matter (`name`, `description`, optional
//! `allowed-tools`) and markdown instructions; the folder may hold extra
//! reference files. Every prompt lists skill names + descriptions (capped); the
//! model loads a skill's body with the `skill` tool when a request matches it.
//! The user can force one for a turn with `/name`, and tasks/subagents can be
//! pinned to one — then `allowed-tools` (if set) restricts the toolset.
//!
//! Creating, editing or deleting a skill changes future behaviour, so those
//! tool actions need an `ask_user` approval in interactive chat and are refused
//! in background runs (a prompt-injected page must not be able to plant one).
//! Skill names reuse the memory note sanitizer (`[a-z0-9_-]`), and reference
//! files are resolved under the skill's own folder only.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::memory::sanitize_name;
use crate::AppState;

const SKILL_FILE: &str = "SKILL.md";
/// Character budget for the skill list injected into every prompt.
const INDEX_BUDGET: usize = 3_000;
const MAX_SKILL_CHARS: usize = 40_000;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_LISTED_FILES: usize = 50;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// Empty = no restriction (all enabled tools).
    pub allowed_tools: Vec<String>,
    /// SKILL.md last modified, unix seconds.
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    #[serde(flatten)]
    pub meta: SkillMeta,
    pub body: String,
    /// Other files in the skill folder, relative paths.
    pub files: Vec<String>,
    pub path: String,
}

/// Parsed front matter: (description, allowed_tools, body). Tolerates the common
/// YAML shapes skills use: `key: value`, folded/literal blocks (`>` / `|` followed by
/// indented lines), and `allowed-tools` as a comma/space list or `[a, b]`.
pub(crate) fn parse_skill(text: &str) -> (String, Vec<String>, String) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (String::new(), Vec::new(), text.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (String::new(), Vec::new(), text.to_string());
    };
    let header = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n').to_string();

    let mut description = String::new();
    let mut tools_raw = String::new();
    let mut lines = header.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        let mut value = value.trim().to_string();
        if value == ">" || value == "|" || value == ">-" || value == "|-" {
            let mut parts = Vec::new();
            while let Some(next) = lines.peek() {
                if next.starts_with(' ') || next.starts_with('\t') || next.trim().is_empty() {
                    parts.push(next.trim().to_string());
                    lines.next();
                } else {
                    break;
                }
            }
            value = parts.join(" ").trim().to_string();
        }
        let value = value.trim_matches('"').trim_matches('\'').to_string();
        match key {
            "description" => description = value,
            "allowed-tools" | "allowed_tools" => tools_raw = value,
            _ => {}
        }
    }
    let allowed_tools = tools_raw
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|t| t.trim().trim_matches('"').trim_matches('\''))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    (description, allowed_tools, body)
}

pub(crate) fn render_skill(name: &str, description: &str, allowed_tools: &[String], body: &str) -> String {
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = format!("---\nname: {}\ndescription: {}\n", name, description);
    if !allowed_tools.is_empty() {
        out.push_str(&format!("allowed-tools: {}\n", allowed_tools.join(", ")));
    }
    out.push_str(&format!("---\n\n{}\n", body.trim()));
    out
}

fn skill_dir(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

fn modified_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// All skills, alphabetical. A missing folder is just "no skills".
pub(crate) fn list_skills(dir: &Path) -> Vec<SkillMeta> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut skills: Vec<SkillMeta> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_string();
            if sanitize_name(&name).ok()? != name {
                return None;
            }
            let file = path.join(SKILL_FILE);
            let text = fs::read_to_string(&file).ok()?;
            let (description, allowed_tools, _) = parse_skill(&text);
            Some(SkillMeta { name, description, allowed_tools, updated_at: modified_secs(&file) })
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn list_files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if out.len() >= MAX_LISTED_FILES {
                return;
            }
            let path = e.path();
            let Ok(rel) = path.strip_prefix(root) else { continue };
            let rel = rel.to_string_lossy().to_string();
            if rel.starts_with('.') || rel == SKILL_FILE {
                continue;
            }
            if path.is_dir() {
                if depth < 2 {
                    walk(root, &path, depth + 1, out);
                }
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, 0, &mut out);
    out
}

pub(crate) fn load_skill(dir: &Path, name: &str) -> Result<Skill, String> {
    let name = sanitize_name(name)?;
    let root = skill_dir(dir, &name);
    let file = root.join(SKILL_FILE);
    let text = fs::read_to_string(&file).map_err(|_| format!("no skill named '{}'", name))?;
    let (description, allowed_tools, body) = parse_skill(&text);
    Ok(Skill {
        meta: SkillMeta { name, description, allowed_tools, updated_at: modified_secs(&file) },
        body,
        files: list_files(&root),
        path: root.display().to_string(),
    })
}

/// Read a reference file inside a skill folder. Absolute paths and `..` are refused,
/// and the canonical result must still sit under the skill's own folder (no symlink
/// escapes).
pub(crate) fn read_skill_file(dir: &Path, name: &str, rel: &str) -> Result<String, String> {
    let name = sanitize_name(name)?;
    let rel_path = Path::new(rel.trim());
    if rel.trim().is_empty()
        || rel_path.is_absolute()
        || rel_path.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!("invalid file path '{}': use a path relative to the skill folder", rel));
    }
    let root = skill_dir(dir, &name)
        .canonicalize()
        .map_err(|_| format!("no skill named '{}'", name))?;
    let full = root
        .join(rel_path)
        .canonicalize()
        .map_err(|_| format!("no file '{}' in skill '{}'", rel, name))?;
    if !full.starts_with(&root) {
        return Err(format!("'{}' is outside the skill folder", rel));
    }
    let size = fs::metadata(&full).map_err(|e| e.to_string())?.len();
    if size > MAX_FILE_BYTES {
        return Err(format!("'{}' is too large to read ({} bytes, max {})", rel, size, MAX_FILE_BYTES));
    }
    fs::read_to_string(&full).map_err(|_| format!("'{}' is not a text file", rel))
}

/// Create or replace a skill's SKILL.md (other files in the folder are kept).
/// Returns the sanitized name.
pub(crate) fn write_skill(
    dir: &Path,
    name: &str,
    description: &str,
    body: &str,
    allowed_tools: &[String],
) -> Result<String, String> {
    let name = sanitize_name(name)?;
    if description.trim().is_empty() {
        return Err("a skill needs a description — it's how the model decides when to use it".to_string());
    }
    if body.chars().count() > MAX_SKILL_CHARS {
        return Err(format!("skill instructions are too long (max {} chars)", MAX_SKILL_CHARS));
    }
    let root = skill_dir(dir, &name);
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    fs::write(root.join(SKILL_FILE), render_skill(&name, description, allowed_tools, body))
        .map_err(|e| e.to_string())?;
    Ok(name)
}

pub(crate) fn delete_skill(dir: &Path, name: &str) -> Result<String, String> {
    let name = sanitize_name(name)?;
    let root = skill_dir(dir, &name);
    if !root.join(SKILL_FILE).is_file() {
        return Err(format!("no skill named '{}'", name));
    }
    fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
    Ok(name)
}

/// The tool set a run pinned to `skill` may use: everything enabled, narrowed to the
/// skill's `allowed-tools` when it lists any. `memory` and `skill` stay available
/// either way (shared context, not capabilities).
pub(crate) fn restrict_tools(
    enabled: &std::collections::HashSet<String>,
    skill: Option<&Skill>,
) -> std::collections::HashSet<String> {
    match skill.map(|s| &s.meta.allowed_tools).filter(|t| !t.is_empty()) {
        None => enabled.clone(),
        Some(allowed) => enabled
            .iter()
            .filter(|t| allowed.contains(t) || t.as_str() == "memory" || t.as_str() == "skill")
            .cloned()
            .collect(),
    }
}

/// Skill list lines (`- name — description`), stopping at `budget` characters.
pub(crate) fn format_index(skills: &[SkillMeta], budget: usize) -> String {
    let mut out = String::new();
    for (i, s) in skills.iter().enumerate() {
        let line = format!("- {} — {}\n", s.name, s.description);
        if out.len() + line.len() > budget {
            out.push_str(&format!("- …and {} more — use `skill` list to see them\n", skills.len() - i));
            break;
        }
        out.push_str(&line);
    }
    out
}

/// The `## Skills` prompt section (empty when the tool is off or no skills exist).
pub(crate) fn prompt_section(state: &AppState, tool_enabled: bool) -> String {
    if !tool_enabled {
        return String::new();
    }
    let skills = list_skills(&state.skills_dir);
    if skills.is_empty() {
        return String::new();
    }
    format!(
        "\n\n## Skills\nSkills are instruction packs for specific kinds of work. When a request \
matches one, load it with the `skill` tool (action `load`) before starting, and follow it. The \
user can also invoke one directly with /name.\n{}",
        format_index(&skills, INDEX_BUDGET).trim_end()
    )
}

/// Prompt block for a turn/run pinned to a skill (slash command, task, subagent).
pub(crate) fn active_skill_section(skill: &Skill) -> String {
    let mut out = format!(
        "\n\n## Active skill: {}\nThis run uses the `{}` skill — follow its instructions.\n\n{}",
        skill.meta.name, skill.meta.name, skill.body.trim()
    );
    if !skill.files.is_empty() {
        out.push_str(&format!(
            "\n\n(Reference files in this skill, readable with `skill` action `read_file`: {})",
            skill.files.join(", ")
        ));
    }
    out
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn required<'a>(args: &'a Value, key: &str, action: &str) -> Result<&'a str, String> {
    arg(args, key)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("`{}` is required for action '{}'", key, action))
}

fn tools_arg(args: &Value) -> Option<Vec<String>> {
    args.get("allowed_tools").and_then(Value::as_array).map(|arr| {
        arr.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    })
}

/// Ask the user before a skill write; refuse outright when nobody is there to ask.
async fn approve(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    interactive: bool,
    question: String,
) -> Result<(), String> {
    if !interactive {
        return Err(
            "Changing skills requires the user's approval, and no user is available in this \
             background run. Leave skills unchanged."
                .to_string(),
        );
    }
    let answer = crate::interactive::ask_user(
        app,
        state,
        session_id,
        &question,
        vec!["Allow".to_string(), "Don't allow".to_string()],
    )
    .await?;
    if answer.trim().eq_ignore_ascii_case("allow") {
        Ok(())
    } else {
        Err(format!("The user declined (answered: {}). Do not retry; leave skills as they are.", answer))
    }
}

/// Dispatch one `skill` tool call.
pub(crate) async fn run_tool(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    args: &Value,
    interactive: bool,
) -> Result<String, String> {
    let dir = &state.skills_dir;
    let action = arg(args, "action").ok_or_else(|| "missing `action`".to_string())?;
    match action {
        "list" => {
            let skills = list_skills(dir);
            if skills.is_empty() {
                return Ok("No skills yet.".to_string());
            }
            Ok(skills.iter().map(|s| format!("- {} — {}", s.name, s.description)).collect::<Vec<_>>().join("\n"))
        }
        "load" => {
            let skill = load_skill(dir, required(args, "name", action)?)?;
            let mut out = format!("# Skill: {}\n{}\n\n{}", skill.meta.name, skill.meta.description, skill.body);
            if !skill.files.is_empty() {
                out.push_str(&format!("\n\nReference files (action read_file): {}", skill.files.join(", ")));
            }
            Ok(out)
        }
        "read_file" => read_skill_file(dir, required(args, "name", action)?, required(args, "path", action)?),
        "create" | "update" => {
            let name = required(args, "name", action)?;
            let existing = load_skill(dir, name).ok();
            if action == "create" && existing.is_some() {
                return Err(format!("a skill named '{}' already exists — use action update", name));
            }
            if action == "update" && existing.is_none() {
                return Err(format!("no skill named '{}' — use action create", name));
            }
            let description = arg(args, "description")
                .map(str::to_string)
                .or_else(|| existing.as_ref().map(|s| s.meta.description.clone()))
                .unwrap_or_default();
            let body = arg(args, "content")
                .map(str::to_string)
                .or_else(|| existing.as_ref().map(|s| s.body.clone()))
                .ok_or_else(|| "`content` is required for action 'create'".to_string())?;
            let allowed = tools_arg(args)
                .or_else(|| existing.as_ref().map(|s| s.meta.allowed_tools.clone()))
                .unwrap_or_default();
            let preview: String = body.chars().take(600).collect();
            approve(
                app,
                state,
                session_id,
                interactive,
                format!(
                    "Muninn wants to {} the skill \"{}\".\n\nDescription: {}\n\n{}{}\n\nAllow this?",
                    action,
                    name,
                    description,
                    preview,
                    if body.chars().count() > 600 { "…" } else { "" }
                ),
            )
            .await?;
            let saved = write_skill(dir, name, &description, &body, &allowed)?;
            let _ = app.emit("skills-changed", ());
            Ok(format!("{} skill '{}'.", if action == "create" { "Created" } else { "Updated" }, saved))
        }
        "delete" => {
            let name = required(args, "name", action)?;
            load_skill(dir, name)?;
            approve(app, state, session_id, interactive, format!("Muninn wants to delete the skill \"{}\". Allow this?", name))
                .await?;
            let gone = delete_skill(dir, name)?;
            let _ = app.emit("skills-changed", ());
            Ok(format!("Deleted skill '{}'.", gone))
        }
        other => Err(format!(
            "unknown skill action '{}' (use list, load, read_file, create, update or delete)",
            other
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muninn-skills-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_handles_common_front_matter_shapes() {
        let (d, t, b) = parse_skill("---\nname: triage\ndescription: Sort the inbox\nallowed-tools: list_inbox, read_mail\n---\n\nDo it.\n");
        assert_eq!(d, "Sort the inbox");
        assert_eq!(t, vec!["list_inbox", "read_mail"]);
        assert_eq!(b.trim(), "Do it.");

        let (d, t, _) = parse_skill("---\nname: x\ndescription: >\n  Folded across\n  two lines\nallowed-tools: [web_search, \"web_fetch\"]\n---\nbody");
        assert_eq!(d, "Folded across two lines");
        assert_eq!(t, vec!["web_search", "web_fetch"]);

        let (d, t, b) = parse_skill("no front matter here");
        assert!(d.is_empty() && t.is_empty());
        assert_eq!(b, "no front matter here");
    }

    #[test]
    fn render_round_trips() {
        let text = render_skill("review", "Review  code\nchanges", &["read_file".to_string()], "Be thorough.");
        let (d, t, b) = parse_skill(&text);
        assert_eq!(d, "Review code changes");
        assert_eq!(t, vec!["read_file"]);
        assert_eq!(b.trim(), "Be thorough.");
    }

    #[test]
    fn write_list_load_delete_cycle() {
        let dir = temp_dir("cycle");
        assert!(write_skill(&dir, "Email Triage", "", "x", &[]).is_err(), "description required");
        assert_eq!(write_skill(&dir, "Email Triage", "Sort mail", "Steps…", &[]).unwrap(), "email-triage");
        fs::write(dir.join("email-triage").join("template.md"), "Dear …").unwrap();
        fs::create_dir_all(dir.join("Not A Skill")).unwrap();
        fs::create_dir_all(dir.join("empty")).unwrap();

        let names: Vec<String> = list_skills(&dir).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["email-triage".to_string()]);

        let skill = load_skill(&dir, "email-triage").unwrap();
        assert_eq!(skill.files, vec!["template.md".to_string()]);
        assert_eq!(read_skill_file(&dir, "email-triage", "template.md").unwrap(), "Dear …");

        delete_skill(&dir, "email-triage").unwrap();
        assert!(load_skill(&dir, "email-triage").is_err());
        assert!(delete_skill(&dir, "empty").is_err(), "folders without SKILL.md aren't skills");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_skill_file_stays_inside_the_skill() {
        let dir = temp_dir("escape");
        write_skill(&dir, "a", "A", "body", &[]).unwrap();
        write_skill(&dir, "b", "B", "secret", &[]).unwrap();
        assert!(read_skill_file(&dir, "a", "../b/SKILL.md").is_err());
        assert!(read_skill_file(&dir, "a", "/etc/hosts").is_err());
        assert!(read_skill_file(&dir, "a", "").is_err());
        assert!(read_skill_file(&dir, "a", "SKILL.md").unwrap().contains("body"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("b").join("SKILL.md"), dir.join("a").join("link.md")).unwrap();
            assert!(read_skill_file(&dir, "a", "link.md").unwrap_err().contains("outside"));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restrict_tools_honours_allowed_tools_but_keeps_shared_context() {
        let enabled: HashSet<String> = ["read_file", "run_command", "memory", "skill", "web_search"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(restrict_tools(&enabled, None), enabled);
        let dir = temp_dir("restrict");
        write_skill(&dir, "r", "R", "b", &["read_file".to_string(), "not_enabled".to_string()]).unwrap();
        let skill = load_skill(&dir, "r").unwrap();
        let got = restrict_tools(&enabled, Some(&skill));
        let want: HashSet<String> = ["read_file", "memory", "skill"].iter().map(|s| s.to_string()).collect();
        assert_eq!(got, want);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_respects_budget() {
        let skills: Vec<SkillMeta> = (0..40)
            .map(|i| SkillMeta {
                name: format!("skill-{}", i),
                description: "does one specific kind of job well".to_string(),
                allowed_tools: vec![],
                updated_at: 0,
            })
            .collect();
        let index = format_index(&skills, 300);
        assert!(index.len() < 400);
        assert!(index.contains("more"));
    }
}
