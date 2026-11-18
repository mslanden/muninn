//! Tauri command handlers, grouped by feature area. Every `#[tauri::command]` is
//! re-exported flat so `lib.rs` registration (`commands::foo`) is unaffected.

mod sessions;
mod toolconfig;
mod feed;
mod tasks;
mod skills;
mod connections;
mod settings;
mod maps;
mod memory;
mod gallery;
mod workspace;
mod terminal;
mod chat;
mod projects;
mod voice;
mod mcp;
mod safety;
mod mobile;

pub use sessions::*;
pub use toolconfig::*;
pub use feed::*;
pub use tasks::*;
pub use skills::*;
pub use connections::*;
pub use settings::*;
pub use maps::*;
pub use memory::*;
pub use gallery::*;
pub use workspace::*;
pub use terminal::*;
pub use chat::*;
pub use projects::*;
pub use voice::*;
pub use mcp::*;
pub use safety::*;
pub use mobile::*;
