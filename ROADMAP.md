# Spinosaur V1 Roadmap

## Goal
Recreate core Verus app workflows in Tauri + Rust with **no daemon**.
All state and logic run in the Tauri process (`src-tauri`), exposed via `#[tauri::command]`.

## V1 Scope (Parity Targets)

### 1) Chat Surface (Must Have)
- [ ] Chat transcript (user/assistant bubbles)
- [ ] Streaming response indicator
- [ ] Provider + model selector
- [ ] Slash command picker (at least basic command dispatch)
- [ ] Stop generation
- [ ] Tool activity panel (already present, keep and improve)
- [ ] Session history list + reopen + delete

### 2) Navigation + Panels (Must Have)
- [ ] Left nav rail with sections:
  - [ ] Feed
  - [ ] Tasks
  - [ ] Tools
  - [ ] Agents
  - [ ] Memory
  - [ ] Gallery
  - [ ] Settings
- [ ] Slide-over content panel for selected section

### 3) Feed (Must Have)
- [ ] List feed items
- [ ] Unread count badge
- [ ] Mark item read
- [ ] Mark all read
- [ ] Remove item
- [ ] Clear all

### 4) Tasks (Must Have)
- [ ] Create/edit/delete task
- [ ] Status tabs: scheduled/running/completed
- [ ] Manual status changes
- [ ] Store `schedule`, `next_run`, `run_count`, `conditions`, `chain_next`

### 5) Tools (Must Have)
- [ ] List tools with metadata
- [ ] Enable/disable tool toggle
- [ ] Tool toggle enforced by chat tool-calling path

### 6) Agents (Must Have)
- [ ] CRUD custom agents
- [ ] Enabled toggle
- [ ] Slash command
- [ ] System prompt + max iterations
- [ ] Allowed tools selection

### 7) Memory (Must Have)
- [ ] Profile card (user profile text)
- [ ] Entities tab (list + delete)
- [ ] Timeline tab (episodes list)
- [ ] History tab (search sessions, inspect session messages)

### 8) Gallery (Should Have for v1)
- [ ] Images tab from local gallery folder
- [ ] Audio tab from local audio folder
- [ ] Open folder action
- [ ] Delete media item

### 9) Settings (Must Have, Reduced)
- [ ] Profile: name/about
- [ ] LLM provider
- [ ] LLM model
- [ ] API key fields (OpenAI + MiniMax first, others can be added incrementally)
- [ ] Web search key field
- [ ] Image generation provider/model field

## Out of Scope for Initial V1
- Voice mode orb + full speech pipeline parity
- macOS launchd lifecycle management
- Daemon IPC protocol compatibility
- Full external service parity for every provider on day one

## Data Model Plan (`src-tauri` SQLite)

Existing tables:
- `sessions`
- `messages`
- `tool_logs`
- `saved_workspaces`

Add tables:
- `feed_items(id, type, title, message, icon_name, is_read, metadata, created_at)`
- `tasks(id, name, description, schedule, next_run, status, completed_at, result, chain_next, conditions, condition_status, run_count, triggered_by, created_at, updated_at)`
- `tools(tool_id, name, description, icon_name, enabled, updated_at)`
- `agents(agent_id, name, description, icon_name, slash_command, system_prompt, max_iterations, enabled, allowed_tools_json, created_at, updated_at)`
- `entities(id, type, name, summary, metadata, created_at, updated_at)`
- `episodes(id, description, entity_names_json, timestamp, created_at)`
- `profiles(id, user_name, user_about, profile_text, updated_at)`
- `app_config(key, value, updated_at)` (or a json config blob table)

## Tauri Command Plan

Existing command base already available:
- `chat`, `create_session`, `get_messages`, `get_tool_logs`
- workspace/file/terminal command set

Add command groups:
- `feed_*`: list/add/mark_read/mark_all_read/remove/clear
- `task_*`: list/add/update/remove/set_status/complete
- `tool_*`: list/set_enabled
- `agent_*`: list/add/update/remove/set_enabled
- `memory_*`: get_profile/update_profile/list_entities/delete_entity/list_episodes
- `history_*`: get_sessions/get_session_messages/search_messages/delete_session
- `settings_*`: get_config/set_config/get_providers/get_models
- `gallery_*`: list_images/list_audio/delete_image/delete_audio/open_folder

## Delivery Order

1. Foundation + UI shell
- Nav rail and panel shell in web UI.
- Shared state store on frontend.

2. Vertical Slice A (first execution target)
- Chat + sessions + tool logs fully polished.
- Add session list panel (history) and delete/search.

3. Vertical Slice B
- Tools + Feed.

4. Vertical Slice C
- Tasks + Agents.

5. Vertical Slice D
- Memory + Settings + Gallery.

6. Hardening
- Migration handling
- Better error surfaces
- Test pass (`npm run build`, `cargo check`)

## Immediate Next Build Step
- Implement nav rail + section panel scaffold in `spinosaur` frontend, then ship Vertical Slice A (Chat + session history) on top of that shell.
