# Muninn Roadmap

Last updated: 2026-07-23. The original V1 parity plan (recreating the Verus app workflows in Tauri + Rust with no daemon) shipped in full and this file no longer tracks it — see git history for the old document.

## Shipped

Everything below is live on `main`:

- **Core app** — tile dashboard (chat, tasks, skills, memory, content, activity, settings), pop-out windows, persisted layouts, tray-resident lifecycle
- **Chat & providers** — streaming, stop, slash commands, session history with pin/rename/delete/search/export, provider/model selector with fetch-models + test-connection (OpenAI, MiniMax/Anthropic-compatible, Meta, custom endpoints), MiniMax-M3 native vision (paste/drag/attach screenshots)
- **Agent tooling** — workspace-scoped file/search/shell, write/edit/append, git/code/http/test, web search/fetch, image gen, Mapbox, screen capture/open app, settings self-config (schema allowlist + ask_user gating), agent self-management CRUD + `call_agent` delegation with sub-session isolation
- **Tasks** — natural-language schedules, chaining with condition gates, per-run history, run-now/pause/fail states, once-mode, per-task workspace + agent targeting, concurrent scheduler
- **Safety** — workspace scoping, dangerous-command detection with approval prompts, LLM auto-verify with deferred-command queue, secret redaction, `/goal` auto-continue loop
- **Media Studio** — shipped 2026-06/07, removed 2026-11-17: the video editor (ffmpeg engine, 5 agent tools, face-scan sidecar) and then the whole Studio tile (social copy, carousels, writing assist, HTML docs). Replaced by the `create_pdf` agent tool; everything else is done in chat.
- **Transcription** — voice input via on-device Swift sidecar (SpeechAnalyzer on macOS 26, SFSpeechRecognizer fallback), cloud whisper (Groq/OpenAI) fallback
- **Mobile** — opt-in in-process HTTP/WS server, QR pairing, embedded phone UI, mobile image support
- **Memory** — profile/entities/episodes with embedding retrieval + backfill

## Next up (candidates)

Platform:

- [ ] Native iOS companion app with push notifications (paid Apple Developer account + paired devices ready; would replace/augment the embedded mobile web UI)
- [ ] Frontend test suite (none exists today; `npm run lint` / `prettier` are in place)

## Non-goals (decided)

- **Video editing** — removed 2026-11-17; not part of what Muninn is for.
- **AI news feeds** — removed 2026-11-18: 2,823 generated items, 25 ever opened, and no refresh for two months without anyone noticing. A scheduled task (optionally producing a PDF digest) covers the need.
- **Web tile** — removed 2026-11-18: a separate AI search box that duplicated chat's web_search/web_fetch and was pinned to MiniMax (out of credit) regardless of the configured model.
- **Daemon/sidecar backend** — all state and logic stay in the single Tauri Rust process.

## Housekeeping

- Update this file when an item ships or a new decision lands; keep "Shipped" as a one-line-per-feature summary, not a changelog.
