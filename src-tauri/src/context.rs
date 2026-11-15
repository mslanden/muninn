//! Context management: keeping what is sent to the model small, relevant and cache-friendly.
//!
//! Three mechanisms, cheapest first:
//!
//! 1. **Stale tool images** (`prune_stale_images`) — screenshots and page captures attached to
//!    tool results are resent on every later round of a turn. Only the most recent few are
//!    worth their payload, so older ones are replaced by a one-line note.
//! 2. **Old tool output** (`clear_old_tool_results`) — when a request nears the budget, the
//!    bodies of old tool results are cleared (a short head is kept), leaving every
//!    tool_use/tool_result pair intact. No LLM call.
//! 3. **Compaction** (`prepare_history`) — when a session's stored history nears the budget,
//!    the older turns are summarized once by the model; the summary is stored on the session
//!    and later turns load only the messages after it. Nothing is deleted: the chat UI still
//!    shows every message.
//!
//! Everything here is written to keep the request **prefix stable** between rounds and turns,
//! because provider prompt caches (Anthropic `cache_control`, Qwen/OpenAI implicit caching)
//! match on the prefix: trimming is done in batches with hysteresis rather than a little on
//! every round, and compaction replaces the previous sliding 200-message window, which
//! shifted the first message — and so invalidated the whole cache — on every turn.

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::db::{add_feed_item, get_app_setting_value, sanitize_tool_pairs};
use crate::models::HistoryMessage;
use crate::AppState;

/// Default conversation budget in tokens (history + in-turn tool rounds).
pub const DEFAULT_BUDGET_TOKENS: u64 = 300_000;
/// Fixed size an image block counts as, in characters (≈1.5k tokens). Its base64 length
/// overstates its token cost by orders of magnitude.
const IMAGE_CHARS: usize = 6_000;

/// Tool-image messages kept with their images; older ones are pruned once there are
/// `PRUNE_IMAGES_AT` of them (batched so the cached prefix isn't rewritten on every capture).
const KEEP_IMAGE_MESSAGES: usize = 2;
const PRUNE_IMAGES_AT: usize = 4;

/// Most recent tool results never cleared, and the size below which one isn't worth clearing.
const KEEP_RECENT_TOOL_RESULTS: usize = 6;
const MIN_CLEARABLE_CHARS: usize = 800;
const CLEARED_HEAD_CHARS: usize = 300;
const CLEARED_MARK: &str = "[older tool output cleared to save context";

/// Newest stored rows considered for one turn; older unsummarized rows are out of reach.
const MAX_HISTORY_ROWS: usize = 2000;
/// Cap on the transcript handed to the summarizer.
const MAX_TRANSCRIPT_CHARS: usize = 400_000;

/// First text of the user message that carries tool images on the OpenAI wire format (tool
/// messages there are text-only, so images follow in a user message). Also how such a
/// message is recognised later for pruning.
pub const TOOL_IMAGE_NOTE: &str = "[Image output from the tool call(s) above]";

const SUMMARY_HEADER: &str = "[Summary of the earlier part of this conversation. The original messages are no longer in your context; treat this as what happened so far.]";
const SUMMARY_FOOTER: &str = "[End of summary. The conversation continues below.]";

/// The conversation budget in characters (~4 per token), from `llm/context_budget_tokens`.
pub fn budget_chars(state: &AppState) -> usize {
    let tokens = get_app_setting_value(state, "llm/context_budget_tokens")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_BUDGET_TOKENS)
        .clamp(16_000, 2_000_000);
    (tokens as usize) * 4
}

fn auto_compact_enabled(state: &AppState) -> bool {
    get_app_setting_value(state, "llm/auto_compact").ok().flatten().as_deref() != Some("0")
}

// ---------------------------------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------------------------------

fn is_image_block(v: &Value) -> bool {
    matches!(v.get("type").and_then(Value::as_str), Some("image") | Some("image_url"))
}

