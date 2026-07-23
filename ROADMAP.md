# Muninn Roadmap

Last updated: 2026-07-23. The original V1 parity plan (recreating the Verus app workflows in Tauri + Rust with no daemon) shipped in full and this file no longer tracks it — see git history for the old document.

## Shipped

Everything below is live on `main`:

- **Core app** — tile dashboard (chat, feeds, tasks, agents, memory, content, studio, logs, settings), pop-out windows, persisted layouts, tray-resident lifecycle
- **Chat & providers** — streaming, stop, slash commands, session history with pin/rename/delete/search/export, provider/model selector with fetch-models + test-connection (OpenAI, MiniMax/Anthropic-compatible, Meta, custom endpoints), MiniMax-M3 native vision (paste/drag/attach screenshots)
- **Agent tooling** — workspace-scoped file/search/shell, write/edit/append, git/code/http/test, web search/fetch, image gen, Mapbox, screen capture/open app, settings self-config (schema allowlist + ask_user gating), agent self-management CRUD + `call_agent` delegation with sub-session isolation
- **Tasks** — natural-language schedules, chaining with condition gates, per-run history, run-now/pause/fail states, once-mode, per-task workspace + agent targeting, concurrent scheduler
- **Safety** — workspace scoping, dangerous-command detection with approval prompts, LLM auto-verify with deferred-command queue, secret redaction, `/goal` auto-continue loop
- **Media Studio (video 10x, all 3 tiers)** — EDL engine with captions (ASS, karaoke), filler-word removal, audio polish (loudnorm/denoise/fades), hardware encode, preview + interactive timeline/transcript, plan autosave, multi-source b-roll/stills, smart reframe (Vision face tracking), auto-shorts, multi-clip compile
- **Transcription** — on-device Swift sidecar (SpeechAnalyzer on macOS 26, SFSpeechRecognizer fallback), cloud whisper (Groq/OpenAI) fallback
- **Mobile** — opt-in in-process HTTP/WS server, QR pairing, embedded phone UI, mobile image support
- **Memory** — profile/entities/episodes with embedding retrieval + backfill

## Next up (candidates)

Video (carry-over ideas from the 10x roadmap):

- [ ] MCP/social posting of rendered shorts (publish from the Content library)
- [ ] Watched-folder auto-clip task template (drop a recording → shorts appear)
- [ ] ASS caption style editor in the studio UI
- [ ] `duration_hint` control in the studio auto-shorts row
- [ ] Vertical (y-axis) smart reframe
- [ ] Multi-clip compile v2: per-clip audio handling and resolution normalization (v1 derives both from clip 0)

Platform:

- [ ] Native iOS companion app with push notifications (paid Apple Developer account + paired devices ready; would replace/augment the embedded mobile web UI)
- [ ] Frontend test suite (none exists today; `npm run lint` / `prettier` are in place)

## Non-goals (decided)

- **Remotion** for video compositing — rejected 2026-07-08; React + headless-Chrome is the wrong fit for a framework-free Tauri app editing real footage. ffmpeg/libass covers captions; animated title cards can reuse the `renderSlideToPng` HTML→PNG overlay trick.
- **Daemon/sidecar backend** — all state and logic stay in the single Tauri Rust process.

## Housekeeping

- Update this file when an item ships or a new decision lands; keep "Shipped" as a one-line-per-feature summary, not a changelog.
