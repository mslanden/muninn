//! MCP server status + lifecycle commands for the settings UI.

use serde::Serialize;
use tauri::State;

use crate::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub name: String,
    pub connected: bool,
    pub tool_count: usize,
    pub error: Option<String>,
}

/// Connect any configured-but-unconnected servers and report per-server status.
#[tauri::command]
pub async fn get_mcp_status(state: State<'_, AppState>) -> Result<Vec<McpServerStatus>, String> {
    // Ensure connections exist before reporting, so the settings panel shows
    // live state rather than "not connected yet" for lazily-started servers.
    state.mcp.connected_tools(state.inner()).await;
    let status = state.mcp.status(state.inner()).await;
    Ok(status
        .into_iter()
        .map(|(name, connected, tool_count, error)| McpServerStatus { name, connected, tool_count, error })
        .collect())
}

/// Kill all MCP connections and reconnect from the current `mcp/servers` config.
#[tauri::command]
pub async fn reload_mcp_servers(state: State<'_, AppState>) -> Result<Vec<McpServerStatus>, String> {
    state.mcp.reset().await;
    get_mcp_status(state).await
}
