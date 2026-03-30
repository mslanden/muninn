# Review TODO (remaining items)

## High Priority
- [ ] Missing CSS variables (`--gold`, `--fg`) - causes invisible context bar fill and content preview issues
- [ ] Potential panic at startup - `unwrap()` on `app.default_window_icon()` in `lib.rs:83`
- [ ] Memory leaks from event-driven re-renders - `memory-updated`, `tasks-changed`, `content-changed` events accumulate duplicate listeners
- [ ] Leaflet maps never `.remove()`d when tiles close
- [ ] Content preview escape key handler leaks when closed by clicking
- [ ] Markdown rendering: `<br>` tags inserted inside `<pre><code>` blocks
- [ ] Markdown rendering: greedy list regex merges separate lists

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
