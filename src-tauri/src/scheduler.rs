use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike, Weekday};
use rusqlite::{params, OptionalExtension};
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;
use crate::db::{add_feed_item, get_app_setting_value, insert_message, with_db};
use crate::llm::{chat_dynamic, chat_with_minimax_anthropic, chat_with_openai, load_agent_by_id, run_agent};
use crate::models::{DayConfig, HistoryMessage, ScheduleConfig};
use crate::tools::{truncate_chars, WorkspaceScope};

/// Safety cap on how deep a chain of triggered tasks can run before we assume a loop.
const MAX_CHAIN_DEPTH: i64 = 10;

/// Decode a child task's `pending_input` (JSON written by an upstream task on success) into
/// What an upstream task left for a chained child in its `pending_input`.
#[derive(Default)]
struct ChainTrigger {
    /// Prompt-context prefix (the upstream output), or None for a normal scheduled run.
    context: Option<String>,
    /// Chain depth so far (loop guard).
    depth: i64,
    /// Parent task id (0 for a scheduled run).
    from_id: i64,
    /// Short note on why this run fired (e.g. the condition verdict).
    via: String,
}

/// a prompt-context prefix, chain depth, and provenance. Returns the default (no context,
/// depth 0, from_id 0) for a normal scheduled run or invalid input.
fn parse_pending_input(pending_input: &str) -> ChainTrigger {
    if pending_input.trim().is_empty() {
        return ChainTrigger::default();
    }
    match serde_json::from_str::<serde_json::Value>(pending_input) {
        Ok(v) => {
            let from = v.get("from").and_then(|x| x.as_str()).unwrap_or("a previous task");
            let output = v.get("output").and_then(|x| x.as_str()).unwrap_or("");
            let depth = v.get("depth").and_then(|x| x.as_i64()).unwrap_or(0);
            let from_id = v.get("from_id").and_then(|x| x.as_i64()).unwrap_or(0);
            let via = v.get("via").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let context = if output.trim().is_empty() {
                None
            } else {
                Some(format!("Output from the upstream task \"{}\":\n\n{}", from, output))
            };
            ChainTrigger { context, depth, from_id, via }
        }
        Err(_) => ChainTrigger::default(),
    }
}

/// Parse a task's `chain_next` column (JSON array of id strings, e.g. `["3","7"]`) into ids.
/// Tolerates bare numbers and a single id string; returns empty on blank/invalid input.
fn parse_chain_next(chain_next: &str) -> Vec<i64> {
    let trimmed = chain_next.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return items
            .iter()
            .filter_map(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
            .collect();
    }
    // Fall back to a single bare id ("5").
    trimmed.parse().map(|id| vec![id]).unwrap_or_default()
}

/// Parse a human-friendly schedule string into the next run time from `now`.
///
/// Supported formats:
///   "every 30s"              → every 30 seconds
///   "every 5m"               → every 5 minutes
///   "every 2h"               → every 2 hours
///   "every 1d"               → every 1 day
///   "daily at 09:00"         → every day at 09:00
///   "daily at 14:30"         → every day at 14:30
///   "every monday at 10:00"  → weekly on monday at 10:00
///   "every friday at 17:00"  → weekly on friday at 17:00
///   "hourly"                 → every hour on the hour
///   "daily"                  → every day at midnight
pub fn next_run_from_schedule(schedule: &str, from: NaiveDateTime) -> Option<NaiveDateTime> {
    let s = schedule.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }

    // "every Ns/Nm/Nh/Nd"
    if s.starts_with("every ") {
        let rest = s[6..].trim();

        // "every <weekday> at HH:MM"
        if let Some(next) = parse_weekly_at(rest, from) {
            return Some(next);
        }

        // "every 30m" style
        if let Some(delta) = parse_interval(rest) {
            return Some(from + delta);
        }
    }

    // "daily at HH:MM"
    if s.starts_with("daily at ") {
        let time_str = s[9..].trim();
        if let Some(time) = parse_hhmm(time_str) {
            let today = from.date().and_time(time);
            return if today > from {
                Some(today)
            } else {
                Some(today + TimeDelta::days(1))
            };
        }
    }

    // "hourly"
    if s == "hourly" {
        let next = from
            .date()
            .and_hms_opt(from.hour(), 0, 0)
            .unwrap_or(from)
            + TimeDelta::hours(1);
        return Some(next);
    }

    // "daily"
    if s == "daily" {
        let tomorrow = from.date().and_hms_opt(0, 0, 0).unwrap_or(from) + TimeDelta::days(1);
        return Some(tomorrow);
    }

    None
}

