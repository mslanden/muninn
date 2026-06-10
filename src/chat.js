import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { chatStreams, ui } from "./state.js";
import { escapeHtml, field, projectLabel, renderMarkdown, threadLabel } from "./utils.js";
import { focusTile, addTile, getChatTileBySessionId, saveLayout } from "./tilesys.js";

export async function openFeedItemInChat(item) {
  const title = item.title || "";
  const url = item.url || "";
  const sourceName = item.sourceName ?? item.source_name ?? "";
  const summary = item.summary || "";

  let seed = `I'm reading this item from my feed and want to dive deeper. Give me a richer overview, the key context, and why it matters — then I'll ask follow-ups.\n\n`;
  seed += `Title: ${title}\n`;
  if (sourceName) seed += `Source: ${sourceName}\n`;
  if (url) seed += `URL: ${url}\n`;
  if (summary) seed += `Summary: ${summary}\n`;

  try {
    const session = await invoke("create_session", { req: { projectPath: "" } });
    const sid = field(session, "id", "id");
    await addChatTile(sid, "");
    const tile = getChatTileBySessionId(sid);
    if (!tile) return;
    focusTile(tile.id);
    const input = tile.el.querySelector(`[data-chat-input="${tile.id}"]`);
    const sendBtn = tile.el.querySelector(`[data-chat-send="${tile.id}"]`);
    if (input && sendBtn) {
      input.value = seed;
      autoResize(input);
      sendBtn.click();
    }
  } catch (e) {
    console.error("Failed to open feed item in chat:", e);
  }
}

// Open a fresh chat tile seeded with a web-search query and auto-send, so the user can
// keep digging into a search result conversationally.
export async function openQueryInChat(query) {
  const q = String(query || "").trim();
  if (!q) return;
  const seed = `I searched the web for: "${q}". Give me a richer, well-sourced overview and the key links, then I'll ask follow-ups.`;
  try {
    const session = await invoke("create_session", { req: { projectPath: "" } });
    const sid = field(session, "id", "id");
    await addChatTile(sid, "");
    const tile = getChatTileBySessionId(sid);
    if (!tile) return;
    focusTile(tile.id);
    const input = tile.el.querySelector(`[data-chat-input="${tile.id}"]`);
    const sendBtn = tile.el.querySelector(`[data-chat-send="${tile.id}"]`);
    if (input && sendBtn) {
      input.value = seed;
      autoResize(input);
      sendBtn.click();
    }
  } catch (e) {
    console.error("Failed to open query in chat:", e);
  }
}

// ========== CHAT TILE ==========
export async function addChatTile(existingSessionId, projectPath) {
  let chatSessionId = existingSessionId || "";
  const pp = projectPath || "";

  if (!chatSessionId) {
    try {
      const session = await invoke("create_session", {
        req: { projectPath: pp }
      });
      chatSessionId = field(session, "id", "id");
    } catch (e) {
      console.error("Failed to create session:", e);
      return;
    }
  }

  const subtitle = pp ? projectLabel(pp) : chatSessionId.slice(0, 8);

  const tileObj = addTile("chat", {
    chatId: chatSessionId,
    subtitle,
    projectPath: pp
  });

  // Init streaming state
  chatStreams[tileObj.id] = {
    bubble: null,
    text: "",
    toolElement: null,
    thinkingEl: null,
    sessionId: chatSessionId
  };

  // Load existing messages
  await loadMessagesForTile(tileObj);

  // Populate thread select
  await refreshThreadSelect(tileObj);
}

export async function addProjectChatTile(projectPath) {
  try {
    await invoke("open_project", { req: { path: projectPath } });
  } catch {}
  try {
    const session = await invoke("create_session", {
      req: { projectPath }
    });
    const chatSessionId = field(session, "id", "id");
    await addChatTile(chatSessionId, projectPath);
  } catch (e) {
    console.error("Failed to create project session:", e);
  }
}

