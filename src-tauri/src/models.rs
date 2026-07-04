use portable_pty;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

/// A single turn in a replayable chat history. `blocks` is an Anthropic-style content array —
/// each block is `{type:"text"|"tool_use"|"tool_result", ...}`. This is the canonical in-memory
/// form; provider adapters convert it to OpenAI's tool_calls shape when needed.
#[derive(Clone, Debug)]
pub struct HistoryMessage {
    pub role: String,
    pub blocks: Vec<Value>,
}

impl HistoryMessage {
    pub fn text(role: &str, text: &str) -> Self {
        Self {
            role: role.to_string(),
            blocks: vec![json!({"type": "text", "text": text})],
        }
    }

    /// Concatenate all text blocks for display / approximate sizing.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
        out
    }

    /// Approximate character count, used for history trimming. Image blocks carry a large
    /// base64 payload whose char length wildly overstates their real token cost (a few
    /// hundred to ~15k), so they are counted at a fixed budget instead — otherwise a single
    /// screenshot would blow the trim budget or be evicted on the very next turn.
    pub fn char_len(&self) -> usize {
        let mut total = 0;
        for block in &self.blocks {
            if block.get("type").and_then(Value::as_str) == Some("image") {
                total += 6_000; // ≈1.5k tokens — a rough, deliberate over-estimate
            } else {
                total += serde_json::to_string(block).map(|s| s.len()).unwrap_or(0);
            }
        }
        total
    }
}

/// A single entry in an agent's ephemeral to-do list. Written by the agent via the
/// `write_todos` tool; stored in-memory per session (not persisted across app restart).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    /// Short imperative phrase, e.g. "Read src/main.rs".
    pub content: String,
    /// One of: "pending" | "in_progress" | "completed".
    pub status: String,
    /// Present-participle form shown while the item is the active one
    /// (e.g. "Reading src/main.rs"). Optional — falls back to `content`.
    #[serde(default)]
    pub active_form: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleConfig {
    pub version: u32,
    pub start_date: String,
    pub end_date: Option<String>,
    pub days: HashMap<String, Option<DayConfig>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DayConfig {
    pub mode: String,
    pub times: Option<Vec<String>>,
    pub interval_mins: Option<u32>,
}

#[derive(Serialize)]
pub struct SessionRecord {
    pub id: String,
    pub created_at: String,
    pub project_path: String,
}

#[derive(Serialize)]
pub struct SessionListItem {
    pub id: String,
    pub created_at: String,
    pub last_message_at: String,
    pub message_count: i64,
    pub preview: String,
    pub project_path: String,
    pub pinned: bool,
    pub title: Option<String>,
}

#[derive(Serialize)]
pub struct MessageRecord {
    pub role: String,
    pub content: String,
    /// Structured content blocks (text/image/tool_use/…) so the UI can re-render images
    /// and other non-text content on reload, not just the plain `content` text.
    pub blocks: Vec<Value>,
    pub created_at: String,
}

/// Generated social-media copy for one platform (Studio tile).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformCopy {
    pub platform: String,
    pub variants: Vec<String>,
}

#[derive(Serialize)]
pub struct ToolLogRecord {
    pub kind: String,
    pub input: String,
    pub output: String,
    pub status: String,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub reply: String,
}

#[derive(Serialize)]
pub struct ProcessStartResponse {
    pub process_id: String,
}

