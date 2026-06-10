//! Apple Mail (Mail.app) read-only access via AppleScript / `osascript`.
//!
//! Three entry points — `list_inbox`, `search_mail`, `read_mail` — are called
//! from `tools::execute_agent_tool`. All output is returned as JSON strings so
//! the LLM gets structured data. Message IDs are the RFC822 `Message-ID`
//! header (globally stable), not Mail's internal scriptable id.
//!
//! Dates are emitted as locale-independent ISO-8601 local time:
//! `YYYY-MM-DDTHH:MM:SS` (no timezone suffix — these are in the user's local
//! timezone, matching what they see in Mail.app). `list_inbox` and
//! `search_mail` accept optional `since` / `until` ISO date bounds which are
//! pushed down into the AppleScript `whose` clause for server-side filtering.
//!
//! Permission: on first invocation, macOS prompts the user to allow Muninn to
//! control Mail (Automation consent). If denied, subsequent calls fail fast
//! with error code -1743 — we map that to a human-readable message telling
//! the user where to grant access.
//!
//! This module intentionally does nothing that writes to Mail: no compose, no
//! send, no delete, no flag changes. Adding those later should be guarded by
//! the dangerous-command approval flow.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use serde_json::{json, Value};
use std::process::Command;

/// Opaque per-message handle encoding `(account, mailbox, int_id)`.
///
/// Mail's integer id is O(1) within a specific mailbox but session-local and
/// scoped — the RFC822 Message-ID is O(n) global. We used to return Message-IDs,
/// which forced `read_mail` to linear-scan every mailbox of every account and
/// crushed Mail.app on large archives. Now list/search return a base64 handle
/// encoding the direct-access triple; `read_mail` decodes it and navigates
/// straight to the message.
///
/// If Mail.app restarts between list and read the int_id can shuffle and the
/// handle becomes stale — `read_mail` returns a clear error in that case
/// telling the caller to re-list. Handles are opaque from the model's POV.
fn encode_handle(account: &str, mailbox: &str, int_id: &str) -> String {
    // \x1f is ASCII unit separator — won't appear in account/mailbox names.
    let raw = format!("{account}\x1f{mailbox}\x1f{int_id}");
    B64.encode(raw.as_bytes())
}

fn decode_handle(handle: &str) -> Result<(String, String, i64), String> {
    let bytes = B64
        .decode(handle.as_bytes())
        .map_err(|_| "message id is not a valid handle (did you pass a raw Message-ID? call list_inbox or search_mail first to get a fresh handle)".to_string())?;
    let s = String::from_utf8(bytes).map_err(|_| "malformed handle".to_string())?;
    let parts: Vec<&str> = s.splitn(3, '\x1f').collect();
    if parts.len() != 3 {
        return Err("malformed handle".to_string());
    }
    let int_id: i64 = parts[2]
        .parse()
        .map_err(|_| "malformed handle (int id)".to_string())?;
    Ok((parts[0].to_string(), parts[1].to_string(), int_id))
}

/// The `pad2` handler + ISO-formatting snippet that every script below reuses.
/// Placed at the top of the script so `my pad2(...)` works from inside
/// `tell application "Mail"` blocks.
const APPLESCRIPT_HELPERS: &str = r#"on pad2(n)
    set s to (n as integer) as string
    if length of s < 2 then return "0" & s
    return s
end pad2

on isoDate(d)
    return (year of d as string) & "-" & my pad2(month of d as integer) & "-" & my pad2(day of d) & "T" & my pad2(hours of d) & ":" & my pad2(minutes of d) & ":" & my pad2(seconds of d)
end isoDate
"#;

