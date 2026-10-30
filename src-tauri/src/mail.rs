//! Apple Mail (Mail.app) access via AppleScript / `osascript`.
//!
//! Read entry points — `list_inbox`, `search_mail`, `read_mail` — plus draft-only
//! write entry points — `compose_draft`, `reply_draft` — are called from
//! `tools::execute_agent_tool`. All output is returned as JSON strings so the LLM
//! gets structured data. Message IDs are the RFC822 `Message-ID` header (globally
//! stable), not Mail's internal scriptable id.
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
//! Writes are DRAFT-ONLY by design: `compose_draft` / `reply_draft` save unsent
//! messages to the Drafts mailbox for the user to review and send manually. There
//! is intentionally no send, delete, or flag-change path — nothing leaves the Mac
//! without an explicit human action in Mail.app.

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
pub(crate) const APPLESCRIPT_HELPERS: &str = r#"on pad2(n)
    set s to (n as integer) as string
    if length of s < 2 then return "0" & s
    return s
end pad2

on isoDate(d)
    return (year of d as string) & "-" & my pad2(month of d as integer) & "-" & my pad2(day of d) & "T" & my pad2(hours of d) & ":" & my pad2(minutes of d) & ":" & my pad2(seconds of d)
end isoDate
"#;

/// Wall-clock cap on one osascript run. AppleScript's `with timeout` is per Apple event,
/// so a script making many slow requests (Mail on a big Gmail inbox) could otherwise
/// run for minutes; past this the process is killed and the model gets a clear error.
const OSASCRIPT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Prefix of the error `run_applescript_with_deadline` returns when it kills a run.
const TIMEOUT_MARKER: &str = "__MUNINN_DEADLINE__:";

/// Deadline for opening one message — a healthy read takes well under 8s.
const READ_DEADLINE: std::time::Duration = std::time::Duration::from_secs(25);

/// After a Mail call times out, Mail is busy (killing osascript doesn't cancel the
/// Apple event Mail is still chewing on, so further requests just queue behind it).
/// For this long, mail tools fail instantly instead of each burning a full deadline.
const MAIL_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(90);

static MAIL_BUSY_UNTIL: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Run a Mail script behind the busy circuit breaker. Timeouts trip the breaker and
/// come back as an actionable message for the model.
fn run_mail_script(script: &str, deadline: std::time::Duration) -> Result<String, String> {
    let busy = |remaining: std::time::Duration| {
        format!(
            "Mail.app isn't responding right now (a request just timed out; retry in ~{}s). \
             Stop using mail tools for the moment — tell the user Mail seems busy (often while \
             it syncs a large account) and to try again shortly. Report what you already have.",
            remaining.as_secs().max(1)
        )
    };
    {
        let guard = MAIL_BUSY_UNTIL.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(until) = *guard {
            let now = std::time::Instant::now();
            if now < until {
                return Err(busy(until - now));
            }
        }
    }
    match run_applescript_with_deadline(script, deadline) {
        Err(e) if e.starts_with(TIMEOUT_MARKER) => {
            *MAIL_BUSY_UNTIL.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(std::time::Instant::now() + MAIL_COOLDOWN);
            Err(format!(
                "Mail didn't respond within {}s, so the request was stopped. {}",
                deadline.as_secs(),
                busy(MAIL_COOLDOWN)
            ))
        }
        other => other,
    }
}

/// Run an AppleScript snippet via `osascript -e` and return stdout on success.
/// Maps the common Automation-denied error to a user-actionable message, and kills the
/// run after OSASCRIPT_DEADLINE.
pub(crate) fn run_applescript(script: &str) -> Result<String, String> {
    run_applescript_with_deadline(script, OSASCRIPT_DEADLINE).map_err(|e| {
        if e.starts_with(TIMEOUT_MARKER) {
            format!(
                "The app didn't respond within {}s, so the request was stopped. It may be busy — \
                 try again shortly.",
                OSASCRIPT_DEADLINE.as_secs()
            )
        } else {
            e
        }
    })
}