/// Approximate size of a JSON value as the model is billed for it: like its serialized
/// length, except image blocks count as a fixed `IMAGE_CHARS`.
pub fn value_chars(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len() + 2,
        Value::Array(items) => items.iter().map(value_chars).sum::<usize>() + 2,
        Value::Object(map) => {
            if is_image_block(v) {
                return IMAGE_CHARS;
            }
            map.iter().map(|(k, val)| k.len() + 4 + value_chars(val)).sum::<usize>() + 2
        }
        other => other.to_string().len(),
    }
}

pub fn messages_chars(messages: &[Value]) -> usize {
    messages.iter().map(value_chars).sum()
}

// ---------------------------------------------------------------------------------------
// In-turn: images
// ---------------------------------------------------------------------------------------

/// The user message that carries a tool round's images on the OpenAI wire format.
pub fn tool_image_message_openai(images: &[(String, String)]) -> Value {
    let mut parts = vec![json!({"type": "text", "text": TOOL_IMAGE_NOTE})];
    for (media_type, data) in images {
        parts.push(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{};base64,{}", media_type, data)}
        }));
    }
    json!({"role": "user", "content": parts})
}

fn content_parts(msg: &Value) -> Option<&Vec<Value>> {
    msg.get("content").and_then(Value::as_array)
}

/// Whether `msg` is a tool-result turn that carries images: an Anthropic user turn with both
/// `tool_result` and image blocks, or the OpenAI follow-up message built above. Images the
/// user attached themselves are never treated as stale.
fn is_tool_image_message(msg: &Value) -> bool {
    if msg.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(parts) = content_parts(msg) else { return false };
    if !parts.iter().any(is_image_block) {
        return false;
    }
    let has_tool_result = parts
        .iter()
        .any(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"));
    let is_openai_carrier = parts
        .first()
        .and_then(|p| p.get("text"))
        .and_then(Value::as_str)
        .map_or(false, |t| t.starts_with(TOOL_IMAGE_NOTE));
    has_tool_result || is_openai_carrier
}

fn replace_images(msg: &mut Value, note: &str) -> usize {
    let Some(parts) = msg.get_mut("content").and_then(Value::as_array_mut) else { return 0 };
    let mut replaced = 0;
    for part in parts.iter_mut() {
        if is_image_block(part) {
            *part = json!({"type": "text", "text": note});
            replaced += 1;
        }
    }
    replaced
}

/// Once `prune_at` tool-result turns carry images, strip the images from all but the newest
/// `keep` of them. Returns how many images were removed.
pub fn prune_stale_images(messages: &mut [Value], keep: usize, prune_at: usize) -> usize {
    let carriers: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_tool_image_message(m))
        .map(|(i, _)| i)
        .collect();
    if carriers.len() < prune_at.max(keep + 1) {
        return 0;
    }
    let mut removed = 0;
    for &i in &carriers[..carriers.len() - keep] {
        removed += replace_images(
            &mut messages[i],
            "[image removed to save context — capture it again if you need to see it]",
        );
    }
    removed
}

pub fn contains_images(messages: &[Value]) -> bool {
    messages
        .iter()
        .any(|m| content_parts(m).map_or(false, |parts| parts.iter().any(is_image_block)))
}

/// Replace every image (user-attached ones included) with a note. Used when the model or
/// endpoint turns out not to accept image input.
pub fn strip_all_images(messages: &mut [Value]) -> usize {
    messages
        .iter_mut()
        .map(|m| replace_images(m, "[image omitted — the current model does not accept image input]"))
        .sum()
}

// ---------------------------------------------------------------------------------------
// In-turn: old tool output
// ---------------------------------------------------------------------------------------

fn cleared_text(old: &str) -> Option<String> {
    if old.len() < MIN_CLEARABLE_CHARS || old.contains(CLEARED_MARK) {
        return None;
    }
    let head: String = old.chars().take(CLEARED_HEAD_CHARS).collect();
    Some(format!(
        "{}…\n{} — it was {} characters. Run the tool again if you still need it.]",
        head,
        CLEARED_MARK,
        old.len()
    ))
}