export async function refreshThreadSelect(tileObj) {
  const select = tileObj.el.querySelector(`[data-thread-select="${tileObj.id}"]`);
  if (!select) return;

  const stream = chatStreams[tileObj.id];
  if (!stream) return;

  const pp = tileObj.projectPath || "";
  try {
    const sessions = await invoke("get_sessions", { req: { limit: 20, projectPath: pp || null } });
    select.innerHTML = '<option value="__new__">+ New Chat</option>';
    for (let i = 0; i < sessions.length; i++) {
      const s = sessions[i];
      const sid = field(s, "id", "id");
      const opt = document.createElement("option");
      opt.value = sid;
      opt.textContent = threadLabel(s, i);
      if (sid === stream.sessionId) opt.selected = true;
      select.appendChild(opt);
    }
  } catch {}
}

export function renderChatTile(tileObj, body) {
  body.classList.add("chat-scroll");
  body.innerHTML = `
    <div class="todo-panel" data-todo-panel="${tileObj.id}" hidden></div>
    <div class="chat-stream" data-chat-stream="${tileObj.id}"></div>
  `;

  // Add thread select + project badge to tile header
  const header = tileObj.el.querySelector(".tile-header");
  if (header) {
    const spacer = header.querySelector(".tile-spacer");
    if (spacer) {
      // Project badge
      if (tileObj.projectPath) {
        const badge = document.createElement("span");
        badge.className = "project-badge";
        badge.textContent = projectLabel(tileObj.projectPath);
        badge.title = tileObj.projectPath;
        spacer.before(badge);
      }
      // Thread select
      const sel = document.createElement("select");
      sel.className = "thread-select";
      sel.dataset.threadSelect = tileObj.id;
      sel.innerHTML = '<option value="__new__">+ New Chat</option>';
      sel.addEventListener("change", async () => {
        const stream = chatStreams[tileObj.id];
        if (!stream) return;
        stream.contextTokens = null;
        if (sel.value === "__new__") {
          const pp = tileObj.projectPath || "";
          const session = await invoke("create_session", { req: { projectPath: pp } });
          const newId = field(session, "id", "id");
          stream.sessionId = newId;
          tileObj.chatId = newId;
          const chatLog = tileObj.el.querySelector(`[data-chat-stream="${tileObj.id}"]`);
          if (chatLog) chatLog.innerHTML = "";
          stream.bubble = null;
          stream.text = "";
          updateContextBar(tileObj.id, 0);
          await refreshThreadSelect(tileObj);
        } else {
          stream.sessionId = sel.value;
          tileObj.chatId = sel.value;
          await loadMessagesForTile(tileObj);
        }
        saveLayout();
      });
      spacer.before(sel);
    }
  }

  const composer = document.createElement("div");
  composer.className = "composer";
  composer.innerHTML = `
    <div class="context-bar" data-context-bar="${tileObj.id}">
      <div class="context-fill" data-context-fill="${tileObj.id}"></div>
      <span class="context-label" data-context-label="${tileObj.id}"></span>
    </div>
    <div class="composer-box">
      <textarea rows="1" placeholder="Message..." data-chat-input="${tileObj.id}"></textarea>
      <div class="composer-bar">
        <div class="composer-left">
          <button class="hint-chip" data-hint="slash">/</button>
        </div>
        <button class="send-btn" data-chat-send="${tileObj.id}">SEND</button>
      </div>
    </div>
  `;

  // Insert composer after tile-body (as sibling, inside tile)
  tileObj.el.appendChild(composer);

  const input = composer.querySelector(`[data-chat-input="${tileObj.id}"]`);
  const sendBtn = composer.querySelector(`[data-chat-send="${tileObj.id}"]`);

  const doSend = async () => {
    const text = input.value.trim();
    if (!text) return;

    const stream = chatStreams[tileObj.id];
    if (!stream) return;

    const chatLog = tileObj.el.querySelector(`[data-chat-stream="${tileObj.id}"]`);

    // Handle /stop command
    if (text.toLowerCase().startsWith("/stop")) {
      const stream = chatStreams[tileObj.id];
      if (stream && stream.sessionId) {
        try {
          await invoke("stop_chat", { sessionId: stream.sessionId });
          addTurn(chatLog, "muninn", "Stopping generation...");
        } catch (e) {
          addTurn(chatLog, "muninn", `Error: ${String(e)}`);
        }
      } else {
        addTurn(chatLog, "muninn", "No active generation to stop.");
      }
      input.value = "";
      autoResize(input);
      return;
    }

    // Handle /init command
    if (text.startsWith("/init")) {
      const initPath = text.replace(/^\/init\s*/, "").trim() || tileObj.projectPath;
      if (!initPath) {
        addTurn(chatLog, "muninn", "Usage: /init or /init /path/to/project (use in a project chat or provide a path)");
        return;
      }
      addTurn(chatLog, "you", text);
      input.value = "";
      autoResize(input);
      showThinking(tileObj.id, "Exploring project");
      try {
        // init_project streams chat-token and tool-executed events to the active tile
        await invoke("init_project", { req: { path: initPath } });
        hideThinking(tileObj.id);
        // Finalize the streamed bubble
        const stream = chatStreams[tileObj.id];
        if (stream && stream.bubble) {
          stream.bubble.innerHTML = renderMarkdown(stream.text);
          stream.bubble = null;
          stream.text = "";
        }
        addTurn(chatLog, "muninn", `AGENTS.md saved to ${initPath}/AGENTS.md`);
      } catch (e) {
        hideThinking(tileObj.id);
        addTurn(chatLog, "muninn", `Error: ${String(e)}`);
      }
      return;
    }

    // Parse slash commands
    let agentId = null;
    let message = text;
    let agentName = null;
    if (text.startsWith("/")) {
      const parsed = parseSlashCommand(text);
      if (parsed) {
        if (parsed.error) {
          addTurn(chatLog, "muninn", parsed.error);
          return;
        }
        agentId = field(parsed.agent, "agentId", "agent_id");
        agentName = field(parsed.agent, "name") || "Agent";
        message = parsed.query;
      }
    }

    addTurn(chatLog, "you", text);
    input.value = "";
    autoResize(input);
    sendBtn.disabled = true;
    stream.bubble = null;
    stream.text = "";

    try {
      showThinking(tileObj.id, agentName ? `${agentName} thinking` : undefined);
      const req = {
        sessionId: stream.sessionId,
        message,
        provider: "minimax",
        model: ui.loadedSettings["llm/model"] || "MiniMax-M2.7"
      };
      if (agentId) req.agentId = agentId;
      await invoke("chat", { req });

      if (stream.bubble) {
        stream.bubble.innerHTML = renderMarkdown(stream.text);
        stream.bubble = null;
        stream.text = "";
      }
      hideThinking(tileObj.id);
    } catch (error) {
      hideThinking(tileObj.id);
      if (stream.bubble) {
        stream.bubble.remove();
        stream.bubble = null;
        stream.text = "";
      }
      addTurn(chatLog, "muninn", `Error: ${String(error)}`);
    } finally {
      sendBtn.disabled = false;
      // If no API usage was reported, estimate from message content
      if (!stream.contextTokens) {
        estimateContextFromMessages(tileObj.id, chatLog);
      }
    }
  };

  sendBtn.addEventListener("click", doSend);

  // Slash command picker
  const slashPicker = document.createElement("div");
  slashPicker.className = "slash-picker hidden";
  composer.querySelector(".composer-box").appendChild(slashPicker);
  let slashPickerIdx = -1;

  function updateSlashPicker() {
    const text = input.value;
    const match = text.match(/^\/(\S*)$/);
    if (!match) { slashPicker.classList.add("hidden"); slashPickerIdx = -1; return; }
    const partial = match[1].toLowerCase();
    const builtins = [
      { slash: "stop", name: "Stop", desc: "Cancel in-flight generation" },
      { slash: "init", name: "Init Project", desc: "Explore and index current project" }
    ];
    const agentItems = ui.cachedAgents
      .filter(a => field(a, "enabled") !== false && field(a, "slashCommand", "slash_command"))
      .map(a => ({ slash: field(a, "slashCommand", "slash_command"), name: field(a, "name"), desc: field(a, "description") || "" }));
    const all = [...builtins, ...agentItems].filter(c => c.slash.toLowerCase().startsWith(partial));
    if (!all.length) { slashPicker.classList.add("hidden"); slashPickerIdx = -1; return; }
    slashPickerIdx = Math.max(0, Math.min(slashPickerIdx, all.length - 1));
    slashPicker.innerHTML = all.map((c, i) =>
      `<div class="slash-picker-item${i === slashPickerIdx ? " active" : ""}" data-idx="${i}">
        <span class="slash-picker-cmd">/${escapeHtml(c.slash)}</span>
        <span class="slash-picker-name">${escapeHtml(c.name)}</span>
        <span class="slash-picker-desc">${escapeHtml(c.desc).substring(0, 60)}</span>
      </div>`
    ).join("");
    slashPicker.classList.remove("hidden");
    slashPicker.querySelectorAll(".slash-picker-item").forEach(el => {
      el.addEventListener("mousedown", (e) => {
        e.preventDefault();
        input.value = `/${all[parseInt(el.dataset.idx)].slash} `;
        slashPicker.classList.add("hidden");
        input.focus();
      });
    });
  }

  // Also wire up the hint chip to insert /
  const hintChip = composer.querySelector('[data-hint="slash"]');
  if (hintChip) {
    hintChip.addEventListener("click", () => {
      input.value = "/";
      input.focus();
      updateSlashPicker();
    });
  }

  input.addEventListener("keydown", (e) => {
    if (!slashPicker.classList.contains("hidden")) {
      const items = slashPicker.querySelectorAll(".slash-picker-item");
      if (e.key === "ArrowDown") { e.preventDefault(); slashPickerIdx = Math.min(slashPickerIdx + 1, items.length - 1); updateSlashPicker(); return; }
      if (e.key === "ArrowUp") { e.preventDefault(); slashPickerIdx = Math.max(slashPickerIdx - 1, 0); updateSlashPicker(); return; }
      if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
        if (slashPickerIdx >= 0 && items[slashPickerIdx]) {
          e.preventDefault();
          items[slashPickerIdx].dispatchEvent(new Event("mousedown"));
          return;
        }
      }
      if (e.key === "Escape") { e.preventDefault(); slashPicker.classList.add("hidden"); return; }
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void doSend();
    }
  });
  input.addEventListener("input", () => { autoResize(input); updateSlashPicker(); });
}

