//! Apple Calendar (Calendar.app) write access via AppleScript / `osascript`.
//!
//! One entry point — `create_event` — called from `tools::execute_agent_tool`.
//! Reuses the shared AppleScript helpers (date literals, escaping, runner) from
//! `mail.rs`. Creating an event is a reversible, visible write, so unlike
//! `send_mail` it is not gated behind an approval prompt.
//!
//! Permission: on first invocation macOS prompts the user to allow Muninn to
//! control Calendar (Automation consent); `run_applescript` maps a denial to a
//! user-actionable message.

use serde_json::json;

use crate::mail::{applescript_date_literal, escape_as_string, parse_iso, run_applescript};

/// A single-line AppleScript string literal (quoted, controls stripped).
fn quoted(s: &str) -> String {
    format!("\"{}\"", escape_as_string(s))
}

/// Create an event in Calendar.app.
///
/// `start` / `end` are ISO date-times (YYYY-MM-DD or YYYY-MM-DDTHH:MM[:SS], local
/// time). If `calendar` is omitted, the first calendar is used. `location` and
/// `notes` are optional. Returns a small JSON blob on success.
pub(crate) fn create_event(
    title: &str,
    start: &str,
    end: &str,
    calendar: Option<&str>,
    location: Option<&str>,
    notes: Option<&str>,
) -> Result<String, String> {
    if title.trim().is_empty() {
        return Err("create_event needs a non-empty title".to_string());
    }
    let start_parts = parse_iso(start).map_err(|e| format!("invalid `start`: {}", e))?;
    let end_parts = parse_iso(end).map_err(|e| format!("invalid `end`: {}", e))?;

    let mut bindings = String::new();
    bindings.push_str(&applescript_date_literal("startDate", start_parts));
    bindings.push_str(&applescript_date_literal("endDate", end_parts));

    // Resolve the target calendar: named if supplied (error if missing), else the first.
    let cal_select = match calendar {
        Some(name) if !name.trim().is_empty() => format!(
            "    set targetCal to missing value\n    \
             try\n        set targetCal to calendar {}\n    end try\n    \
             if targetCal is missing value then error \"no calendar named {}\"\n",
            quoted(name.trim()),
            escape_as_string(name.trim()),
        ),
        _ => "    set targetCal to first calendar\n".to_string(),
    };

    // Build optional properties.
    let mut props = format!(
        "summary:{}, start date:startDate, end date:endDate",
        quoted(title)
    );
    if let Some(loc) = location.filter(|s| !s.trim().is_empty()) {
        props.push_str(&format!(", location:{}", quoted(loc)));
    }
    if let Some(desc) = notes.filter(|s| !s.trim().is_empty()) {
        props.push_str(&format!(", description:{}", quoted(desc)));
    }

    let script = format!(
        r#"{bindings}tell application "Calendar"
{cal_select}    tell targetCal
        make new event with properties {{{props}}}
    end tell
end tell
return "ok""#,
        bindings = bindings,
        cal_select = cal_select,
        props = props,
    );

    run_applescript(&script)?;
    Ok(json!({
        "ok": true,
        "action": "event_created",
        "title": title,
        "start": start,
        "end": end,
        "calendar": calendar.unwrap_or("(first calendar)"),
    })
    .to_string())
}
