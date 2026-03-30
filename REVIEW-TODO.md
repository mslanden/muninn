# Review TODO (remaining items)

## High Priority (all done)
- [x] Missing CSS variables (`--gold`, `--fg`)
- [x] Potential panic at startup - `unwrap()` on `app.default_window_icon()`
- [x] Memory leaks from event-driven re-renders (verified safe - innerHTML wipes old listeners)
- [x] Leaflet maps never `.remove()`d when tiles close
- [x] Content preview escape key handler leaks when closed by clicking
- [x] Markdown rendering: `<br>` tags inserted inside `<pre><code>` blocks
- [x] Markdown rendering: greedy list regex merges separate lists

## Previously Fixed
- [x] XSS vulnerabilities - escapeHtml applied across 25+ innerHTML locations
- [x] /stop command to cancel in-flight chat generation
- [x] Progress reminders every 5 tool-call rounds

## Medium Priority (all done)
- [x] Mutex panics in `audio.rs`
- [x] WAL mode for SQLite
- [x] Redundant field access `feed.id ?? feed.id`
- [x] Dead code: `cachedContentFiles`, `threadOptions`
- [x] Error handling on frontend `invoke()` calls
- [x] CSP in `tauri.conf.json`
- [x] Stale context bar after thread switch
- [x] Orphaned CSS removed

## ROADMAP Gaps (all done)
- [x] Slash command picker UI (autocomplete dropdown)
- [x] Feed: per-item remove button
- [x] Tasks: schema columns (`completed_at`, `result`, `run_count`, `triggered_by`, `chain_next`, `conditions`, `condition_status`)
- [x] Tasks: manual status change UI
- [x] Session delete button in history UI
