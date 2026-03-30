# Spinosaur

Spinosaur is a macOS-first Tauri + Rust local coding assistant MVP.

## Included MVP features

- No daemon backend (all backend logic runs in the Tauri Rust process)
- PTY terminal execution with streamed output to UI
- File tools scoped to a workspace root (`read`, `write`, `list`, `search`)
- Agent chat wired to OpenAI Chat Completions with function tools
- Dangerous-command checks and explicit user approval in the UI
- SQLite persistence for sessions, messages, and tool logs

## Prerequisites

- Rust toolchain (`rustup`, `cargo`)
- Node.js + npm
- Xcode Command Line Tools (`xcode-select --install`)
- `OPENAI_API_KEY` for OpenAI provider
- `MINIMAX_API_KEY` (or `ANTHROPIC_API_KEY`) for MiniMax Anthropic-compatible provider

## Run

```bash
cd /Users/marcelinolanden/Documents/myProjects/Verus/spinosaur
npm install
npm run tauri dev
```

## MiniMax Provider

Use chat provider `MiniMax (Anthropic Compatible)` in the UI and set:

```bash
export MINIMAX_API_KEY=your_key_here
# optional override, default is https://api.minimax.io/anthropic
export MINIMAX_ANTHROPIC_BASE_URL=https://api.minimax.io/anthropic
```

## Build

```bash
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
```

## Notes

- Workspace access is restricted to the selected workspace root.
- Commands matching dangerous patterns are blocked unless approved in UI.
- Agent shell tool calls still enforce dangerous-command blocking.