export function autoResize(textarea) {
  textarea.style.height = "auto";
  textarea.style.height = `${Math.min(textarea.scrollHeight, 220)}px`;
}

let currentTtsAudio = null;
const ttsCache = new Map();

export async function playTts(btn, text) {
  // If already playing, stop it
  if (btn.classList.contains("playing")) {
    if (currentTtsAudio) { currentTtsAudio.pause(); currentTtsAudio = null; }
    btn.classList.remove("playing");
    btn.innerHTML = "&#9654;";
    return;
  }

  // Strip markdown for cleaner TTS input
  const plain = text.replace(/```[\s\S]*?```/g, " code block ").replace(/[*_#`>\[\]()!|]/g, "").replace(/\n+/g, " ").trim();
  if (!plain) return;

  btn.classList.add("playing");
  btn.innerHTML = "&#9632;";

  try {
    // Use cache key based on first 200 chars (enough to be unique)
    const cacheKey = plain.slice(0, 200);
    let blobUrl = ttsCache.get(cacheKey);

    if (!blobUrl) {
      const resp = await invoke("tts_speak", { req: { text: plain, provider: "minimax" } });
      if (!resp.audioBase64) { btn.classList.remove("playing"); btn.innerHTML = "&#9654;"; return; }

      const audioBlob = new Blob(
        [Uint8Array.from(atob(resp.audioBase64), c => c.charCodeAt(0))],
        { type: resp.contentType || "audio/mp3" }
      );
      blobUrl = URL.createObjectURL(audioBlob);
      ttsCache.set(cacheKey, blobUrl);
    }

    const audio = new Audio(blobUrl);
    currentTtsAudio = audio;
    audio.onended = () => {
      btn.classList.remove("playing");
      btn.innerHTML = "&#9654;";
      currentTtsAudio = null;
    };
    audio.play();
  } catch (e) {
    console.error("TTS error:", e);
    btn.classList.remove("playing");
    btn.innerHTML = "&#9654;";
  }
}

export function addTurn(chatLog, role, text) {
  const turn = document.createElement("div");
  turn.className = "turn";
  const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const speakBtn = role === "muninn" ? `<button class="tts-btn" title="Read aloud">&#9654;</button>` : "";
  turn.innerHTML = `
    <div class="turn-header">
      <span class="turn-label ${role}">${role === "you" ? "YOU" : "MUNINN"}</span>
      <span class="turn-time">${time}</span>
      ${speakBtn}
    </div>
    <div class="turn-body">${renderMarkdown(text)}</div>
  `;
  if (role === "muninn") {
    const btn = turn.querySelector(".tts-btn");
    btn.addEventListener("click", () => playTts(btn, text));
  }
  chatLog.appendChild(turn);
  chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
}

export function addToolCard(chatLog, toolName, args, status) {
  const card = document.createElement("div");
  card.className = "tool-card";

  let argsPreview = "";
  try {
    const parsed = JSON.parse(args);
    if (parsed.command) argsPreview = parsed.command.slice(0, 60);
    else if (parsed.path) argsPreview = parsed.path;
    else if (parsed.pattern) argsPreview = parsed.pattern;
  } catch {}

  const dotClass = status === "running" ? "dot running" : "dot";
  const statusText = status || "running";

  card.innerHTML = `
    <span class="${dotClass}"></span>
    <span>${escapeHtml(toolName)}</span>
    ${argsPreview ? `<span class="target">${escapeHtml(argsPreview)}</span>` : ""}
    <span class="status-badge">${escapeHtml(statusText)}</span>
  `;

  chatLog.appendChild(card);
  chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
  return card;
}

export function renderTodoPanel(tileId, items) {
  const panel = document.querySelector(`[data-todo-panel="${tileId}"]`);
  if (!panel) return;
  if (!items || items.length === 0) {
    panel.hidden = true;
    panel.innerHTML = "";
    return;
  }
  const lines = items.map((it) => {
    const status = it.status || "pending";
    const label = status === "in_progress" && it.activeForm ? it.activeForm : it.content;
    const icon = status === "completed" ? "\u2713" : status === "in_progress" ? "\u25B6" : "\u25CB";
    return `<li class="todo-item todo-${status}"><span class="todo-icon">${icon}</span><span class="todo-text">${escapeHtml(label)}</span></li>`;
  }).join("");
  const done = items.filter((i) => i.status === "completed").length;
  panel.hidden = false;
  panel.innerHTML = `
    <div class="todo-header">Plan <span class="todo-progress">${done}/${items.length}</span></div>
    <ul class="todo-list">${lines}</ul>
  `;
}

export function showUserQuestion(tileId, sessionId, question, choices) {
  const chatLog = document.querySelector(`[data-chat-stream="${tileId}"]`);
  if (!chatLog) return;
  // Only one question prompt at a time per tile — replace any existing one.
  const existing = chatLog.querySelector(".user-question");
  if (existing) existing.remove();

  const card = document.createElement("div");
  card.className = "user-question";
  card.dataset.sessionId = sessionId;
  const choiceButtons = Array.isArray(choices) && choices.length > 0
    ? `<div class="question-choices">${choices.map((c, i) => `<button class="question-choice" data-choice-idx="${i}">${escapeHtml(c)}</button>`).join("")}</div>`
    : "";
  card.innerHTML = `
    <div class="question-header">Muninn is asking</div>
    <div class="question-body">${escapeHtml(question)}</div>
    ${choiceButtons}
    <div class="question-input-row">
      <input type="text" class="question-input" placeholder="Type your answer..." />
      <button class="question-submit">Send</button>
    </div>
  `;
  chatLog.appendChild(card);
  chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;

  const input = card.querySelector(".question-input");
  const submit = card.querySelector(".question-submit");
  const send = async (answer) => {
    const trimmed = String(answer || "").trim();
    if (!trimmed) return;
    try {
      await invoke("answer_user_question", { sessionId, answer: trimmed });
      // Replace the prompt with a read-only echo so the turn stream still
      // shows what the user said (the LLM will see this as a tool_result).
      card.classList.add("user-question-answered");
      card.innerHTML = `<div class="question-header">You answered</div><div class="question-body">${escapeHtml(trimmed)}</div>`;
    } catch (e) {
      console.error("answer_user_question failed:", e);
    }
  };
  submit.addEventListener("click", () => send(input.value));
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send(input.value);
    }
  });
  card.querySelectorAll(".question-choice").forEach((btn, idx) => {
    btn.addEventListener("click", () => send(choices[idx]));
  });
  // Autofocus the input so the user can just start typing.
  setTimeout(() => input.focus(), 0);
}

