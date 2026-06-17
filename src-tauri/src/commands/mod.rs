//! Tauri command handlers, grouped by feature area. Every `#[tauri::command]` is
//! re-exported flat so `lib.rs` registration (`commands::foo`) is unaffected.

mod sessions;
mod toolconfig;
mod feed;
mod tasks;
mod agents;
mod settings;
mod memory;
mod gallery;
mod workspace;
mod terminal;
mod chat;
mod projects;
mod voice;
mod ai_feeds;
mod mcp;

pub use sessions::*;
pub use toolconfig::*;
pub use feed::*;
pub use tasks::*;
pub use agents::*;
pub use settings::*;
pub use memory::*;
pub use gallery::*;
pub use workspace::*;
pub use terminal::*;
pub use chat::*;
pub use projects::*;
pub use voice::*;
pub use ai_feeds::*;
pub use mcp::*;
