# Muninn

Muninn is a macOS-first personal AI assistant built with Tauri 2 + Rust. It runs as a single tray-resident process — no daemon, no separate backend — with a vanilla-JS tile dashboard as the frontend.

## Features

- **Tile dashboard** — chat, feeds, tasks, agents, memory, content library, media studio, logs, and settings panels arranged in a grid, with pop-out windows and persisted layouts
- **Agent chat** — OpenAI-compatible and Anthropic-compatible providers (OpenAI, MiniMax, Meta, custom endpoints) with a shared in-Rust tool loop: workspace-scoped file/search/shell tools, web search & fetch, git/code/http tools, image generation, Mapbox directions/places, screen capture, and video editing
- **Custom agents** — user-defined agents with their own system prompts and tool allowlists, agent self-management (CRUD via tools), and agent-to-agent delegation with sub-session isolation
- **Scheduled tasks** — natural-language schedules (`every 30s`, `daily at 09:00`, `every monday at 10:00`), task chaining with condition gates, per-run history, and per-task workspace/agent targeting
- **AI feeds** — scheduled LLM jobs that generate feed items (with optional images) on their own cadence
- **Media Studio** — local ffmpeg video engine: non-destructive edit plans (cuts, captions, filters, overlays, aspect crop, speed), multi-source b-roll inserts, subject-aware smart reframe (Vision face detection), auto-generated 9:16 shorts, and multi-clip compilations
- **On-device transcription** — Swift sidecar using Apple SpeechAnalyzer (macOS 26) or SFSpeechRecognizer, with cloud whisper (Groq/OpenAI) fallback
- **Memory** — persistent memories with embedding-based retrieval injected into chat
- **Voice** — microphone recording + transcription for voice input
- **Mobile companion** — opt-in in-process HTTP/WS server with QR pairing and an embedded phone UI
- **Safety boundaries** — workspace-scoped file access, dangerous-command detection with user approval prompts, and secret redaction for anything crossing into the webview

## Prerequisites

- Rust toolchain (`rustup`, `cargo`)
- Node.js + npm
- Xcode Command Line Tools (`xcode-select --install`) — `swiftc` is needed to build the transcriber/framescan sidecars
- `ffmpeg` (Homebrew) for Media Studio and video tools
- API keys, via env vars or the Settings UI:
  - `OPENAI_API_KEY` for the OpenAI provider
  - `MINIMAX_API_KEY` (or `ANTHROPIC_API_KEY`) for the MiniMax Anthropic-compatible provider; optional `MINIMAX_ANTHROPIC_BASE_URL` (default `https://api.minimax.io/anthropic`)
  - Optional: Groq key (faster cloud transcription), Mapbox token (maps)

Settings saved in the app take precedence over env vars.

## Run (dev)

```bash
cd Muninn
npm install
npm run tauri dev
```

`npm run dev` alone runs the Vite frontend without the Rust host (useful for pure UI work; `invoke` calls will error).

## Build

```bash
npm run build
npm run tauri build          # production .app bundle
```

For a debug bundle with working voice features (mic + speech-recognition entitlements):

```bash
npm run tauri build -- --debug
npm run sign-dev
npm run start
```

## Checks & tests

```bash
cargo check --manifest-path src-tauri/Cargo.toml   # Rust type-check
cargo test --manifest-path src-tauri/Cargo.toml --lib
npm run lint
npm run format:check
```

## Notes

- The app is tray-resident: closing the window hides it; quit from the tray menu.
- Data lives in SQLite at `~/Library/Application Support/com.marcelinolanden.muninn/muninn.db`.
- Workspace access is restricted to the selected workspace root; agent shell commands go through dangerous-command checks with in-UI approval.
- See [CLAUDE.md](CLAUDE.md) for architecture details and contributor conventions.
