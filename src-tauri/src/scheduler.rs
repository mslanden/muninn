use chrono::{Datelike, Local, NaiveDateTime, NaiveTime, TimeDelta, Timelike, Weekday};
use rusqlite::params;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;
use crate::db::{add_feed_item, get_app_setting_value, with_db};
use crate::llm::{chat_with_minimax_anthropic, chat_with_openai};
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

/// The main scheduler loop. Spawned once at app startup.
pub async fn run_scheduler(app: AppHandle) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let state = app.state::<AppState>();
        let now = Local::now().naive_local();
        let now_str = now.format("%Y-%m-%dT%H:%M:%S").to_string();

        // Find due tasks: status = 'scheduled', next_run is non-empty and <= now
        let due_tasks: Vec<(i64, String, String, String)> = match with_db(&state, |db| {
            let mut stmt = db
                .prepare(
                    "SELECT id, name, description, schedule FROM tasks \
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

        for (task_id, task_name, description, schedule) in due_tasks {
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
                &format!("Task started: {}", task_name),
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

            // Save user message
            let _ = with_db(&state, |db| {
                db.execute(
                    "INSERT INTO messages (session_id, role, content) VALUES (?1, 'user', ?2)",
                    params![session_id, prompt],
                )
                .map_err(|e| e.to_string())
            });

            let history = vec![("user".to_string(), prompt.clone())];

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
                chat_with_minimax_anthropic(&app, &state, &session_id, &model, &history, None).await
            } else {
                chat_with_openai(&app, &state, &session_id, &model, &history, None).await
            };

            let (status, reply_preview) = match &result {
                Ok(reply) => {
                    let _ = with_db(&state, |db| {
                        db.execute(
                            "INSERT INTO messages (session_id, role, content) VALUES (?1, 'assistant', ?2)",
                            params![session_id, reply],
                        )
                        .map_err(|e| e.to_string())
                    });
                    ("completed", truncate_chars(reply, 200))
                }
                Err(e) => ("scheduled", format!("Error: {}", truncate_chars(e, 180))),
            };

            // Calculate next run
            let new_next_run = compute_next_run(&schedule, now).unwrap_or_default();

            // If task has a recurring schedule, reset to 'scheduled' with new next_run.
            // If no schedule (one-shot), mark as completed.
            let final_status = if !schedule.trim().is_empty() && !new_next_run.is_empty() {
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
}