/// `run_applescript` with an explicit wall-clock deadline.
fn run_applescript_with_deadline(script: &str, deadline: std::time::Duration) -> Result<String, String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to invoke osascript: {}", e))?;
    // Drain both pipes on threads so a large result (a long mail body) can't fill the
    // pipe buffer and stall the child past the deadline.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_reader = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err_reader = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{}{}s", TIMEOUT_MARKER, deadline.as_secs()));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    let output = std::process::Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("-1743")
            || stderr.contains("not authorized")
            || stderr.contains("not allowed to send Apple events")
            || stderr.contains("doesn't have permission")
        {
            return Err(
                "Muninn needs Automation permission for the target app (Mail/Calendar). Open \
                 System Settings → Privacy & Security → Automation, find Muninn, \
                 and enable the relevant checkbox, then retry."
                    .to_string(),
            );
        }
        if stderr.contains("-1712") {
            return Err(
                "Mail took too long to respond (Apple event timed out). Try a narrower request — \
                 a keyword search_mail, or a shorter date range."
                    .to_string(),
            );
        }
        return Err(format!("AppleScript error: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Escape a user string so it can be safely interpolated into an AppleScript
/// double-quoted literal. Strips controls that would corrupt our TSV output.
pub(crate) fn escape_as_string(s: &str) -> String {
    s.replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', " ")
        .replace('\t', " ")
}

/// Parsed ISO date components used to build an AppleScript date literal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DateParts {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Parse a user-supplied ISO date. Accepts:
/// - `YYYY-MM-DD` (time defaults to 00:00:00)
/// - `YYYY-MM-DDTHH:MM` (seconds default to 0)
/// - `YYYY-MM-DDTHH:MM:SS`
/// `T` may also be a space. Timezone suffixes are ignored — dates are
/// interpreted in the user's local timezone.
pub(crate) fn parse_iso(s: &str) -> Result<DateParts, String> {
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
pub(crate) fn applescript_date_literal(var_name: &str, parts: DateParts) -> String {
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

/// Seconds a list/search script may spend waiting on Mail before it stops and returns
/// whatever it has (osascript's own default is 120s, which hid failures as hangs).
const MAIL_TIMEOUT_SECS: u32 = 45;

/// Days covered by `list_inbox` when the caller gives no `since`.
const DEFAULT_LIST_DAYS: i64 = 30;

/// AppleScript that sets `found` to the message a handle points at (or leaves it
/// `missing value`). First a direct by-id reference in the handle's mailbox; that only
/// resolves top-level mailboxes, and search handles for Gmail name the nested
/// `[Gmail]/All Mail` as "All Mail", so fall back to the unified inbox matching id +
/// account (both measured instant on a ~12k-message inbox).
fn find_message_script(account_esc: &str, mailbox_esc: &str, int_id: i64) -> String {
    format!(
        r#"    set found to missing value
    try
        set targetAcct to account "{acct}"
        try
            -- Direct by-id element reference (instant). «class mssg» because Mail's
            -- `message id` property shadows the `message id N` reference form.
            set found to «class mssg» id {iid} of mailbox "{mbox}" of targetAcct
            set probe to date received of found
        on error
            set found to missing value
        end try
        if found is missing value then
            set candidates to (messages of inbox whose id is {iid})
            repeat with c in candidates
                if name of (account of (mailbox of c)) is "{acct}" then
                    set found to contents of c
                    exit repeat
                end if
            end repeat
        end if
    end try"#,
        acct = account_esc,
        mbox = mailbox_esc,
        iid = int_id
    )
}

/// Error for a handle that can't be opened. Deliberately tells the model NOT to
/// re-search for a "fresh" handle: searching returns the same handle, which used to
/// send it into a search → read → search loop.
fn not_found_error(account: &str, mailbox: &str, int_id: i64) -> String {
    format!(
        "Could not open message {account}/{mailbox}#{int_id} in Mail. Do not retry this handle \
         (searching again returns the same one) — work from the date/sender/subject in the \
         search results, skip this message, or tell the user it couldn't be opened."
    )
}

/// Post-filters applied in Rust after the script returns (the scripts over-collect
/// cheaply; exact bounds, unread filtering, ordering and the limit live here).
#[derive(Default)]
struct MessageFilter<'a> {
    since: Option<&'a str>,
    until: Option<&'a str>,
    unread_only: bool,
}

/// Parse the AppleScript tab-separated output into a JSON blob the LLM can consume.
/// Message lines: `account \t mailbox \t int_id \t iso_date \t from \t subject \t status`
/// (the first three columns become an opaque base64 handle in `id`). Lines starting with
/// `#ERR\t<account>\t<msg>` or `#PARTIAL\t<msg>` become `warnings`, so a slow or broken
/// account shows up as such instead of as a silently empty result.
fn parse_messages(raw: &str, limit: u32, filter: &MessageFilter) -> String {
    let mut warnings: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        if let Some(rest) = line.strip_prefix("#ERR\t") {
            let (acct, msg) = rest.split_once('\t').unwrap_or((rest, ""));
            warnings.push(format!("{}: {}", acct.trim(), msg.trim()));
            continue;
        }
        if let Some(rest) = line.strip_prefix("#PARTIAL\t") {
            warnings.push(rest.trim().to_string());
            continue;
        }
        let fields: Vec<&str> = line.splitn(7, '\t').collect();
        if fields.len() != 7 {
            continue;
        }
        let (account, mailbox, int_id) = (fields[0].trim(), fields[1].trim(), fields[2].trim());
        if account.is_empty() || mailbox.is_empty() || int_id.is_empty() {
            continue;
        }
        let date = fields[3].trim();
        // ISO-8601 local timestamps compare correctly as strings, and a bare date
        // bound ("2026-10-14") sorts before any time on that day.
        if filter.since.is_some_and(|s| date < s) || filter.until.is_some_and(|u| date >= u) {
            continue;
        }
        let status = fields[6].trim();
        if filter.unread_only && status != "unread" {
            continue;
        }
        messages.push(json!({
            "id": encode_handle(account, mailbox, int_id),
            "date": date,
            "from": fields[4].trim(),
            "subject": fields[5].trim(),
            "status": status,
        }));
    }
    messages.sort_by(|a, b| b["date"].as_str().cmp(&a["date"].as_str()));
    messages.truncate(limit as usize);
    let mut out = json!({ "count": messages.len(), "messages": messages });
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    out.to_string()
}

/// List recent messages across every account's INBOX, newest first.
///
/// Deliberately NOT `messages of inbox whose date received >= …`: on the unified inbox
/// Mail evaluates a date-only `whose` message by message, which took >90s on a ~12k
/// message inbox and timed out. Instead, per account INBOX (stored newest-first): read
/// the newest `limit` messages in one bulk fetch — or, with `until` / `unread_only`,
/// dates (and read status) in chunks of CHUNK from the newest end,
/// stopping at the window's edge, then fetch subject/sender only for the picked
/// messages — those are the expensive fields on Gmail. Rust re-applies the exact
/// bounds, the unread filter, ordering and the limit.
///
/// `limit` is clamped to 1..=50. `since` defaults to the last DEFAULT_LIST_DAYS days;
/// `since` / `until` are ISO local times (half-open `[since, until)`).
pub(crate) fn list_inbox(
    limit: u32,
    unread_only: bool,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<String, String> {
    let limit = limit.clamp(1, 50);
    let default_since;
    let since = match since {
        Some(s) => s,
        None => {
            default_since = (chrono::Local::now() - chrono::Duration::days(DEFAULT_LIST_DAYS))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            default_since.as_str()
        }
    };
    let mut bindings = applescript_date_literal(
        "sinceDate",
        parse_iso(since).map_err(|e| format!("invalid `since`: {}", e))?,
    );
    let until_check = match until {
        Some(u) => {
            bindings.push_str(&applescript_date_literal(
                "untilDate",
                parse_iso(u).map_err(|e| format!("invalid `until`: {}", e))?,
            ));
            "d < untilDate"
        }
        None => "true",
    };
    // Scan budget per account: chunked date reads from the newest end (dates are cheap;
    // subject/sender are not, so those are fetched only for picked messages).
    const CHUNK: u32 = 200;
    const MAX_SCAN: u32 = 3000;

    let script = format!(
        r##"{helpers}
{bindings}tell application "Mail"
    set output to ""
    with timeout of {timeout} seconds
        repeat with acct in accounts
            set acctName to name of acct
            try
                set ib to mailbox "INBOX" of acct
                set n to count of messages of ib
                if n > 0 and {simple} then
                    -- Common case (no end date, no unread filter): the newest N messages in
                    -- one bulk fetch per field; Rust drops anything older than `since`.
                    -- Mailboxes are stored newest-first, and Rust sorts regardless.
                    set k to {per_account}
                    if n < k then set k to n
                    set rng to a reference to messages 1 thru k of ib
                    set {{ids, dts, snds, subjs, rdd}} to {{id, date received, sender, subject, read status}} of rng
                    repeat with x from 1 to count of ids
                        set rs to "read"
                        if item x of rdd is false then set rs to "unread"
                        set output to output & acctName & tab & "INBOX" & tab & ((item x of ids) as string) & tab & my isoDate(item x of dts) & tab & (item x of snds) & tab & (item x of subjs) & tab & rs & linefeed
                    end repeat
                else if n > 0 then
                    set newestFirst to (date received of message 1 of ib) > (date received of message n of ib)
                    -- Scan from the newest end in chunks, collecting message indices inside
                    -- the window; stop at the first message older than sinceDate.
                    set picked to {{}}
                    set scanned to 0
                    set done to false
                    repeat while (not done) and scanned < n and scanned < {max_scan}
                        set c1 to scanned + 1
                        set c2 to scanned + {chunk}
                        if c2 > n then set c2 to n
                        if newestFirst then
                            set a to c1
                            set b to c2
                        else
                            set a to n - c2 + 1
                            set b to n - c1 + 1
                        end if
                        set ds to date received of messages a thru b of ib
                        set rss to {{}}
                        if {unread_only} then set rss to read status of messages a thru b of ib
                        set k to count of ds
                        repeat with stepNo from 1 to k
                            if newestFirst then
                                set j to stepNo
                            else
                                set j to k - stepNo + 1
                            end if
                            set d to item j of ds
                            if d < sinceDate then
                                set done to true
                                exit repeat
                            end if
                            if {until_check} then
                                set keep to true
                                if {unread_only} then set keep to ((item j of rss) is false)
                                if keep then
                                    set end of picked to (a + j - 1)
                                    if (count of picked) is greater than or equal to {per_account} then
                                        set done to true
                                        exit repeat
                                    end if
                                end if
                            end if
                        end repeat
                        set scanned to c2
                    end repeat
                    if scanned is greater than or equal to {max_scan} and not done then
                        set output to output & "#ERR" & tab & acctName & tab & "only the newest {max_scan} messages were scanned; narrow the date range" & linefeed
                    end if
                    set pc to count of picked
                    if pc > 0 then
                        set lo to item 1 of picked
                        set hi to item pc of picked
                        if lo > hi then set {{lo, hi}} to {{hi, lo}}
                        if (hi - lo + 1) is pc then
                            -- Contiguous slice (the usual case): one bulk fetch per field.
                            set rng to a reference to messages lo thru hi of ib
                            set {{ids, dts, snds, subjs, rdd}} to {{id, date received, sender, subject, read status}} of rng
                            repeat with x from 1 to count of ids
                                set rs to "read"
                                if item x of rdd is false then set rs to "unread"
                                set output to output & acctName & tab & "INBOX" & tab & ((item x of ids) as string) & tab & my isoDate(item x of dts) & tab & (item x of snds) & tab & (item x of subjs) & tab & rs & linefeed
                            end repeat
                        else
                            repeat with idx in picked
                                set m to message (contents of idx) of ib
                                set rs to "read"
                                if read status of m is false then set rs to "unread"
                                set output to output & acctName & tab & "INBOX" & tab & ((id of m) as string) & tab & my isoDate(date received of m) & tab & (sender of m) & tab & (subject of m) & tab & rs & linefeed
                            end repeat
                        end if
                    end if
                end if
            on error errMsg
                set output to output & "#ERR" & tab & acctName & tab & errMsg & linefeed
            end try
        end repeat
    end timeout
    return output
end tell"##,
        helpers = APPLESCRIPT_HELPERS,
        bindings = bindings,
        timeout = MAIL_TIMEOUT_SECS,
        until_check = until_check,
        per_account = limit,
        chunk = CHUNK,
        max_scan = MAX_SCAN,
        unread_only = if unread_only { "true" } else { "false" },
        simple = if until.is_none() && !unread_only { "true" } else { "false" },
    );

    let raw = run_mail_script(&script, OSASCRIPT_DEADLINE)?;
    let filter = MessageFilter { since: Some(since), until, unread_only };
    Ok(parse_messages(&raw, limit, &filter))
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
    // A date-only search is a listing — and a date-only `whose` on the unified inbox is
    // pathologically slow (see list_inbox), so take the per-account path instead.
    if query.trim().is_empty() {
        return list_inbox(limit, false, since, until);
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
        r##"{helpers}
{bindings}tell application "Mail"
    set output to ""
    set collected to 0
    try
        with timeout of {timeout} seconds
        -- A keyword in the predicate lets Mail use its search index (~4s on ~12k messages).
        set matches to (messages of inbox whose {predicate})
        repeat with m in matches
            if collected is greater than or equal to {cap} then exit repeat
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
        end timeout
    on error errMsg
        set output to output & "#PARTIAL" & tab & "Mail stopped responding partway (" & errMsg & ") — results may be incomplete; try a more specific query." & linefeed
    end try
    return output
end tell"##,
        helpers = APPLESCRIPT_HELPERS,
        bindings = date_bindings,
        predicate = predicate,
        timeout = MAIL_TIMEOUT_SECS,
        // Unified-inbox matches aren't date-ordered, so read a little past the limit and
        // let Rust pick the newest. Bounded: each Gmail message read costs an Apple event.
        cap = (limit * 2).clamp(limit, 60)
    );

    let raw = run_mail_script(&script, OSASCRIPT_DEADLINE)?;
    let filter = MessageFilter { since, until, unread_only: false };
    Ok(parse_messages(&raw, limit, &filter))
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
{find}
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
        find = find_message_script(&account_esc, &mailbox_esc, int_id),
    );

    let raw = run_mail_script(&script, READ_DEADLINE)?;
    if raw.trim() == "__MUNINN_NOT_FOUND__" {
        return Err(not_found_error(&account, &mailbox, int_id));
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

// ---- Write actions (draft-only — nothing is ever sent automatically) ----

/// Build an AppleScript string *expression* from possibly-multiline text, preserving
/// line breaks via `& linefeed &`. Returns `""` for empty input. The result is NOT
/// wrapped by the caller — it already includes its own quotes.
fn applescript_multiline(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    let escaped = s.replace('\\', r"\\").replace('"', "\\\"");
    escaped
        .split('\n')
        .map(|line| format!("\"{}\"", line.trim_end_matches('\r')))
        .collect::<Vec<_>>()
        .join(" & linefeed & ")
}

/// A single-line AppleScript string literal (quoted, controls stripped).
fn applescript_quoted(s: &str) -> String {
    format!("\"{}\"", escape_as_string(s))
}

/// Split a recipient field on commas/semicolons into individual addresses.
fn split_recipients(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Emit the AppleScript that adds `to`/`cc` recipients to the outgoing message bound
/// to `newMsg`. Each address becomes a `make new ... recipient` line.
fn recipient_lines(to: &[String], cc: &[String]) -> String {
    let mut out = String::new();
    for addr in to {
        out.push_str(&format!(
            "        make new to recipient at end of to recipients with properties {{address:{}}}\n",
            applescript_quoted(addr)
        ));
    }
    for addr in cc {
        out.push_str(&format!(
            "        make new cc recipient at end of cc recipients with properties {{address:{}}}\n",
            applescript_quoted(addr)
        ));
    }
    out
}

/// Create an unsent draft in Apple Mail. Nothing is sent — the message is saved to the
/// Drafts mailbox for the user to review and send manually.
pub(crate) fn compose_draft(
    to: &str,
    subject: &str,
    body: &str,
    cc: Option<&str>,
) -> Result<String, String> {
    let to_list = split_recipients(to);
    if to_list.is_empty() {
        return Err("compose_draft needs at least one `to` recipient".to_string());
    }
    let cc_list = cc.map(split_recipients).unwrap_or_default();

    let script = format!(
        r#"tell application "Mail"
    set newMsg to make new outgoing message with properties {{subject:{subj}, content:{content}, visible:false}}
    tell newMsg
{recipients}    end tell
    save newMsg
end tell
return "ok""#,
        subj = applescript_quoted(subject),
        content = applescript_multiline(body),
        recipients = recipient_lines(&to_list, &cc_list),
    );

    run_mail_script(&script, OSASCRIPT_DEADLINE)?;
    Ok(json!({
        "ok": true,
        "action": "draft_created",
        "to": to_list,
        "cc": cc_list,
        "subject": subject,
        "note": "Draft saved to Mail's Drafts. Nothing was sent — the user reviews and sends it.",
    })
    .to_string())
}

/// Create a draft reply to an existing message (identified by its `list_inbox` /
/// `search_mail` handle). The draft is addressed to the original sender with a
/// `Re:`-prefixed subject and saved to Drafts — never sent.
pub(crate) fn reply_draft(handle: &str, body: &str) -> Result<String, String> {
    if handle.trim().is_empty() {
        return Err("handle is empty".to_string());
    }
    let (account, mailbox, int_id) = decode_handle(handle.trim())?;
    let account_esc = escape_as_string(&account);
    let mailbox_esc = escape_as_string(&mailbox);

    let script = format!(
        r#"tell application "Mail"
{find}
    if found is missing value then return "__MUNINN_NOT_FOUND__"
    set origSender to sender of found
    set replyAddr to origSender
    try
        set replyAddr to extract address from origSender
    end try
    set origSubject to subject of found
    set replySubject to origSubject
    if replySubject does not start with "Re:" then set replySubject to "Re: " & origSubject
    set newMsg to make new outgoing message with properties {{subject:replySubject, content:{content}, visible:false}}
    tell newMsg
        make new to recipient at end of to recipients with properties {{address:replyAddr}}
    end tell
    save newMsg
end tell
return "ok""#,
        find = find_message_script(&account_esc, &mailbox_esc, int_id),
        content = applescript_multiline(body),
    );

    let raw = run_mail_script(&script, READ_DEADLINE)?;
    if raw.trim() == "__MUNINN_NOT_FOUND__" {
        return Err(not_found_error(&account, &mailbox, int_id));
    }
    Ok(json!({
        "ok": true,
        "action": "reply_draft_created",
        "note": "Reply draft saved to Mail's Drafts, addressed to the original sender. Nothing was sent.",
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_preserves_newlines_and_escapes_quotes() {
        assert_eq!(applescript_multiline(""), "\"\"");
        assert_eq!(applescript_multiline("hi"), "\"hi\"");
        assert_eq!(applescript_multiline("a\nb"), "\"a\" & linefeed & \"b\"");
        // Quotes and backslashes must be escaped so they can't break out of the literal.
        assert_eq!(applescript_multiline("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(applescript_multiline("c:\\x"), "\"c:\\\\x\"");
        // A CR before LF is stripped (CRLF bodies normalize cleanly).
        assert_eq!(applescript_multiline("a\r\nb"), "\"a\" & linefeed & \"b\"");
    }

    #[test]
    fn parsed_messages_are_sorted_newest_first_then_limited() {
        let raw = "Zoho\tINBOX\t1\t2026-03-16T18:03:05\ta@x\told\tread\n\
                   Google\tAll Mail\t2\t2026-10-26T22:02:56\tb@y\tnewest\tunread\n\
                   iCloud\tINBOX\t3\t2026-10-23T12:56:31\tc@z\tmiddle\tread\n\
                   malformed line\n";
        let out: Value = serde_json::from_str(&parse_messages(raw, 2, &MessageFilter::default())).unwrap();
        let subjects: Vec<&str> = out["messages"].as_array().unwrap().iter()
            .map(|m| m["subject"].as_str().unwrap()).collect();
        assert_eq!(subjects, vec!["newest", "middle"]);
        assert_eq!(out["count"], 2);
    }

    #[test]
    fn parse_applies_bounds_unread_filter_and_surfaces_warnings() {
        let raw = "iCloud\tINBOX\t1\t2026-10-13T23:59:59\ta\ttoo old\tunread\n\
                   iCloud\tINBOX\t2\t2026-10-14T08:00:00\tb\tin window\tunread\n\
                   iCloud\tINBOX\t3\t2026-10-15T09:00:00\tc\tread one\tread\n\
                   iCloud\tINBOX\t4\t2026-10-20T00:00:01\td\tpast until\tunread\n\
                   #ERR\tGoogle\tMail got an error: AppleEvent timed out.\n\
                   #PARTIAL\tMail stopped responding partway\n";
        let f = MessageFilter { since: Some("2026-10-14"), until: Some("2026-10-20"), unread_only: true };
        let out: Value = serde_json::from_str(&parse_messages(raw, 10, &f)).unwrap();
        let subjects: Vec<&str> = out["messages"].as_array().unwrap().iter()
            .map(|m| m["subject"].as_str().unwrap()).collect();
        assert_eq!(subjects, vec!["in window"]);
        let warnings = out["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].as_str().unwrap().starts_with("Google: "));
        // No warnings key when everything succeeded.
        let clean: Value = serde_json::from_str(&parse_messages("", 5, &MessageFilter::default())).unwrap();
        assert!(clean.get("warnings").is_none());
    }

    #[test]
    fn find_message_script_falls_back_to_the_unified_inbox() {
        let script = find_message_script("Google", "All Mail", 75471);
        assert!(script.contains(r#"«class mssg» id 75471 of mailbox "All Mail" of targetAcct"#));
        assert!(script.contains("messages of inbox whose id is 75471"));
        assert!(script.contains(r#"is "Google""#));
    }

    #[test]
    fn breaker_fails_fast_while_mail_is_busy() {
        // Simulate a recent timeout: every mail call short-circuits without osascript.
        *MAIL_BUSY_UNTIL.lock().unwrap() = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
        let t = std::time::Instant::now();
        let err = run_mail_script("this would not even compile", READ_DEADLINE).unwrap_err();
        assert!(err.contains("isn't responding right now"));
        assert!(t.elapsed() < std::time::Duration::from_millis(50));
        // Once the cool-down has passed, calls go through again (and fail on the bogus script).
        *MAIL_BUSY_UNTIL.lock().unwrap() = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        let err = run_mail_script("this would not even compile", READ_DEADLINE).unwrap_err();
        assert!(!err.contains("isn't responding"));
        *MAIL_BUSY_UNTIL.lock().unwrap() = None;
    }

    #[test]
    fn not_found_error_discourages_retrying() {
        let e = not_found_error("Google", "All Mail", 1);
        assert!(e.contains("Do not retry"));
        assert!(!e.contains("fresh handle"));
    }

    /// Live timing check against the real Mail.app (needs Automation permission for the
    /// test runner). Prints only counts, warnings and timings — never message contents.
    /// `cargo test --lib mail_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mail_live_timings() {
        let since = (chrono::Local::now() - chrono::Duration::days(14)).format("%Y-%m-%d").to_string();
        let report = |label: &str, f: &dyn Fn() -> Result<String, String>| {
            let t = std::time::Instant::now();
            match f() {
                Ok(out) => {
                    let v: Value = serde_json::from_str(&out).unwrap();
                    println!("{label}: count={} warnings={} in {:.1}s", v["count"], v.get("warnings").map(|w| w.to_string()).unwrap_or_default(), t.elapsed().as_secs_f64());
                }
                Err(e) => println!("{label}: ERROR {e} in {:.1}s", t.elapsed().as_secs_f64()),
            }
        };
        report("list_inbox(20)", &|| list_inbox(20, false, None, None));
        report("list_inbox(50, since 14d)", &|| list_inbox(50, false, Some(&since), None));
        report("list_inbox(20, unread, since 14d)", &|| list_inbox(20, true, Some(&since), None));
        report("search_mail(receipt, since 14d)", &|| search_mail("receipt", 50, Some(&since), None));
        report("search_mail(order, since 14d)", &|| search_mail("order", 50, Some(&since), None));
        report("search_mail(date-only 14d)", &|| search_mail("", 30, Some(&since), None));
        // read_mail on the newest receipt handle, if any — prints only success + length.
        if let Ok(out) = search_mail("receipt", 10, Some(&since), None) {
            let v: Value = serde_json::from_str(&out).unwrap();
            for m in v["messages"].as_array().unwrap().iter().take(3) {
                let id = m["id"].as_str().unwrap();
                let t = std::time::Instant::now();
                let (acct, mbox, _) = decode_handle(id).unwrap();
                match read_mail(id) {
                    Ok(body) => println!("read_mail [{acct}/{mbox}]: ok {} chars in {:.1}s", body.len(), t.elapsed().as_secs_f64()),
                    Err(e) => println!("read_mail [{acct}/{mbox}]: ERROR {e}"),
                }
            }
        }
    }

    #[test]
    fn split_recipients_handles_commas_and_semicolons() {
        assert_eq!(split_recipients("a@x.com"), vec!["a@x.com"]);
        assert_eq!(
            split_recipients(" a@x.com, b@y.com ; c@z.com"),
            vec!["a@x.com", "b@y.com", "c@z.com"]
        );
        assert!(split_recipients("  ,  ; ").is_empty());
    }
}