fn parse_interval(s: &str) -> Option<TimeDelta> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let n: i64 = num_str.trim().parse().ok()?;
    if n <= 0 {
        return None;
    }
    match unit {
        "s" => Some(TimeDelta::seconds(n)),
        "m" => Some(TimeDelta::minutes(n)),
        "h" => Some(TimeDelta::hours(n)),
        "d" => Some(TimeDelta::days(n)),
        _ => None,
    }
}

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let h: u32 = parts[0].trim().parse().ok()?;
    let m: u32 = parts[1].trim().parse().ok()?;
    NaiveTime::from_hms_opt(h, m, 0)
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    match s {
        "monday" | "mon" => Some(Weekday::Mon),
        "tuesday" | "tue" => Some(Weekday::Tue),
        "wednesday" | "wed" => Some(Weekday::Wed),
        "thursday" | "thu" => Some(Weekday::Thu),
        "friday" | "fri" => Some(Weekday::Fri),
        "saturday" | "sat" => Some(Weekday::Sat),
        "sunday" | "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

fn parse_weekly_at(rest: &str, from: NaiveDateTime) -> Option<NaiveDateTime> {
    // rest = "monday at 10:00"
    let parts: Vec<&str> = rest.splitn(2, " at ").collect();
    if parts.len() != 2 {
        return None;
    }
    let weekday = parse_weekday(parts[0].trim())?;
    let time = parse_hhmm(parts[1].trim())?;

    let current_weekday = from.weekday();
    let days_ahead = (weekday.num_days_from_monday() as i64
        - current_weekday.num_days_from_monday() as i64
        + 7)
        % 7;

    let candidate = from.date().and_time(time) + TimeDelta::days(days_ahead);
    if candidate > from {
        Some(candidate)
    } else {
        Some(candidate + TimeDelta::days(7))
    }
}

/// Next run from either schedule shape, anchored at `from` (schedule_config wins).
fn next_run_from_any(schedule: &str, schedule_config: &str, from: NaiveDateTime) -> Option<NaiveDateTime> {
    if !schedule_config.trim().is_empty() {
        next_run_from_schedule_config(schedule_config, from)
    } else if !schedule.trim().is_empty() {
        next_run_from_schedule(schedule, from)
    } else {
        None
    }
}

/// Compute a task's new `next_run` after a run finishes, without interval drift.
///
/// `due_at` is the task's `next_run` as loaded at claim time (the slot this run was
/// due at), `now` is the completion time, `manual` is whether this was a manual
/// trigger (run_task_now / run_task tool) rather than a scheduler tick.
///
/// - **Scheduled runs** anchor the recomputation on the due slot, so "every 30m"
///   due at 14:00 that finishes at 14:03 yields 14:30 (not 14:33 — the drift bug).
///   If the slot-anchored result is already <= now (we ran very late), recompute
///   from `now` instead so a catch-up run doesn't unleash a storm of missed slots.
///   An unparseable `due_at` falls back to `now` (the old behavior).
/// - **Manual runs** must not disturb the schedule: if the prior `next_run` is
///   non-empty, parses, and is still in the future, it is kept unchanged. A stale
///   or missing one is recomputed from `now` as before.
///
/// Returns the formatted timestamp, or "" when no next run is computable.
fn next_run_after_run(
    schedule: &str,
    schedule_config: &str,
    due_at: &str,
    now: NaiveDateTime,
    manual: bool,
) -> String {
    let fmt = |dt: NaiveDateTime| dt.format("%Y-%m-%dT%H:%M:%S").to_string();
    let due = NaiveDateTime::parse_from_str(due_at.trim(), "%Y-%m-%dT%H:%M:%S").ok();

    if manual {
        // A manual run of a recurring task must not rebase its schedule.
        if let Some(prior) = due {
            if prior > now {
                return due_at.trim().to_string();
            }
        }
        return next_run_from_any(schedule, schedule_config, now).map(fmt).unwrap_or_default();
    }

    // Scheduled run: anchor on the due slot to avoid drifting by run duration.
    if let Some(slot) = due {
        if let Some(next) = next_run_from_any(schedule, schedule_config, slot) {
            if next > now {
                return fmt(next);
            }
            // Slot-anchored next run is already in the past (we ran very late) —
            // rebase on now so we don't schedule an immediate catch-up storm.
        }
    }
    next_run_from_any(schedule, schedule_config, now).map(fmt).unwrap_or_default()
}

fn weekday_to_key(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

/// Find the next run from a day config on a specific date.
fn next_run_on_date(date: NaiveDate, cfg: &DayConfig, after: Option<NaiveDateTime>) -> Option<NaiveDateTime> {
    match cfg.mode.as_str() {
        "times" => {
            let times = cfg.times.as_ref()?;
            let mut parsed: Vec<NaiveTime> = times.iter().filter_map(|t| parse_hhmm(t)).collect();
            parsed.sort();
            for t in parsed {
                let candidate = date.and_time(t);
                if let Some(ref a) = after {
                    if candidate <= *a {
                        continue;
                    }
                }
                return Some(candidate);
            }
            None
        }
        "continuous" => {
            let mins = cfg.interval_mins.unwrap_or(60) as i64;
            if mins <= 0 {
                return None;
            }
            if let Some(ref a) = after {
                if a.date() == date {
                    let next = *a + TimeDelta::minutes(mins);
                    if next.date() == date {
                        return Some(next);
                    }
                    return None;
                }
            }
            // Start of day
            date.and_hms_opt(0, 0, 0)
        }
        _ => None,
    }
}

/// Parse a JSON schedule config and compute the next run time from `from`.
pub fn next_run_from_schedule_config(
    config_json: &str,
    from: NaiveDateTime,
) -> Option<NaiveDateTime> {
    let config: ScheduleConfig = serde_json::from_str(config_json).ok()?;
    let start = NaiveDate::parse_from_str(&config.start_date, "%Y-%m-%d").ok()?;
    let end = config
        .end_date
        .as_ref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

    // Determine the effective start point
    let effective_from = if from.date() < start {
        start.and_hms_opt(0, 0, 0)?
    } else {
        from
    };

    // Past end date?
    if let Some(end_date) = end {
        if effective_from.date() > end_date {
            return None;
        }
    }

    // Check today first
    let today_key = weekday_to_key(effective_from.weekday());
    if let Some(Some(day_cfg)) = config.days.get(today_key) {
        if let Some(next) = next_run_on_date(effective_from.date(), day_cfg, Some(from)) {
            if end.map_or(true, |e| next.date() <= e) {
                return Some(next);
            }
        }
    }

    // Search forward up to 7 days
    for offset in 1..=7 {
        let candidate_date = effective_from.date() + TimeDelta::days(offset);
        if let Some(end_date) = end {
            if candidate_date > end_date {
                return None;
            }
        }
        let key = weekday_to_key(candidate_date.weekday());
        if let Some(Some(day_cfg)) = config.days.get(key) {
            if let Some(next) = next_run_on_date(candidate_date, day_cfg, None) {
                return Some(next);
            }
        }
    }
    None
}

/// Reset tasks stranded in 'running' by a quit/crash mid-run. The scheduler only
/// picks up 'scheduled' tasks, so without this a task interrupted once would never
/// fire again. Recurring tasks rejoin their schedule; one-shot tasks retry
/// immediately (their interrupted run never completed). 'paused' and 'failed'
/// tasks are deliberately left untouched — those states are user/engine-managed.
pub fn recover_stuck_tasks(conn: &rusqlite::Connection) {
    let now = Local::now().naive_local();
    let stuck: Vec<(i64, String, String, String)> = match conn
        .prepare("SELECT id, name, schedule, schedule_config FROM tasks WHERE status = 'running'")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
        }) {
        Ok(rows) => rows,
        Err(_) => return,
    };

    for (id, name, schedule, schedule_config) in stuck {
        let next = if !schedule_config.trim().is_empty() {
            next_run_from_schedule_config(&schedule_config, now)
        } else if !schedule.trim().is_empty() {
            next_run_from_schedule(&schedule, now)
        } else {
            None
        };
        let next_str = next.unwrap_or(now).format("%Y-%m-%dT%H:%M:%S").to_string();
        let _ = conn.execute(
            "UPDATE tasks SET status = 'scheduled', next_run = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![id, next_str],
        );
        eprintln!(
            "[Scheduler] recovered task '{}' (id {}) stuck in 'running'; next run {}",
            name, id, next_str
        );
    }
}