#[derive(Serialize)]
pub struct SafetyCheck {
    pub dangerous: bool,
    pub reasons: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct FsResult {
    pub path: String,
    pub content: String,
}

#[derive(Serialize)]
pub struct PathResult {
    pub workspace_root: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub path: String,
    pub name: String,
    pub agents_md: String,
    pub created_at: String,
    pub last_opened_at: String,
}

#[derive(Serialize)]
pub struct ToolConfigRecord {
    pub tool_id: String,
    pub name: String,
    pub description: String,
    pub icon_name: String,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct FeedRecord {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub message: String,
    pub icon_name: String,
    pub is_read: bool,
    pub metadata: String,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct TaskRecord {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub schedule: String,
    pub schedule_config: String,
    pub next_run: String,
    pub status: String,
    pub completed_at: String,
    pub result: String,
    pub run_count: i64,
    pub triggered_by: String,
    pub chain_next: String,
    pub conditions: String,
    pub condition_status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct TaskRun {
    pub id: i64,
    pub task_id: i64,
    pub session_id: String,
    pub trigger_kind: String,
    pub triggered_by: i64,
    pub status: String,
    pub output: String,
    pub note: String,
    pub started_at: String,
    pub finished_at: String,
}

#[derive(Serialize)]
pub struct AgentRecord {
    pub agent_id: String,
    pub name: String,
    pub description: String,
    pub slash_command: String,
    pub system_prompt: String,
    pub max_iterations: i64,
    pub enabled: bool,
    pub allowed_tools_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct MemoryProfileRecord {
    pub user_name: String,
    pub user_about: String,
    pub profile_text: String,
    pub identity: String,
    pub personality: String,
    pub goals: String,
    pub preferences: String,
    pub current_state: String,
    pub patterns: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct EntityRecord {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub summary: String,
    pub aliases: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct RelationshipRecord {
    pub id: i64,
    pub from_entity_id: i64,
    pub to_entity_id: i64,
    pub from_name: String,
    pub to_name: String,
    pub label: String,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct EpisodeRecord {
    pub id: i64,
    pub description: String,
    pub timestamp: String,
    pub entity_names: Vec<String>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct MediaItemRecord {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: i64,
    pub modified_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentPathRecord {
    pub content_dir: String,
}

#[derive(Clone, Serialize)]
pub struct TerminalOutputEvent {
    pub process_id: String,
    pub chunk: String,
}

#[derive(Clone, Serialize)]
pub struct TerminalExitEvent {
    pub process_id: String,
    pub exit_code: Option<u32>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStartEvent {
    pub session_id: String,
    pub tool_name: String,
    pub arguments: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCompleteEvent {
    pub session_id: String,
    pub tool_name: String,
    pub output: String,
    pub status: String,
}

#[derive(Clone, Serialize)]
pub struct ChatTokenEvent {
    pub session_id: String,
    pub token: String,
}

#[derive(Clone, Serialize)]
pub struct ChatUsageEvent {
    pub session_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Serialize)]
pub struct ChatDoneEvent {
    pub session_id: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIterationEvent {
    pub session_id: String,
    pub agent_name: String,
    pub iteration: i64,
    pub max_iterations: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRequest {
    pub session_id: String,
    pub message: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub agent_id: Option<String>,
    /// Optional images shared with the turn (screenshots, etc.), sent to vision-capable
    /// models as Anthropic image content blocks.
    pub attachments: Option<Vec<ChatAttachment>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAttachment {
    /// MIME type, e.g. "image/png" or "image/jpeg".
    pub media_type: String,
    /// Base64-encoded image bytes (no `data:` URL prefix).
    pub data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionQueryRequest {
    pub limit: Option<usize>,
    pub query: Option<String>,
    pub project_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateRequest {
    pub project_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolToggleRequest {
    pub tool_id: String,
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedListRequest {
    pub limit: Option<usize>,
    pub unread_only: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedItemRequest {
    pub id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskCreateRequest {
    pub name: String,
    pub description: Option<String>,
    pub schedule: Option<String>,
    pub schedule_config: Option<String>,
    pub next_run: Option<String>,
    /// JSON array of task-id strings to fire when this task completes successfully.
    pub chain_next: Option<String>,
    /// Natural-language gate: when chain-triggered, only run if this holds for the input.
    pub conditions: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskUpdateRequest {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub schedule: Option<String>,
    pub schedule_config: Option<String>,
    pub next_run: Option<String>,
    /// JSON array of task-id strings to fire when this task completes successfully.
    pub chain_next: Option<String>,
    /// Natural-language gate: when chain-triggered, only run if this holds for the input.
    pub conditions: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusRequest {
    pub id: i64,
    pub status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskIdRequest {
    pub id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCreateRequest {
    pub name: String,
    pub description: Option<String>,
    pub slash_command: Option<String>,
    pub system_prompt: Option<String>,
    pub max_iterations: Option<i64>,
    pub enabled: Option<bool>,
    pub allowed_tools: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUpdateRequest {
    pub agent_id: String,
    pub name: String,
    pub description: Option<String>,
    pub slash_command: Option<String>,
    pub system_prompt: Option<String>,
    pub max_iterations: Option<i64>,
    pub enabled: Option<bool>,
    pub allowed_tools: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToggleRequest {
    pub agent_id: String,
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdRequest {
    pub agent_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUpdateRequest {
    pub settings: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryProfileUpdateRequest {
    pub user_name: Option<String>,
    pub user_about: Option<String>,
    pub profile_text: Option<String>,
    pub identity: Option<String>,
    pub personality: Option<String>,
    pub goals: Option<String>,
    pub preferences: Option<String>,
    pub current_state: Option<String>,
    pub patterns: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityUpsertRequest {
    pub id: Option<i64>,
    pub kind: String,
    pub name: String,
    pub summary: Option<String>,
    pub aliases: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipUpsertRequest {
    pub from_entity_id: i64,
    pub to_entity_id: i64,
    pub label: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipIdRequest {
    pub id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMessagesRequest {
    pub query: String,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityIdRequest {
    pub id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeCreateRequest {
    pub description: String,
    pub timestamp: Option<String>,
    pub entity_names: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeIdRequest {
    pub id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaDeleteRequest {
    pub kind: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentImportRequest {
    pub paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentReadRequest {
    pub path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentReadResponse {
    pub content: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStartRequest {
    pub session_id: String,
    pub command: String,
    pub cwd: Option<String>,
    pub approved_dangerous: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInputRequest {
    pub process_id: String,
    pub input: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStopRequest {
    pub process_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReadRequest {
    pub path: String,
    pub max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileWriteRequest {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchRequest {
    pub path: String,
    pub pattern: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListRequest {
    pub path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRootRequest {
    pub workspace_root: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeRequest {
    pub audio_base64: String,
}

#[derive(Serialize)]
pub struct TranscribeResponse {
    pub text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsRequest {
    pub text: String,
    pub provider: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsResponse {
    pub audio_base64: String,
    pub content_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAddRequest {
    pub path: String,
    pub name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPathRequest {
    pub path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInitRequest {
    pub path: String,
}

pub struct RunningProcess {
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiFeedRecord {
    pub id: i64,
    pub name: String,
    pub topics: Vec<String>,
    pub refresh_interval_mins: i64,
    pub schedule_config: String,
    pub last_refreshed_at: String,
    pub created_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiFeedItemRecord {
    pub id: i64,
    pub feed_id: i64,
    pub title: String,
    pub summary: String,
    pub url: String,
    pub source_name: String,
    pub image_url: String,
    pub is_read: bool,
    pub created_at: String,
    pub article: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiWebSearchRequest {
    pub query: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiWebSource {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiWebSearchResult {
    pub answer: String,
    pub sources: Vec<AiWebSource>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiFeedRefreshResult {
    pub items_added: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiFeedCreateRequest {
    pub name: String,
    pub topics: Vec<String>,
    pub refresh_interval_mins: Option<i64>,
    pub schedule_config: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiFeedUpdateRequest {
    pub id: i64,
    pub name: String,
    pub topics: Vec<String>,
    pub refresh_interval_mins: Option<i64>,
    pub schedule_config: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiFeedIdRequest {
    pub id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiFeedItemIdRequest {
    pub id: i64,
}