export function showThinking(tileId, label) {
  const stream = chatStreams[tileId];
  if (!stream) return;
  const chatLog = document.querySelector(`[data-chat-stream="${tileId}"]`);
  if (!chatLog) return;

  if (stream.thinkingEl) {
    stream.thinkingEl.querySelector(".thinking-dots").innerHTML =
      `${escapeHtml(label || "Thinking")}<span>.</span><span>.</span><span>.</span>`;
    return;
  }

  stream.thinkingEl = document.createElement("div");
  stream.thinkingEl.className = "thinking-turn";
  stream.thinkingEl.innerHTML = `<span class="thinking-dots">${escapeHtml(label || "Thinking")}<span>.</span><span>.</span><span>.</span></span>`;
  chatLog.appendChild(stream.thinkingEl);
  chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
}

export function hideThinking(tileId) {
  const stream = chatStreams[tileId];
  if (!stream || !stream.thinkingEl) return;
  stream.thinkingEl.remove();
  stream.thinkingEl = null;
}

export async function loadMessagesForTile(tileObj) {
  const stream = chatStreams[tileObj.id];
  if (!stream) return;

  const chatLog = tileObj.el.querySelector(`[data-chat-stream="${tileObj.id}"]`);
  if (!chatLog) return;

  try {
    const messages = await invoke("get_messages", { sessionId: stream.sessionId });
    const logs = await invoke("get_tool_logs", { sessionId: stream.sessionId });

    const timeline = [];
    for (const msg of messages) {
      timeline.push({
        type: "message",
        createdAt: field(msg, "createdAt", "created_at") || "",
        role: msg.role,
        content: msg.content
      });
    }
    for (const log of logs) {
      timeline.push({
        type: "tool",
        createdAt: field(log, "createdAt", "created_at") || "",
        kind: log.kind,
        status: log.status,
        output: log.output
      });
    }
    timeline.sort((a, b) => a.createdAt.localeCompare(b.createdAt));

    chatLog.innerHTML = "";
    for (const item of timeline) {
      if (item.type === "tool") {
        addToolCard(chatLog, item.kind, "", item.status);
      } else {
        const role = item.role === "user" ? "you" : "muninn";
        addTurn(chatLog, role, item.content);
      }
    }
    // Estimate context usage from loaded messages
    estimateContextFromMessages(tileObj.id, chatLog);
  } catch (e) {
    console.error("Failed to load messages:", e);
  }
}