/// "2d 4h", "6h 12m", "45m", "30s" — for labeling late catch-up runs.
fn human_duration(d: TimeDelta) -> String {
    let secs = d.num_seconds().max(0);
    let (days, hours, mins) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        format!("{}s", secs)
    }
}

/// Fail a claimed run before the LLM is ever called (invalid workspace path, missing or
/// disabled agent). Applies the same status rules as an errored LLM run — recurring tasks
/// rejoin their schedule, schedule-less tasks go 'failed', a manually-run paused task is
/// restored to 'paused' — records a task_run with the error, and notifies the feed.
/// Deliberately does NOT run the task unscoped: a task pinned to a workspace that no
/// longer resolves must fail loudly, not silently fall back to broader access.
#[allow(clippy::too_many_arguments)]
fn fail_run_early(
    app: &AppHandle,
    state: &AppState,
    task_id: i64,
    task_name: &str,
    prior_status: &str,
    new_next_run: &str,
    trigger_kind: &str,
    trigger_from_id: i64,
    started_at: &str,
    error: &str,
) {
    let mut final_status = if !new_next_run.is_empty() { "scheduled" } else { "failed" };
    if final_status == "scheduled" && prior_status == "paused" {
        final_status = "paused";
    }
    let preview = format!("Error: {}", truncate_chars(error, 180));
    let _ = with_db(state, |db| {
        db.execute(
            "UPDATE tasks SET status = ?2, next_run = ?3, run_count = run_count + 1, \
             result = ?4, completed_at = '', updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![task_id, final_status, new_next_run, preview],
        )
        .map_err(|e| e.to_string())
    });
    let _ = app.emit("tasks-changed", ());
    add_feed_item(
        state,
        "task",
        &format!("Task failed: {}", task_name),
        &preview,
        "error",
        "",
    );
    crate::db::insert_task_run(
        state, task_id, "", trigger_kind, trigger_from_id,
        "failed", &format!("Error: {}", error), "", started_at,
    );
}

/// Atomic claim used by scheduler ticks: only a 'scheduled' task is claimable, and the
/// rows-changed check in execute_task makes concurrent claims mutually exclusive.
const CLAIM_SCHEDULED_SQL: &str =
    "UPDATE tasks SET status = 'running', pending_input = '', updated_at = CURRENT_TIMESTAMP \
     WHERE id = ?1 AND status = 'scheduled'";

/// Atomic claim used by manual triggers (run_task_now command / run_task tool): anything
/// not currently running is claimable — including 'completed', 'failed', and 'paused'
/// (a manually-run paused task is restored to 'paused' after the run).
const CLAIM_MANUAL_SQL: &str =
    "UPDATE tasks SET status = 'running', pending_input = '', updated_at = CURRENT_TIMESTAMP \
     WHERE id = ?1 AND status != 'running'";

