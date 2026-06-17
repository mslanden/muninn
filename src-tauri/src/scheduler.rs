use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike, Weekday};
use rusqlite::params;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;
use crate::db::{add_feed_item, get_app_setting_value, insert_message, with_db};
use crate::llm::{chat_with_minimax_anthropic, chat_with_openai};
use crate::models::{DayConfig, HistoryMessage, ScheduleConfig};
use crate::tools::truncate_chars;

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

/// Calculate next_run for a task given its schedule and optionally its previous next_run.
fn compute_next_run(schedule: &str, last_run: NaiveDateTime) -> Option<String> {
    next_run_from_schedule(schedule, last_run)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
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
/// immediately (their interrupted run never completed).
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

/// The main scheduler loop. Spawned once at app startup.
pub async fn run_scheduler(app: AppHandle) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let state = app.state::<AppState>();
        let now = Local::now().naive_local();
        let now_str = now.format("%Y-%m-%dT%H:%M:%S").to_string();

        // Find due tasks: status = 'scheduled', next_run is non-empty and <= now
        let due_tasks: Vec<(i64, String, String, String, String, String)> = match with_db(&state, |db| {
            let mut stmt = db
                .prepare(
                    "SELECT id, name, description, schedule, schedule_config, next_run FROM tasks \
                     WHERE status = 'scheduled' AND next_run != '' AND next_run <= ?1",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![now_str], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
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

        for (task_id, task_name, description, schedule, schedule_config, due_at) in due_tasks {
            // A run firing well past its slot is a catch-up (the app or the Mac was
            // down at the scheduled time) — label it so it doesn't look random.
            let late_label = NaiveDateTime::parse_from_str(&due_at, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|slot| now - slot)
                .filter(|late| *late > TimeDelta::minutes(5))
                .map(|late| format!(" ({} late — missed the {} slot)", human_duration(late), due_at.replace('T', " ")));
            // Mark as running
            let _ = with_db(&state, |db| {
                db.execute(
                    "UPDATE tasks SET status = 'running', updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
                    params![task_id],
                )
                .map_err(|e| e.to_string())
            });

            let _ = app.emit("tasks-changed", ());

            add_feed_item(
                &state,
                "task",
                &format!("Task started: {}{}", task_name, late_label.as_deref().unwrap_or("")),
                &truncate_chars(&description, 120),
                "clock",
                "",
            );

            // Create a dedicated session for this task run
            let session_id = uuid::Uuid::new_v4().to_string();
            let _ = with_db(&state, |db| {
                db.execute(
                    "INSERT INTO sessions (id, project_path) VALUES (?1, ?2)",
                    params![session_id, "scheduled-task"],
                )
                .map_err(|e| e.to_string())
            });

            // Build the task prompt
            let prompt = if description.trim().is_empty() {
                format!("Execute the scheduled task: {}", task_name)
            } else {
                description.clone()
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

            // Run LLM
            let result = if provider == "minimax" || model.starts_with("MiniMax-") {
                chat_with_minimax_anthropic(&app, &state, &session_id, &model, &history, None, false).await
            } else {
                chat_with_openai(&app, &state, &session_id, &model, &history, None, false).await
            };

            let (status, reply_preview) = match &result {
                Ok((reply, blocks)) => {
                    let _ = insert_message(&state, &session_id, "assistant", reply, blocks, false);
                    ("completed", truncate_chars(reply, 200))
                }
                Err(e) => ("scheduled", format!("Error: {}", truncate_chars(e, 180))),
            };

            // Calculate next run (prefer schedule_config over legacy schedule)
            let new_next_run = if !schedule_config.trim().is_empty() {
                next_run_from_schedule_config(&schedule_config, now)
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
                    .unwrap_or_default()
            } else {
                compute_next_run(&schedule, now).unwrap_or_default()
            };

            // If task has a recurring schedule, reset to 'scheduled' with new next_run.
            // If no schedule (one-shot), mark as completed.
            let has_schedule = (!schedule.trim().is_empty()) || (!schedule_config.trim().is_empty());
            let final_status = if has_schedule && !new_next_run.is_empty() {
                "scheduled"
            } else {
                status
            };

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

    #[test]
    fn human_duration_formats_each_magnitude() {
        assert_eq!(human_duration(TimeDelta::seconds(30)), "30s");
        assert_eq!(human_duration(TimeDelta::minutes(45)), "45m");
        assert_eq!(human_duration(TimeDelta::minutes(372)), "6h 12m");
        assert_eq!(human_duration(TimeDelta::hours(52)), "2d 4h");
        assert_eq!(human_duration(TimeDelta::seconds(-5)), "0s");
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