export function parseSlashCommand(text) {
  const match = text.match(/^\/(\S+)\s*(.*)/s);
  if (!match) return null;
  const command = match[1].toLowerCase();
  const query = match[2].trim();
  const agent = ui.cachedAgents.find(
    a => (field(a, "slashCommand", "slash_command") || "").toLowerCase() === command && field(a, "enabled") !== false
  );
  if (!agent) return { error: `Unknown command /${command}` };
  return { agent, query: query || text };
}

export const CONTEXT_WINDOW = 204800;

export function updateContextBar(tileId, tokens) {
  const bar = document.querySelector(`[data-context-bar="${tileId}"]`);
  const fill = document.querySelector(`[data-context-fill="${tileId}"]`);
  const label = document.querySelector(`[data-context-label="${tileId}"]`);
  if (!bar || !fill || !label) return;

  const pct = Math.min((tokens / CONTEXT_WINDOW) * 100, 100);
  fill.style.width = `${pct}%`;

  if (pct > 85) {
    fill.style.background = "var(--red)";
  } else if (pct > 60) {
    fill.style.background = "var(--warn)";
  } else {
    fill.style.background = "var(--gold)";
  }

  const used = tokens >= 1000 ? `${(tokens / 1000).toFixed(1)}k` : `${tokens}`;
  const max = `${(CONTEXT_WINDOW / 1000).toFixed(0)}k`;
  label.textContent = `${used} / ${max} tokens (${pct.toFixed(0)}%)`;
  bar.style.display = tokens > 0 ? "block" : "none";
}

export function estimateContextFromMessages(tileId, chatLog) {
  if (!chatLog) return;
  const turns = chatLog.querySelectorAll(".turn-body");
  let totalChars = 0;
  for (const t of turns) {
    totalChars += (t.textContent || "").length;
  }
  // ~4 chars per token is a rough estimate
  const estimated = Math.round(totalChars / 4);
  if (estimated > 0) updateContextBar(tileId, estimated);
}


registerTileRenderer("chat", renderChatTile);