/// Run an AppleScript snippet via `osascript -e` and return stdout on success.
/// Maps the common Automation-denied error to a user-actionable message.
fn run_applescript(script: &str) -> Result<String, String> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("failed to invoke osascript: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("-1743")
            || stderr.contains("not authorized")
            || stderr.contains("not allowed to send Apple events")
            || stderr.contains("doesn't have permission")
        {
            return Err(
                "Muninn needs Automation permission for Mail. Open \
                 System Settings → Privacy & Security → Automation, find Muninn, \
                 and enable the Mail checkbox, then retry."
                    .to_string(),
            );
        }
        return Err(format!("AppleScript error: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Escape a user string so it can be safely interpolated into an AppleScript
/// double-quoted literal. Strips controls that would corrupt our TSV output.
fn escape_as_string(s: &str) -> String {
    s.replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', " ")
        .replace('\t', " ")
}

/// Parsed ISO date components used to build an AppleScript date literal.
#[derive(Debug, Clone, Copy)]
struct DateParts {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

/// Parse a user-supplied ISO date. Accepts:
/// - `YYYY-MM-DD` (time defaults to 00:00:00)
/// - `YYYY-MM-DDTHH:MM` (seconds default to 0)
/// - `YYYY-MM-DDTHH:MM:SS`
/// `T` may also be a space. Timezone suffixes are ignored — dates are
/// interpreted in the user's local timezone.
fn parse_iso(s: &str) -> Result<DateParts, String> {
    let s = s.trim();
    let (date_part, time_part) = match s
        .find(|c: char| c == 'T' || c == 't' || c == ' ')
    {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let mut dp = date_part.splitn(3, '-');
    let year: i32 = dp
        .next()
        .ok_or("missing year")?
        .parse()
        .map_err(|_| "invalid year".to_string())?;
    let month: u32 = dp
        .next()
        .ok_or("missing month")?
        .parse()
        .map_err(|_| "invalid month".to_string())?;
    let day: u32 = dp
        .next()
        .ok_or("missing day")?
        .trim_matches(|c: char| c.is_whitespace())
        .parse()
        .map_err(|_| "invalid day".to_string())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(format!("out-of-range date: {}-{:02}-{:02}", year, month, day));
    }

    let (hour, minute, second) = if time_part.is_empty() {
        (0, 0, 0)
    } else {
        // Strip trailing timezone designators we don't use.
        let tp = time_part
            .trim_end_matches(|c: char| c == 'Z' || c == 'z')
            .trim();
        let tp = match tp.find(|c: char| c == '+' || c == '-') {
            Some(i) => &tp[..i],
            None => tp,
        };
        let mut parts = tp.splitn(3, ':');
        let h: u32 = parts
            .next()
            .unwrap_or("0")
            .parse()
            .map_err(|_| "invalid hour".to_string())?;
        let m: u32 = parts
            .next()
            .unwrap_or("0")
            .parse()
            .map_err(|_| "invalid minute".to_string())?;
        let sec: u32 = parts
            .next()
            .unwrap_or("0")
            .split('.')
            .next()
            .unwrap_or("0")
            .parse()
            .map_err(|_| "invalid second".to_string())?;
        (h, m, sec)
    };
    Ok(DateParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
    })
}

/// Emit AppleScript that binds `varName` to a date built from `parts`.
fn applescript_date_literal(var_name: &str, parts: DateParts) -> String {
    format!(
        r#"set {var} to current date
set year of {var} to {y}
set month of {var} to {m}
set day of {var} to {d}
set hours of {var} to {h}
set minutes of {var} to {mi}
set seconds of {var} to {s}
"#,
        var = var_name,
        y = parts.year,
        m = parts.month,
        d = parts.day,
        h = parts.hour,
        mi = parts.minute,
        s = parts.second
    )
}

/// Parse the AppleScript tab-separated output into a JSON blob the LLM can consume.
/// Format per line: `account \t mailbox \t int_id \t iso_date \t from \t subject \t status`
/// The first three columns become an opaque base64 handle in the output `id` field.
fn parse_messages_tsv(raw: &str, limit: u32) -> String {
    let messages: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.splitn(7, '\t').collect();
            if fields.len() != 7 {
                return None;
            }
            let account = fields[0].trim();
            let mailbox = fields[1].trim();
            let int_id = fields[2].trim();
            if account.is_empty() || mailbox.is_empty() || int_id.is_empty() {
                return None;
            }
            Some(json!({
                "id": encode_handle(account, mailbox, int_id),
                "date": fields[3].trim(),
                "from": fields[4].trim(),
                "subject": fields[5].trim(),
                "status": fields[6].trim(),
            }))
        })
        .take(limit as usize)
        .collect();

    json!({
        "count": messages.len(),
        "messages": messages,
    })
    .to_string()
}

/// List the most recent messages in Apple Mail's unified inbox.
///
/// `limit` is clamped to 1..=50. `unread_only` filters out already-read mail.
/// `since` / `until` are ISO date strings that push down a date-range filter
/// into the AppleScript `whose` clause (half-open `[since, until)` when both
/// are supplied). All time comparisons are local-timezone.
pub(crate) fn list_inbox(
    limit: u32,
    unread_only: bool,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<String, String> {
    let limit = limit.clamp(1, 50);
    let unread_as = if unread_only { "true" } else { "false" };

    // Build optional date bindings and matching predicate fragments.
    let mut date_bindings = String::new();
    let mut date_predicate = String::new();
    if let Some(s) = since {
        let parts = parse_iso(s).map_err(|e| format!("invalid `since`: {}", e))?;
        date_bindings.push_str(&applescript_date_literal("sinceDate", parts));
        date_predicate.push_str(" and date received >= sinceDate");
    }
    if let Some(u) = until {
        let parts = parse_iso(u).map_err(|e| format!("invalid `until`: {}", e))?;
        date_bindings.push_str(&applescript_date_literal("untilDate", parts));
        date_predicate.push_str(" and date received < untilDate");
    }

    // When no date filter is present, iterate all inbox messages newest-first
    // and stop when we hit the limit. When date bounds ARE present, let Mail
    // pre-filter via `whose` — much faster than scanning blindly.
    let body = if date_predicate.is_empty() {
        format!(
            r#"set msgs to messages of inbox
    set msgCount to count of msgs
    set collected to 0
    set scanned to 0
    repeat with i from msgCount to 1 by -1
        if collected is greater than or equal to {limit} then exit repeat
        set scanned to scanned + 1
        if scanned is greater than 500 then exit repeat
        try
            set m to item i of msgs
            set isRead to read status of m
            if ({unread} is false) or (isRead is false) then
                set rs to "read"
                if isRead is false then set rs to "unread"
                set mb to mailbox of m
                set acctName to name of (account of mb)
                set mbName to name of mb
                set intId to (id of m) as string
                set d to my isoDate(date received of m)
                set snd to sender of m
                set subj to subject of m
                set output to output & acctName & tab & mbName & tab & intId & tab & d & tab & snd & tab & subj & tab & rs & linefeed
                set collected to collected + 1
            end if
        end try
    end repeat"#,
            limit = limit,
            unread = unread_as
        )
    } else {
        format!(
            r#"try
        set matches to (messages of inbox whose (read status is false or {unread} is false){predicate})
        set collected to 0
        repeat with m in matches
            if collected is greater than or equal to {limit} then exit repeat
            try
                set rs to "read"
                if read status of m is false then set rs to "unread"
                set mb to mailbox of m
                set acctName to name of (account of mb)
                set mbName to name of mb
                set intId to (id of m) as string
                set d to my isoDate(date received of m)
                set snd to sender of m
                set subj to subject of m
                set output to output & acctName & tab & mbName & tab & intId & tab & d & tab & snd & tab & subj & tab & rs & linefeed
                set collected to collected + 1
            end try
        end repeat
    end try"#,
            limit = limit,
            unread = unread_as,
            predicate = date_predicate
        )
    };

    let script = format!(
        "{helpers}\n{bindings}tell application \"Mail\"\n    set output to \"\"\n    {body}\n    return output\nend tell",
        helpers = APPLESCRIPT_HELPERS,
        bindings = date_bindings,
        body = body
    );

    let raw = run_applescript(&script)?;
    Ok(parse_messages_tsv(&raw, limit))
}

/// Search Apple Mail for messages matching `query` in subject OR sender,
/// optionally bounded by `since` / `until` ISO dates.
pub(crate) fn search_mail(
    query: &str,
    limit: u32,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<String, String> {
    if query.trim().is_empty() && since.is_none() && until.is_none() {
        return Err(
            "search_mail needs at least one of: query, since, until. \
             Use list_inbox for an unfiltered recent list."
                .to_string(),
        );
    }
    let limit = limit.clamp(1, 50);
    let q = escape_as_string(query);

    let mut date_bindings = String::new();
    let mut predicate_parts: Vec<String> = Vec::new();

    if !query.trim().is_empty() {
        predicate_parts.push(format!("(subject contains \"{q}\" or sender contains \"{q}\")"));
    }
    if let Some(s) = since {
        let parts = parse_iso(s).map_err(|e| format!("invalid `since`: {}", e))?;
        date_bindings.push_str(&applescript_date_literal("sinceDate", parts));
        predicate_parts.push("date received >= sinceDate".to_string());
    }
    if let Some(u) = until {
        let parts = parse_iso(u).map_err(|e| format!("invalid `until`: {}", e))?;
        date_bindings.push_str(&applescript_date_literal("untilDate", parts));
        predicate_parts.push("date received < untilDate".to_string());
    }
    let predicate = predicate_parts.join(" and ");

    let script = format!(
        r#"{helpers}
{bindings}tell application "Mail"
    set output to ""
    set collected to 0
    try
        set matches to (messages of inbox whose {predicate})
        repeat with m in matches
            if collected is greater than or equal to {limit} then exit repeat
            try
                set mb to mailbox of m
                set acctName to name of (account of mb)
                set mbName to name of mb
                set intId to (id of m) as string
                set d to my isoDate(date received of m)
                set snd to sender of m
                set subj to subject of m
                set rs to "read"
                if read status of m is false then set rs to "unread"
                set output to output & acctName & tab & mbName & tab & intId & tab & d & tab & snd & tab & subj & tab & rs & linefeed
                set collected to collected + 1
            end try
        end repeat
    end try
    return output
end tell"#,
        helpers = APPLESCRIPT_HELPERS,
        bindings = date_bindings,
        predicate = predicate,
        limit = limit
    );

    let raw = run_applescript(&script)?;
    Ok(parse_messages_tsv(&raw, limit))
}

/// Fetch the full body + headers of a single message by its handle (as
/// returned from `list_inbox` / `search_mail`).
///
/// The handle encodes `(account, mailbox, int_id)` — all three are required
/// to do a direct, O(1) lookup. The previous Message-ID scan approach would
/// iterate every message in every mailbox, which hangs Mail.app on large
/// archives.
pub(crate) fn read_mail(handle: &str) -> Result<String, String> {
    if handle.trim().is_empty() {
        return Err("handle is empty".to_string());
    }
    let (account, mailbox, int_id) = decode_handle(handle.trim())?;
    let account_esc = escape_as_string(&account);
    let mailbox_esc = escape_as_string(&mailbox);

    let script = format!(
        r#"{helpers}
tell application "Mail"
    set found to missing value
    try
        set targetAcct to account "{acct}"
        set targetMb to mailbox "{mbox}" of targetAcct
        set found to (first message of targetMb whose id is {iid})
    end try
    if found is missing value then return "__MUNINN_NOT_FOUND__"
    set m to found
    set subj to subject of m
    set snd to sender of m
    set d to my isoDate(date received of m)
    set recipientsStr to ""
    try
        set recipientsStr to ((to recipients of m) as string)
    end try
    set body to ""
    try
        set body to content of m
    end try
    return "From: " & snd & linefeed & ¬
           "To: " & recipientsStr & linefeed & ¬
           "Subject: " & subj & linefeed & ¬
           "Date: " & d & linefeed & linefeed & body
end tell"#,
        helpers = APPLESCRIPT_HELPERS,
        acct = account_esc,
        mbox = mailbox_esc,
        iid = int_id
    );

    let raw = run_applescript(&script)?;
    if raw.trim() == "__MUNINN_NOT_FOUND__" {
        return Err(format!(
            "Message not found at {account}/{mailbox}#{int_id}. Mail.app may have restarted \
             since the handle was issued, shuffling its internal ids. Call list_inbox or \
             search_mail again to get a fresh handle."
        ));
    }

    // Char-safe truncation (not byte-based) so UTF-8 sequences aren't split.
    const MAX_CHARS: usize = 20_000;
    let total = raw.chars().count();
    if total <= MAX_CHARS {
        return Ok(raw);
    }
    let truncated: String = raw.chars().take(MAX_CHARS).collect();
    Ok(format!(
        "{truncated}\n\n... [body truncated — {total} chars total, showing first {MAX_CHARS}]"
    ))
}
