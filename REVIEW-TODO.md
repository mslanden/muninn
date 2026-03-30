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

## Medium Priority
- [ ] Mutex panics in `audio.rs` - `.unwrap()` on mutex locks should use `if let Ok()`
- [ ] No WAL mode for SQLite - add `PRAGMA journal_mode = WAL`
- [ ] Redundant field access `main.js:894` - `feed.id ?? feed.id` is a no-op
- [ ] `cachedContentFiles` set but never read (`main.js:1920`)
- [ ] `threadOptions` declared but never used (`main.js:41`)
- [ ] Missing error handling on ~15 frontend `invoke()` calls
- [ ] CSP disabled in `tauri.conf.json` (`"csp": null`)
- [ ] Stale context bar after thread switch - `contextTokens` never reset
- [ ] Orphaned CSS: `.layout-bar`, `.layout-pill`, `.layout-divider`, `.layout-add`

## ROADMAP Gaps
- [ ] Slash command picker UI (autocomplete dropdown)
- [ ] Feed: per-item remove button
- [ ] Tasks: `chain_next`, `conditions`, `run_count`, `triggered_by` schema + logic
- [ ] Tasks: manual status change UI
- [ ] Session delete button in history UI