/// Run a task once: claim it, create a session, send its description to the LLM,
/// record the run, and fire any chained children. Re-loads the task row itself so
/// callers only need an id. Used by the scheduler tick (manual = false), the
/// `run_task_now` command, and the `run_task` agent tool (manual = true).
///
/// The claim is ATOMIC (`UPDATE ... SET status = 'running' WHERE id = ? AND status ...`
/// with a rows-changed check), which is the double-run guard: a scheduler tick can only
/// claim a 'scheduled' task; a manual trigger can claim anything not currently 'running'.
///
/// Status rules after the run:
/// - success + recurring schedule with a computable next_run → 'scheduled'
/// - success + no schedule (one-shot, whether scheduled or manual) → 'completed'
/// - error + computable next_run → 'scheduled' (recurring tasks rejoin their schedule)
/// - error + no schedule / next_run not computable → 'failed' (re-fire with run_task)
/// - a manually-run task that was 'paused' goes back to 'paused' instead of 'scheduled',
///   so a one-off manual run never silently resumes a paused schedule.
pub async fn execute_task(app: AppHandle, task_id: i64, manual: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    let now = Local::now().naive_local();
    let now_str = now.format("%Y-%m-%dT%H:%M:%S").to_string();

    // Load everything the run needs up front.
    #[allow(clippy::type_complexity)]
    let row: Option<(String, String, String, String, String, String, String, String, String, String)> =
        with_db(&state, |db| {
            db.query_row(
                "SELECT name, description, schedule, schedule_config, next_run, chain_next, pending_input, status, project_path, agent_id \
                 FROM tasks WHERE id = ?1",
                params![task_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| e.to_string())
        })?;
    let (task_name, description, schedule, schedule_config, due_at, chain_next, pending_input, prior_status, project_path, agent_id) =
        row.ok_or_else(|| format!("no task with id {}", task_id))?;
    let project_path = project_path.trim().to_string();
    let agent_id = agent_id.trim().to_string();

    // Atomic claim — the double-run guard. Also clears consumed chain input so it
    // can't replay on a later tick.
    let claim_sql = if manual { CLAIM_MANUAL_SQL } else { CLAIM_SCHEDULED_SQL };
    let claimed = with_db(&state, |db| {
        db.execute(claim_sql, params![task_id]).map_err(|e| e.to_string())
    })?;
    if claimed == 0 {
        return Err(format!("task {} ('{}') is already running", task_id, task_name));
    }

    // A run firing well past its slot is a catch-up (the app or the Mac was
    // down at the scheduled time) — label it so it doesn't look random.
    // Manual runs aren't tied to a slot, so they never get the label.
    let late_label = if manual {
        None
    } else {
        NaiveDateTime::parse_from_str(&due_at, "%Y-%m-%dT%H:%M:%S")
            .ok()
            .map(|slot| now - slot)
            .filter(|late| *late > TimeDelta::minutes(5))
            .map(|late| format!(" ({} late — missed the {} slot)", human_duration(late), due_at.replace('T', " ")))
    };

    // If a chained upstream task fired this run, it left its output in pending_input.
    let trigger = parse_pending_input(&pending_input);
    let run_started_at = now_str.clone();
    let trigger_kind = if manual {
        "manual"
    } else if trigger.from_id != 0 {
        "chain"
    } else {
        "scheduled"
    };

    let _ = app.emit("tasks-changed", ());

    add_feed_item(
        &state,
        "task",
        &format!("Task started: {}{}", task_name, late_label.as_deref().unwrap_or("")),
        &truncate_chars(&description, 120),
        "clock",
        "",
    );

    // ---- Per-task workspace scope + custom agent (validated before any LLM spend) ----
    // A non-empty project_path scopes the run's file/shell tools to that folder. If it
    // no longer resolves (folder deleted/renamed), the run fails — never runs unscoped.
    let scope: Option<WorkspaceScope> = if project_path.is_empty() {
        None
    } else {
        match WorkspaceScope::from_path(&project_path) {
            Ok(s) => Some(s),
            Err(e) => {
                let next = next_run_after_run(
                    &schedule, &schedule_config, &due_at, Local::now().naive_local(), manual,
                );
                fail_run_early(
                    &app, &state, task_id, &task_name, &prior_status, &next,
                    trigger_kind, trigger.from_id, &run_started_at,
                    &format!("workspace path '{}' is invalid: {}", project_path, e),
                );
                return Ok(());
            }
        }
    };
    // A non-empty agent_id runs the task through that custom agent (its system prompt +
    // tool allowlist). A deleted or disabled agent fails the run the same way.
    if !agent_id.is_empty() {
        let agent_err = match load_agent_by_id(&state, &agent_id) {
            Ok(agent) if agent.enabled => None,
            Ok(agent) => Some(format!("agent '{}' ({}) is disabled", agent.name, agent_id)),
            Err(e) => Some(format!("agent '{}' is unavailable: {}", agent_id, e)),
        };
        if let Some(msg) = agent_err {
            let next = next_run_after_run(
                &schedule, &schedule_config, &due_at, Local::now().naive_local(), manual,
            );
            fail_run_early(
                &app, &state, task_id, &task_name, &prior_status, &next,
                trigger_kind, trigger.from_id, &run_started_at, &msg,
            );
            return Ok(());
        }
    }

    // Create a dedicated session for this task run. Sessions of workspace-pinned tasks
    // carry the task's project_path so reopening the session keeps the same scope.
    let session_id = uuid::Uuid::new_v4().to_string();
    let session_project: &str = if project_path.is_empty() { "scheduled-task" } else { &project_path };
    let _ = with_db(&state, |db| {
        db.execute(
            "INSERT INTO sessions (id, project_path) VALUES (?1, ?2)",
            params![session_id, session_project],
        )
        .map_err(|e| e.to_string())
    });

    // Build the task prompt
    let base_prompt = if description.trim().is_empty() {
        format!("Execute the scheduled task: {}", task_name)
    } else {
        description.clone()
    };
    // Prepend upstream output when this run was triggered by a chained task.
    let prompt = match &trigger.context {
        Some(ctx) => format!("{}\n\n---\n\n{}", ctx, base_prompt),
        None => base_prompt,
    };

    // Save user message (structured so future replays keep the turn intact).
    let user_blocks = vec![serde_json::json!({"type": "text", "text": prompt.clone()})];
    let _ = insert_message(&state, &session_id, "user", &prompt, &user_blocks, false);

    let history = vec![HistoryMessage::text("user", &prompt)];

    // Determine provider/model
    let provider = get_app_setting_value(&state, "llm/provider")
        .ok()
        .flatten()
        .unwrap_or_else(|| "minimax".to_string());
    let model = get_app_setting_value(&state, "llm/model")
        .ok()
        .flatten()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| {
            if provider == "minimax" {
                "MiniMax-M2.7".to_string()
            } else {
                "gpt-4.1-mini".to_string()
            }
        });

    // Run LLM. All task runs are non-interactive (no user to ask — dangerous commands
    // hard-block) and carry the task's workspace scope (None = default workspace root).
    let result = if !agent_id.is_empty() {
        run_agent(&app, &state, &session_id, &agent_id, &history, scope.as_ref(), false).await
    } else if provider == "minimax" || model.starts_with("MiniMax-") {
        chat_with_minimax_anthropic(&app, &state, &session_id, &model, &history, scope.as_ref(), false).await
    } else if provider == "openai" {
        chat_with_openai(&app, &state, &session_id, &model, &history, scope.as_ref(), false).await
    } else {
        chat_dynamic(&app, &state, &session_id, &provider, &model, &history, scope.as_ref(), false).await
    };

    let (reply_preview, full_output) = match &result {
        Ok((reply, blocks)) => {
            let _ = insert_message(&state, &session_id, "assistant", reply, blocks, false);
            (truncate_chars(reply, 200), reply.clone())
        }
        Err(e) => (format!("Error: {}", truncate_chars(e, 180)), String::new()),
    };

    // Calculate next run (prefer schedule_config over legacy schedule). Scheduled runs
    // anchor on the due slot so interval schedules don't drift by the run's duration;
    // manual runs keep a still-future next_run untouched. See next_run_after_run.
    let new_next_run = next_run_after_run(
        &schedule, &schedule_config, &due_at, Local::now().naive_local(), manual,
    );

    // Success: rejoin the schedule if there is one, else the one-shot is done.
    // Error: rejoin the schedule if a next run is computable, else mark 'failed'
    // (a schedule-less errored task would otherwise sit in 'scheduled' forever
    // with an empty next_run). A manual run of a paused recurring task goes back
    // to 'paused' rather than silently resuming the schedule.
    let has_schedule = (!schedule.trim().is_empty()) || (!schedule_config.trim().is_empty());
    let mut final_status = match &result {
        Ok(_) if has_schedule && !new_next_run.is_empty() => "scheduled",
        Ok(_) => "completed",
        Err(_) if !new_next_run.is_empty() => "scheduled",
        Err(_) => "failed",
    };
    if final_status == "scheduled" && prior_status == "paused" {
        final_status = "paused";
    }

    let completed_at = if final_status == "completed" {
        chrono::Local::now().naive_local().format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        String::new()
    };
    let _ = with_db(&state, |db| {
        db.execute(
            "UPDATE tasks SET status = ?2, next_run = ?3, run_count = run_count + 1, \
             result = ?4, completed_at = ?5, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![task_id, final_status, new_next_run, reply_preview, completed_at],
        )
        .map_err(|e| e.to_string())
    });

    let _ = app.emit("tasks-changed", ());

    add_feed_item(
        &state,
        "task",
        &format!("Task {}: {}", if result.is_ok() { "completed" } else { "failed" }, task_name),
        &reply_preview,
        if result.is_ok() { "checkmark" } else { "error" },
        &session_id,
    );

    crate::notify_user::notify_user(
        &app,
        &format!(
            "Task {}: {}{}",
            if result.is_ok() { "completed" } else { "failed" },
            task_name,
            if late_label.is_some() { " (ran late)" } else { "" }
        ),
        &reply_preview,
    );

    // ---- Record this run (independent of the task's reset status) ----
    let run_status = if result.is_ok() { "completed" } else { "failed" };
    let run_output = match &result {
        Ok((reply, _)) => reply.clone(),
        Err(e) => format!("Error: {}", e),
    };
    // Note: condition verdict for chain runs, else the late/catch-up label if any.
    let run_note = if !trigger.via.is_empty() {
        trigger.via.clone()
    } else {
        late_label.as_deref().map(|s| s.trim().trim_matches(['(', ')']).to_string()).unwrap_or_default()
    };
    crate::db::insert_task_run(
        &state, task_id, &session_id, trigger_kind, trigger.from_id,
        run_status, &run_output, &run_note, &run_started_at,
    );

    // ---- Chaining ----
    // On success, fire downstream tasks listed in chain_next. Each child is queued for
    // the next tick (next_run = now) with this task's full output as pending_input.
    if result.is_ok() {
        let child_ids = parse_chain_next(&chain_next);
        let next_depth = trigger.depth + 1;
        if !child_ids.is_empty() && next_depth > MAX_CHAIN_DEPTH {
            add_feed_item(
                &state,
                "task",
                &format!("Chain stopped: {} (max depth {} reached)", task_name, MAX_CHAIN_DEPTH),
                "Possible loop in chained tasks — children were not triggered.",
                "error",
                "",
            );
        } else {
            for child_id in child_ids {
                if child_id == task_id {
                    continue; // never chain a task to itself
                }

                // A child may carry a natural-language gate condition; evaluate it
                // against this task's output before deciding whether to fire it.
                let condition: String = with_db(&state, |db| {
                    db.query_row(
                        "SELECT conditions FROM tasks WHERE id = ?1",
                        params![child_id],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|e| e.to_string())
                })
                .unwrap_or_default();

                let (proceed, via_note) = if condition.trim().is_empty() {
                    (true, String::new())
                } else {
                    let (met, reason) =
                        crate::llm::evaluate_chain_condition(&state, &condition, &full_output).await;
                    let note = format!(
                        "{} — {}: {}",
                        now_str.replace('T', " "),
                        if met { "met" } else { "not met" },
                        truncate_chars(&reason, 160)
                    );
                    let _ = with_db(&state, |db| {
                        db.execute(
                            "UPDATE tasks SET condition_status = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
                            params![child_id, note],
                        )
                        .map_err(|e| e.to_string())
                    });
                    if !met {
                        add_feed_item(
                            &state,
                            "task",
                            &format!("Chain skipped: {} → task #{} (condition not met)", task_name, child_id),
                            &truncate_chars(&reason, 120),
                            "info",
                            "",
                        );
                    }
                    (met, if met { format!("condition met — {}", truncate_chars(&reason, 120)) } else { String::new() })
                };
                if !proceed {
                    continue;
                }

                let pending = serde_json::json!({
                    "from": task_name,
                    "from_id": task_id,
                    "via": via_note,
                    "output": full_output,
                    "depth": next_depth,
                })
                .to_string();
                // A 'paused' child is never chain-triggered — pausing a task must stop
                // every path that runs it, not just its own schedule. ('completed'
                // children ARE revived: schedule-less chain children complete after
                // each run and rely on the parent to re-arm them.)
                let updated = with_db(&state, |db| {
                    db.execute(
                        "UPDATE tasks SET pending_input = ?2, triggered_by = ?3, next_run = ?4, \
                         status = 'scheduled', updated_at = CURRENT_TIMESTAMP \
                         WHERE id = ?1 AND status NOT IN ('running', 'paused')",
                        params![child_id, pending, task_id.to_string(), now_str],
                    )
                    .map_err(|e| e.to_string())
                });
                if matches!(updated, Ok(n) if n > 0) {
                    add_feed_item(
                        &state,
                        "task",
                        &format!("Chain triggered: {} → task #{}", task_name, child_id),
                        &truncate_chars(&reply_preview, 120),
                        "clock",
                        "",
                    );
                } else {
                    add_feed_item(
                        &state,
                        "task",
                        &format!("Chain skipped: {} → task #{} (paused, running, or deleted)", task_name, child_id),
                        "",
                        "info",
                        "",
                    );
                }
            }
            let _ = app.emit("tasks-changed", ());
        }
    }

    Ok(())
}