/// Clear tool output in one message; returns characters saved.
fn clear_tool_output(msg: &mut Value) -> usize {
    let mut saved = 0;
    let mut clear = |slot: &mut Value| {
        if let Some(new) = slot.as_str().and_then(cleared_text) {
            saved += slot.as_str().map(str::len).unwrap_or(0).saturating_sub(new.len());
            *slot = Value::String(new);
        }
    };
    match msg.get("role").and_then(Value::as_str) {
        // OpenAI: one `tool` message per result.
        Some("tool") => {
            if let Some(content) = msg.get_mut("content") {
                clear(content);
            }
        }
        // Anthropic: `tool_result` blocks inside a user turn.
        Some("user") => {
            if let Some(parts) = msg.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts.iter_mut() {
                    if part.get("type").and_then(Value::as_str) == Some("tool_result") {
                        if let Some(content) = part.get_mut("content") {
                            clear(content);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    saved
}

fn is_tool_result_message(msg: &Value) -> bool {
    match msg.get("role").and_then(Value::as_str) {
        Some("tool") => true,
        Some("user") => content_parts(msg).map_or(false, |parts| {
            parts.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"))
        }),
        _ => false,
    }
}

/// While the conversation is larger than `target_chars`, clear the bodies of the oldest tool
/// results (never the newest `keep_recent`). Pairing is untouched, so the provider still
/// accepts the history. Returns how many messages were cleared.
pub fn clear_old_tool_results(messages: &mut [Value], keep_recent: usize, target_chars: usize) -> usize {
    let mut total = messages_chars(messages);
    if total <= target_chars {
        return 0;
    }
    let carriers: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_tool_result_message(m))
        .map(|(i, _)| i)
        .collect();
    let clearable = carriers.len().saturating_sub(keep_recent);
    let mut cleared = 0;
    for &i in &carriers[..clearable] {
        if total <= target_chars {
            break;
        }
        let saved = clear_tool_output(&mut messages[i]);
        if saved > 0 {
            total = total.saturating_sub(saved);
            cleared += 1;
        }
    }
    cleared
}

/// Per-round upkeep of the outgoing message list. Images are pruned in batches; tool output
/// is cleared only once the request passes 80% of the budget, and then down to 55%, so the
/// (cached) prefix is rewritten rarely rather than a little every round.
pub fn maintain(messages: &mut [Value], budget_chars: usize) -> (usize, usize) {
    let images = prune_stale_images(messages, KEEP_IMAGE_MESSAGES, PRUNE_IMAGES_AT);
    let cleared = if messages_chars(messages) > budget_chars / 10 * 8 {
        clear_old_tool_results(messages, KEEP_RECENT_TOOL_RESULTS, budget_chars / 100 * 55)
    } else {
        0
    };
    (images, cleared)
}

// ---------------------------------------------------------------------------------------
// Cross-turn: compaction
// ---------------------------------------------------------------------------------------

fn has_block(msg: &HistoryMessage, kind: &str) -> bool {
    msg.blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some(kind))
}

/// A message a kept tail may start on: something the user (or the goal loop) said, not a
/// tool-result turn — cutting inside a tool round would orphan its tool_use/tool_result pair.
fn is_turn_start(msg: &HistoryMessage) -> bool {
    msg.role == "user" && !msg.blocks.is_empty() && !has_block(msg, "tool_result")
}

/// Choose how many leading messages to summarize: the kept tail starts on a turn boundary and
/// is the longest one that fits `tail_chars` and `tail_rows`. When even the last turn alone is
/// too big, the tail is just that turn. Returns 0 when there is nothing to summarize.
pub fn choose_cut(messages: &[HistoryMessage], tail_chars: usize, tail_rows: usize) -> usize {
    let mut best: Option<usize> = None;
    let mut last_boundary: Option<usize> = None;
    let mut size = 0usize;
    for i in (1..messages.len()).rev() {
        size += messages[i].char_len();
        if !is_turn_start(&messages[i]) {
            continue;
        }
        if last_boundary.is_none() {
            last_boundary = Some(i);
        }
        if size <= tail_chars && messages.len() - i <= tail_rows {
            best = Some(i);
        } else {
            break;
        }
    }
    best.or(last_boundary).unwrap_or(0)
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{}… [{} more characters]", head, text.chars().count() - max)
}

/// Render messages as a plain transcript for the summarizer. Tool calls and results are
/// abbreviated — the summary needs what was done and found, not full file dumps.
pub fn render_transcript(messages: &[HistoryMessage]) -> String {
    let mut out = String::new();
    for msg in messages {
        let who = if msg.role == "assistant" { "Assistant" } else { "User" };
        for block in &msg.blocks {
            let line = match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    if text.is_empty() {
                        continue;
                    }
                    format!("{}: {}", who, clip(text, 4000))
                }
                Some("tool_use") => format!(
                    "Assistant called tool {}({})",
                    block.get("name").and_then(Value::as_str).unwrap_or("?"),
                    clip(&block.get("input").map(|v| v.to_string()).unwrap_or_default(), 300)
                ),
                Some("tool_result") => {
                    let content = match block.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    format!("Tool result: {}", clip(content.trim(), 600))
                }
                Some("image") => format!("{}: [image]", who),
                _ => continue,
            };
            out.push_str(&line);
            out.push_str("\n\n");
        }
    }
    let total = out.chars().count();
    if total > MAX_TRANSCRIPT_CHARS {
        let head_n = MAX_TRANSCRIPT_CHARS / 5;
        let tail_n = MAX_TRANSCRIPT_CHARS - head_n;
        let head: String = out.chars().take(head_n).collect();
        let tail: String = out.chars().skip(total - tail_n).collect();
        return format!("{}\n\n[... middle of the transcript omitted for length ...]\n\n{}", head, tail);
    }
    out
}

/// Put the stored summary in front of the kept messages. It is merged into the first user
/// message when there is one, so the history still starts with a user turn and never has two
/// user turns in a row (some providers reject both).
pub fn attach_summary(history: &mut Vec<HistoryMessage>, summary: &str) {
    let summary = summary.trim();
    if summary.is_empty() {
        return;
    }
    let block = json!({
        "type": "text",
        "text": format!("{}\n\n{}\n\n{}", SUMMARY_HEADER, summary, SUMMARY_FOOTER)
    });
    match history.first_mut() {
        Some(first) if first.role == "user" && !has_block(first, "tool_result") => {
            first.blocks.insert(0, block);
        }
        _ => history.insert(0, HistoryMessage { role: "user".to_string(), blocks: vec![block] }),
    }
}

/// Decide whether stored history needs compacting, and if so how many leading messages to
/// fold into the summary. `None` = leave it alone.
pub fn plan_compaction(messages: &[HistoryMessage], summary_chars: usize, budget_chars: usize) -> Option<usize> {
    let total: usize = messages.iter().map(HistoryMessage::char_len).sum::<usize>() + summary_chars;
    let too_big = total > budget_chars / 10 * 6;
    let too_many = messages.len() >= MAX_HISTORY_ROWS / 4 * 3;
    if !too_big && !too_many {
        return None;
    }
    let cut = choose_cut(messages, budget_chars / 4, MAX_HISTORY_ROWS / 4);
    (cut > 0).then_some(cut)
}

/// Final shaping shared by every path: enforce the hard budget (dropping the oldest messages
/// — only reached when compaction is off or failed), repair tool pairs, attach the summary.
pub fn finalize_history(mut history: Vec<HistoryMessage>, summary: &str, budget_chars: usize) -> Vec<HistoryMessage> {
    let mut total: usize = history.iter().map(HistoryMessage::char_len).sum();
    while total > budget_chars && history.len() > 2 {
        total -= history[0].char_len();
        history.remove(0);
    }
    sanitize_tool_pairs(&mut history);
    attach_summary(&mut history, summary);
    history
}

const SUMMARIZER_SYSTEM: &str = "You compress the earlier part of a conversation between a user and an AI assistant that works with tools (files, shell, web, screenshots) so the assistant can continue the work without the original messages. Write only the summary.";

fn summarizer_prompt(previous: &str, transcript: &str) -> String {
    let mut prompt = String::new();
    if !previous.trim().is_empty() {
        prompt.push_str("SUMMARY OF EVEN EARLIER CONVERSATION (fold this in; keep what still matters):\n");
        prompt.push_str(previous.trim());
        prompt.push_str("\n\n");
    }
    prompt.push_str("CONVERSATION TO SUMMARIZE:\n");
    prompt.push_str(transcript);
    prompt.push_str(
        "\n\nWrite the summary the assistant will rely on from now on. Use these sections, omitting any that are empty:\n\
        ## What the user wants\nTheir goals and requests, in order, including ones already finished.\n\
        ## Instructions and preferences\nAnything the user told the assistant to do, avoid or prefer. Quote exact wording where it matters.\n\
        ## What was done\nWork completed, decisions made and why. Name files, functions, commands, URLs and other identifiers exactly.\n\
        ## Findings\nFacts learned from tools that are still needed (errors, outputs, values).\n\
        ## Current state and next steps\nWhat was in progress at the end, what is unfinished or unverified, and what comes next.\n\n\
        Be specific and factual; do not invent anything that is not in the conversation. Aim for under 900 words.",
    );
    prompt
}

/// Summarize with the configured chat model (one non-streaming call).
async fn summarize(state: &AppState, previous: &str, transcript: &str) -> Result<String, String> {
    let cfg = crate::llm::resolve_llm_config(state)?;
    let prompt = summarizer_prompt(previous, transcript);
    let text = crate::llm::complete_once(state, &cfg, SUMMARIZER_SYSTEM, &prompt, 3000, crate::llm::ONE_SHOT_LONG_SECS).await?;
    if text.chars().count() < 40 {
        return Err("the model returned no usable summary".to_string());
    }
    Ok(text)
}

/// Load the history to send for this turn, compacting it first if it has grown too large.
/// Compaction is best-effort: any failure falls back to sending the newest messages that fit,
/// so it can never fail a turn.
pub async fn prepare_history(app: &AppHandle, state: &AppState, session_id: &str) -> Result<Vec<HistoryMessage>, String> {
    let budget = budget_chars(state);
    let (mut summary, summary_upto) = crate::db::get_context_summary(state, session_id)?;
    let mut rows = crate::db::load_history_rows(state, session_id, summary_upto, MAX_HISTORY_ROWS)?;

    if auto_compact_enabled(state) {
        let messages: Vec<HistoryMessage> = rows.iter().map(|(_, m)| m.clone()).collect();
        if let Some(cut) = plan_compaction(&messages, summary.len(), budget) {
            let transcript = render_transcript(&messages[..cut]);
            match summarize(state, &summary, &transcript).await {
                Ok(new_summary) => {
                    let upto = rows[cut - 1].0;
                    crate::db::set_context_summary(state, session_id, &new_summary, upto)?;
                    rows.drain(..cut);
                    summary = new_summary;
                    add_feed_item(
                        state,
                        "system",
                        "Conversation compacted",
                        &format!("Summarized {} earlier messages to free up context.", cut),
                        "robot",
                        session_id,
                    );
                    let _ = app.emit(
                        "context-compacted",
                        json!({"session_id": session_id, "messages": cut, "ok": true}),
                    );
                }
                Err(e) => {
                    add_feed_item(
                        state,
                        "system",
                        "Conversation compaction failed",
                        &format!("Could not summarize earlier messages ({}); sending the most recent ones instead.", e),
                        "error",
                        session_id,
                    );
                    let _ = app.emit(
                        "context-compacted",
                        json!({"session_id": session_id, "messages": 0, "ok": false}),
                    );
                }
            }
        }
    }

    let history: Vec<HistoryMessage> = rows.into_iter().map(|(_, m)| m).collect();
    Ok(finalize_history(history, &summary, budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> HistoryMessage {
        HistoryMessage::text("user", text)
    }
    fn assistant(text: &str) -> HistoryMessage {
        HistoryMessage::text("assistant", text)
    }
    fn tool_call(id: &str) -> HistoryMessage {
        HistoryMessage {
            role: "assistant".into(),
            blocks: vec![json!({"type": "tool_use", "id": id, "name": "read_file", "input": {"path": "a.rs"}})],
        }
    }
    fn tool_result(id: &str, content: &str) -> HistoryMessage {
        HistoryMessage {
            role: "user".into(),
            blocks: vec![json!({"type": "tool_result", "tool_use_id": id, "content": content})],
        }
    }
    fn png() -> (String, String) {
        ("image/png".to_string(), "A".repeat(200_000))
    }
    fn anthropic_image_result(id: &str) -> Value {
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": "Captured."},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(200_000)}}
        ]})
    }

    #[test]
    fn images_count_as_a_fixed_size() {
        let msg = anthropic_image_result("t1");
        let size = value_chars(&msg);
        assert!(size > IMAGE_CHARS && size < IMAGE_CHARS + 300, "got {}", size);
        let openai = tool_image_message_openai(&[png()]);
        assert!(value_chars(&openai) < IMAGE_CHARS + 300);
        // Plain text is counted at roughly its length.
        let text = json!({"role": "user", "content": "x".repeat(1000)});
        assert!((1000..1100).contains(&value_chars(&text)));
    }

    #[test]
    fn stale_tool_images_are_pruned_in_batches_and_user_images_kept() {
        let user_image = json!({"role": "user", "content": [
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
        ]});
        let mut messages = vec![
            user_image.clone(),
            anthropic_image_result("t1"),
            anthropic_image_result("t2"),
            anthropic_image_result("t3"),
        ];
        // Three carriers: below the batch threshold, nothing changes (prefix stays cached).
        assert_eq!(prune_stale_images(&mut messages, 2, 4), 0);
        messages.push(anthropic_image_result("t4"));
        assert_eq!(prune_stale_images(&mut messages, 2, 4), 2);
        assert!(!contains_images(&messages[1..3]));
        assert!(contains_images(&messages[3..]));
        // The tool_result block survives, so pairing is intact.
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][1]["type"], "text");
        // The user's own attachment is never pruned.
        assert_eq!(messages[0], user_image);
        // Idempotent.
        assert_eq!(prune_stale_images(&mut messages, 2, 4), 0);
    }

    #[test]
    fn openai_tool_image_messages_are_pruned_too() {
        let mut messages: Vec<Value> = (0..4).map(|_| tool_image_message_openai(&[png()])).collect();
        assert_eq!(prune_stale_images(&mut messages, 2, 4), 2);
        assert!(!contains_images(&messages[..2]));
        assert!(contains_images(&messages[2..]));
        assert_eq!(messages[0]["content"][0]["text"], TOOL_IMAGE_NOTE);
    }

    #[test]
    fn strip_all_images_removes_user_images_as_well() {
        let mut messages = vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
            ]}),
            anthropic_image_result("t1"),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert!(contains_images(&messages));
        assert_eq!(strip_all_images(&mut messages), 2);
        assert!(!contains_images(&messages));
        assert_eq!(messages[2]["content"], "ok");
    }

    #[test]
    fn old_tool_output_is_cleared_oldest_first_keeping_recent_and_pairs() {
        let big = "x".repeat(20_000);
        let mut messages = vec![json!({"role": "system", "content": "sys"})];
        for i in 0..10 {
            messages.push(json!({"role": "assistant", "content": null, "tool_calls": [{"id": format!("c{}", i)}]}));
            messages.push(json!({"role": "tool", "tool_call_id": format!("c{}", i), "content": big}));
        }
        let before = messages_chars(&messages);
        assert!(before > 200_000);

        // Under target: untouched.
        assert_eq!(clear_old_tool_results(&mut messages.clone(), 3, before + 1), 0);

        let cleared = clear_old_tool_results(&mut messages, 3, 100_000);
        assert!(cleared >= 5 && cleared <= 7, "cleared {}", cleared);
        assert!(messages_chars(&messages) <= 100_000 + 20_000);
        // Oldest cleared, with a head kept and a marker; newest three untouched.
        let first = messages[2]["content"].as_str().unwrap();
        assert!(first.starts_with("xxx") && first.contains(CLEARED_MARK) && first.len() < 600);
        for k in 0..3 {
            assert_eq!(messages[messages.len() - 1 - 2 * k]["content"].as_str().unwrap().len(), 20_000);
        }
        // Structure is unchanged: same roles and ids in the same order.
        assert_eq!(messages.len(), 21);
        assert_eq!(messages[2]["tool_call_id"], "c0");
        // Already-cleared results are not cleared again.
        let again = clear_old_tool_results(&mut messages, 3, 0);
        assert_eq!(again, 7 - cleared);
    }

    #[test]
    fn anthropic_tool_results_are_cleared_in_place() {
        let mut messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "y".repeat(5000)}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "read_file", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "content": "z".repeat(5000)}]}),
        ];
        assert_eq!(clear_old_tool_results(&mut messages, 1, 0), 1);
        let cleared = messages[2]["content"][0]["content"].as_str().unwrap();
        assert!(cleared.contains(CLEARED_MARK));
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(messages[4]["content"][0]["content"].as_str().unwrap().len(), 5000);
        // The user's own text is never touched.
        assert_eq!(messages[0]["content"][0]["text"], "go");
    }

    #[test]
    fn maintain_only_clears_past_the_threshold() {
        let big = "x".repeat(10_000);
        let mut messages: Vec<Value> = (0..20)
            .map(|i| json!({"role": "tool", "tool_call_id": format!("c{}", i), "content": big}))
            .collect();
        // ~200k chars. With a 400k budget (80% = 320k) nothing happens.
        assert_eq!(maintain(&mut messages, 400_000), (0, 0));
        // With a 240k budget (80% = 192k) it clears down toward 55% (132k).
        let (_, cleared) = maintain(&mut messages, 240_000);
        assert!(cleared > 0);
        assert!(messages_chars(&messages) <= 140_000, "{}", messages_chars(&messages));
        // And is then stable: the next round does not rewrite the prefix again.
        assert_eq!(maintain(&mut messages, 240_000), (0, 0));
    }

    #[test]
    fn cut_lands_on_a_turn_boundary_never_inside_a_tool_round() {
        let big = "r".repeat(4000);
        let history = vec![
            user("first request"),          // 0
            tool_call("a"),                 // 1
            tool_result("a", &big),         // 2
            assistant("done one"),          // 3
            user("second request"),         // 4
            tool_call("b"),                 // 5
            tool_result("b", &big),         // 6
            assistant("done two"),          // 7
            user("third request"),          // 8
        ];
        // Tail budget fits only the last message.
        assert_eq!(choose_cut(&history, 200, 100), 8);
        // Fits the second turn too: the cut moves back to its start, not into its tool round.
        assert_eq!(choose_cut(&history, 6000, 100), 4);
        // Row limit wins over a generous size budget.
        assert_eq!(choose_cut(&history, 1_000_000, 3), 8);
        // Everything fits: still summarize the first turn at most (cut never 0 here because
        // index 0 is not a candidate), and a single-turn history has nothing to cut.
        assert_eq!(choose_cut(&history, 1_000_000, 100), 4);
        assert_eq!(choose_cut(&history[..4], 1_000_000, 100), 0);
        // A final turn that is itself too big is kept whole.
        let mut long = history.clone();
        long.push(tool_call("c"));
        long.push(tool_result("c", &"q".repeat(50_000)));
        assert_eq!(choose_cut(&long, 200, 100), 8);
    }

    #[test]
    fn compaction_is_planned_only_when_history_is_large() {
        let small = vec![user("a"), assistant("b"), user("c")];
        assert_eq!(plan_compaction(&small, 0, 1_000_000), None);

        let mut big = Vec::new();
        for i in 0..30 {
            big.push(user(&format!("request {}", i)));
            big.push(assistant(&"w".repeat(5000)));
        }
        big.push(user("latest"));
        // ~150k chars against a 200k budget: over the 60% trigger.
        let cut = plan_compaction(&big, 0, 200_000).expect("should compact");
        assert!(cut > 0 && cut < big.len());
        assert!(is_turn_start(&big[cut]));
        let tail: usize = big[cut..].iter().map(HistoryMessage::char_len).sum();
        assert!(tail <= 50_000, "tail {}", tail);
        // A large stored summary counts toward the trigger; the cut is the last turn start.
        assert_eq!(plan_compaction(&small, 900_000, 1_000_000), Some(2));
        // A single turn has no earlier turn to fold away, however large it is.
        let one_turn = vec![user("go"), assistant(&"w".repeat(900_000))];
        assert_eq!(plan_compaction(&one_turn, 0, 1_000_000), None);
    }

    #[test]
    fn summary_is_merged_into_the_first_user_message() {
        let mut history = vec![user("continue please"), assistant("ok")];
        attach_summary(&mut history, "  The user is building X.  ");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].blocks.len(), 2);
        let first = history[0].blocks[0]["text"].as_str().unwrap();
        assert!(first.starts_with(SUMMARY_HEADER) && first.contains("The user is building X.") && first.ends_with(SUMMARY_FOOTER));
        assert_eq!(history[0].blocks[1]["text"], "continue please");

        // History starting with an assistant turn gets a user turn in front instead.
        let mut history = vec![assistant("ok"), user("next")];
        attach_summary(&mut history, "S");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[1].role, "assistant");

        // No summary: untouched.
        let mut history = vec![user("hi")];
        attach_summary(&mut history, "   ");
        assert_eq!(history[0].blocks.len(), 1);
    }

    #[test]
    fn finalize_enforces_budget_and_keeps_tool_pairs_valid() {
        let history = vec![
            user("old"),
            tool_call("a"),
            tool_result("a", &"r".repeat(3000)),
            assistant("did it"),
            user("new"),
        ];
        // Budget forces the oldest messages out; the orphaned tool_result must not survive.
        let out = finalize_history(history.clone(), "Earlier: stuff.", 400);
        assert_eq!(out[0].role, "user");
        assert!(out[0].blocks[0]["text"].as_str().unwrap().contains("Earlier: stuff."));
        assert!(!out.iter().any(|m| has_block(m, "tool_result")));
        assert_eq!(out.last().unwrap().plain_text(), "new");
        // Roomy budget: everything kept, in order.
        let out = finalize_history(history, "", 1_000_000);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn transcript_abbreviates_tools_and_caps_length() {
        let history = vec![
            user("please fix the bug"),
            tool_call("a"),
            tool_result("a", &"L".repeat(5000)),
            assistant("fixed it"),
        ];
        let t = render_transcript(&history);
        assert!(t.contains("User: please fix the bug"));
        assert!(t.contains("Assistant called tool read_file({\"path\":\"a.rs\"})"));
        assert!(t.contains("Tool result: LLL") && t.contains("more characters]"));
        assert!(t.contains("Assistant: fixed it"));
        assert!(t.len() < 1200, "{}", t.len());

        let huge: Vec<HistoryMessage> = (0..300).map(|i| user(&format!("{} {}", i, "m".repeat(3500)))).collect();
        let t = render_transcript(&huge);
        assert!(t.chars().count() <= MAX_TRANSCRIPT_CHARS + 100);
        assert!(t.contains("middle of the transcript omitted"));
        assert!(t.starts_with("User: 0 ") && t.contains("User: 299 "));
    }

    #[test]
    fn summarizer_prompt_includes_previous_summary_and_transcript() {
        let p = summarizer_prompt("old summary", "User: hi");
        assert!(p.contains("old summary") && p.contains("User: hi"));
        assert!(p.find("old summary").unwrap() < p.find("User: hi").unwrap());
        assert!(!summarizer_prompt("", "User: hi").contains("EVEN EARLIER"));
    }
}