/// The main scheduler loop. Spawned once at app startup.
pub async fn run_scheduler(app: AppHandle) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let state = app.state::<AppState>();
        let now = Local::now().naive_local();
        let now_str = now.format("%Y-%m-%dT%H:%M:%S").to_string();

        // Find due tasks: status = 'scheduled', next_run is non-empty and <= now.
        // Each due task runs concurrently on its own spawned future; the atomic
        // claim inside execute_task is the double-run guard (a later tick can't
        // re-claim a task that's still 'running').
        let due_tasks: Vec<i64> = match with_db(&state, |db| {
            let mut stmt = db
                .prepare(
                    "SELECT id FROM tasks \
                     WHERE status = 'scheduled' AND next_run != '' AND next_run <= ?1",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![now_str], |row| row.get::<_, i64>(0))
                .map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| e.to_string())?);
            }
            Ok(out)
        }) {
            Ok(tasks) => tasks,
            Err(_) => continue,
        };

        for task_id in due_tasks {
            let task_app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = execute_task(task_app, task_id, false).await {
                    eprintln!("[Scheduler] task {} not run: {}", task_id, e);
                }
            });
        }

        // ---- Feed auto-refresh ----
        // Find feeds due for refresh: schedule_config feeds use next_run, legacy feeds use interval math.
        let due_feeds: Vec<(i64, String, String)> = match with_db(&state, |db| {
            let mut stmt = db
                .prepare(
                    "SELECT id, name, schedule_config FROM ai_feeds \
                     WHERE (schedule_config != '' AND next_run != '' AND next_run <= ?1) \
                        OR (schedule_config = '' AND (last_refreshed_at = '' \
                            OR datetime(last_refreshed_at, '+' || refresh_interval_mins || ' minutes') <= datetime(?1)))",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![now_str], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                })
                .map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| e.to_string())?);
            }
            Ok(out)
        }) {
            Ok(feeds) => feeds,
            Err(_) => vec![],
        };

        for (feed_id, feed_name, feed_schedule_config) in due_feeds {
            match crate::feeds::refresh_feed_impl(state.inner(), feed_id, &app).await {
                Ok(items_added) => {
                    add_feed_item(
                        &state,
                        "feed",
                        &format!("Feed refreshed: {} (+{} items)", feed_name, items_added),
                        "",
                        "checkmark",
                        "",
                    );
                    // Only interrupt the user when the refresh actually produced something.
                    if items_added > 0 {
                        crate::notify_user::notify_user(
                            &app,
                            &format!("{}: {} new item{}", feed_name, items_added, if items_added == 1 { "" } else { "s" }),
                            "Open Muninn to read",
                        );
                    }
                }
                Err(e) => {
                    add_feed_item(
                        &state,
                        "feed",
                        &format!("Feed refresh failed: {}", feed_name),
                        &truncate_chars(&e, 120),
                        "error",
                        "",
                    );
                }
            }

            // Compute and store next_run for schedule_config feeds
            if !feed_schedule_config.trim().is_empty() {
                let new_next = next_run_from_schedule_config(&feed_schedule_config, now)
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
                    .unwrap_or_default();
                let _ = with_db(&state, |db| {
                    db.execute(
                        "UPDATE ai_feeds SET next_run = ?2 WHERE id = ?1",
                        params![feed_id, new_next],
                    )
                    .map_err(|e| e.to_string())
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    #[test]
    fn parse_chain_next_handles_arrays_and_bare_ids() {
        assert_eq!(parse_chain_next(""), Vec::<i64>::new());
        assert_eq!(parse_chain_next("   "), Vec::<i64>::new());
        assert_eq!(parse_chain_next("[3,7]"), vec![3, 7]);
        assert_eq!(parse_chain_next(r#"["3","7"]"#), vec![3, 7]);
        assert_eq!(parse_chain_next("5"), vec![5]);
        assert_eq!(parse_chain_next("garbage"), Vec::<i64>::new());
    }

    #[test]
    fn parse_pending_input_extracts_context_and_provenance() {
        let t = parse_pending_input(
            r#"{"from":"Refresh","from_id":3,"via":"condition met — 2 unread","output":"hello","depth":2}"#,
        );
        assert_eq!(t.depth, 2);
        assert_eq!(t.from_id, 3);
        assert_eq!(t.via, "condition met — 2 unread");
        let ctx = t.context.unwrap();
        assert!(ctx.contains("Refresh"));
        assert!(ctx.contains("hello"));

        // Blank / invalid → no context, defaults.
        let blank = parse_pending_input("");
        assert!(blank.context.is_none() && blank.depth == 0 && blank.from_id == 0);
        assert!(parse_pending_input("not json").context.is_none());
        // Empty output yields no context but preserves depth for the loop guard.
        let empty = parse_pending_input(r#"{"output":"","depth":4}"#);
        assert!(empty.context.is_none());
        assert_eq!(empty.depth, 4);
    }

    #[test]
    fn test_every_30m() {
        let from = dt(2026, 3, 10, 14, 0);
        let next = next_run_from_schedule("every 30m", from).unwrap();
        assert_eq!(next, dt(2026, 3, 10, 14, 30));
    }

    #[test]
    fn test_every_2h() {
        let from = dt(2026, 3, 10, 14, 0);
        let next = next_run_from_schedule("every 2h", from).unwrap();
        assert_eq!(next, dt(2026, 3, 10, 16, 0));
    }

    #[test]
    fn test_daily_at() {
        let from = dt(2026, 3, 10, 8, 0);
        let next = next_run_from_schedule("daily at 09:00", from).unwrap();
        assert_eq!(next, dt(2026, 3, 10, 9, 0));

        // If already past, should be tomorrow
        let from2 = dt(2026, 3, 10, 10, 0);
        let next2 = next_run_from_schedule("daily at 09:00", from2).unwrap();
        assert_eq!(next2, dt(2026, 3, 11, 9, 0));
    }

    #[test]
    fn test_weekly() {
        // March 10, 2026 is a Tuesday
        let from = dt(2026, 3, 10, 8, 0);
        let next = next_run_from_schedule("every friday at 17:00", from).unwrap();
        assert_eq!(next, dt(2026, 3, 13, 17, 0));
    }

    #[test]
    fn test_hourly() {
        let from = dt(2026, 3, 10, 14, 23);
        let next = next_run_from_schedule("hourly", from).unwrap();
        assert_eq!(next, dt(2026, 3, 10, 15, 0));
    }

    #[test]
    fn test_empty_returns_none() {
        let from = dt(2026, 3, 10, 14, 0);
        assert!(next_run_from_schedule("", from).is_none());
    }

    #[test]
    fn test_invalid_returns_none() {
        let from = dt(2026, 3, 10, 14, 0);
        assert!(next_run_from_schedule("nonsense", from).is_none());
    }

    // ---- next_run_after_run (interval-drift correction) ----

    #[test]
    fn scheduled_run_anchors_on_due_slot_not_completion_time() {
        // "every 30m" due 14:00, finishes 14:03 → next is 14:30, not 14:33 (no drift).
        let next = next_run_after_run(
            "every 30m", "", "2026-03-10T14:00:00", dt(2026, 3, 10, 14, 3), false,
        );
        assert_eq!(next, "2026-03-10T14:30:00");
    }

    #[test]
    fn scheduled_run_anchors_continuous_schedule_config_on_due_slot() {
        // Continuous-mode schedule_config drifts the same way; slot anchoring fixes it.
        let config = r#"{"version":1,"startDate":"2026-01-01","days":{"tue":{"mode":"continuous","intervalMins":30}}}"#;
        let next = next_run_after_run(
            "", config, "2026-03-10T14:00:00", dt(2026, 3, 10, 14, 3), false,
        );
        assert_eq!(next, "2026-03-10T14:30:00");
    }

    #[test]
    fn very_late_scheduled_run_rebases_on_now_no_catchup_storm() {
        // Due 3 hours ago on "every 30m": slot-anchored 11:30 is long past — recompute
        // from now so the next run is in the future, not an immediate catch-up burst.
        let now = dt(2026, 3, 10, 14, 3);
        let next = next_run_after_run("every 30m", "", "2026-03-10T11:00:00", now, false);
        assert_eq!(next, "2026-03-10T14:33:00");
        assert!(NaiveDateTime::parse_from_str(&next, "%Y-%m-%dT%H:%M:%S").unwrap() > now);
    }

    #[test]
    fn scheduled_run_with_unparseable_due_at_falls_back_to_now() {
        let next = next_run_after_run("every 30m", "", "", dt(2026, 3, 10, 14, 3), false);
        assert_eq!(next, "2026-03-10T14:33:00");
        let next = next_run_after_run("every 30m", "", "garbage", dt(2026, 3, 10, 14, 3), false);
        assert_eq!(next, "2026-03-10T14:33:00");
    }

    #[test]
    fn manual_run_preserves_a_future_next_run() {
        // Manually firing a recurring task must not rebase its schedule.
        let next = next_run_after_run(
            "every 30m", "", "2026-03-10T15:00:00", dt(2026, 3, 10, 14, 3), true,
        );
        assert_eq!(next, "2026-03-10T15:00:00");
    }

    #[test]
    fn manual_run_with_stale_next_run_recomputes_from_now() {
        // Prior next_run already in the past → recompute from now (old behavior).
        let next = next_run_after_run(
            "every 30m", "", "2026-03-10T14:00:00", dt(2026, 3, 10, 14, 3), true,
        );
        assert_eq!(next, "2026-03-10T14:33:00");
        // Empty/unparseable prior next_run → also recompute from now.
        let next = next_run_after_run("every 30m", "", "", dt(2026, 3, 10, 14, 3), true);
        assert_eq!(next, "2026-03-10T14:33:00");
    }

    #[test]
    fn schedule_less_task_yields_empty_next_run() {
        assert_eq!(next_run_after_run("", "", "2026-03-10T14:00:00", dt(2026, 3, 10, 14, 3), false), "");
        assert_eq!(next_run_after_run("", "", "", dt(2026, 3, 10, 14, 3), true), "");
    }

    #[test]
    fn human_duration_formats_each_magnitude() {
        assert_eq!(human_duration(TimeDelta::seconds(30)), "30s");
        assert_eq!(human_duration(TimeDelta::minutes(45)), "45m");
        assert_eq!(human_duration(TimeDelta::minutes(372)), "6h 12m");
        assert_eq!(human_duration(TimeDelta::hours(52)), "2d 4h");
        assert_eq!(human_duration(TimeDelta::seconds(-5)), "0s");
    }

    #[test]
    fn atomic_claim_prevents_double_run() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO tasks (name, schedule, status, next_run, pending_input) \
             VALUES ('due', 'every 30m', 'scheduled', '2026-01-01T00:00:00', '{\"output\":\"x\"}')",
            [],
        )
        .unwrap();
        let id = conn.last_insert_rowid();

        // First claim wins...
        assert_eq!(conn.execute(CLAIM_SCHEDULED_SQL, params![id]).unwrap(), 1);
        // ...and consumes the pending chain input so it can't replay.
        let pending: String = conn
            .query_row("SELECT pending_input FROM tasks WHERE id = ?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, "");
        // A concurrent tick (or a second manual trigger) can't claim it again.
        assert_eq!(conn.execute(CLAIM_SCHEDULED_SQL, params![id]).unwrap(), 0);
        assert_eq!(conn.execute(CLAIM_MANUAL_SQL, params![id]).unwrap(), 0);
    }

    #[test]
    fn scheduler_claim_only_takes_scheduled_tasks() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        for status in ["paused", "completed", "failed"] {
            conn.execute(
                "INSERT INTO tasks (name, status) VALUES (?1, ?1)",
                params![status],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            // The scheduler must never claim a non-'scheduled' task...
            assert_eq!(conn.execute(CLAIM_SCHEDULED_SQL, params![id]).unwrap(), 0, "{status}");
            // ...but a manual trigger may claim any non-running task.
            assert_eq!(conn.execute(CLAIM_MANUAL_SQL, params![id]).unwrap(), 1, "{status}");
        }
    }

    #[test]
    fn recover_leaves_paused_and_failed_untouched() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        conn.execute("INSERT INTO tasks (name, status) VALUES ('p', 'paused')", []).unwrap();
        conn.execute("INSERT INTO tasks (name, status) VALUES ('f', 'failed')", []).unwrap();

        recover_stuck_tasks(&conn);

        let rows: Vec<(String, String)> = conn
            .prepare("SELECT name, status FROM tasks ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(rows[0].1, "paused");
        assert_eq!(rows[1].1, "failed");
    }

    #[test]
    fn recover_resets_stuck_running_tasks() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO tasks (name, schedule, status) VALUES ('recurring', 'daily at 09:00', 'running')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (name, schedule, status) VALUES ('oneshot', '', 'running')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (name, schedule, status, next_run) VALUES ('healthy', '', 'scheduled', '2099-01-01T00:00:00')",
            [],
        )
        .unwrap();

        recover_stuck_tasks(&conn);

        let rows: Vec<(String, String, String)> = conn
            .prepare("SELECT name, status, next_run FROM tasks ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .filter_map(Result::ok)
            .collect();

        // Recurring task rejoins its schedule with a fresh next_run.
        assert_eq!(rows[0].1, "scheduled");
        assert!(!rows[0].2.is_empty());
        // One-shot task retries immediately (next_run = now, not empty).
        assert_eq!(rows[1].1, "scheduled");
        assert!(!rows[1].2.is_empty());
        // Untouched tasks keep their state.
        assert_eq!(rows[2].1, "scheduled");
        assert_eq!(rows[2].2, "2099-01-01T00:00:00");
    }
}
