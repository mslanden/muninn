import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { tiles, chatStreams, ui } from "./state.js";
import { escapeHtml, field, projectLabel, renderMarkdown, threadLabel } from "./utils.js";
import { focusTile, addTile, getChatTileBySessionId, saveLayout } from "./tilesys.js";
import { icon } from "./icons.js";

// Refresh the sidebar chat-history list after session-mutating actions. Imported
// lazily-at-call (module-level import is fine — only *invoked* at runtime, so the
// chat.js<->sidebar.js cycle never bites at init). Guarded so a missing sidebar
// (pop-out / quick-ask windows) never breaks chat.
import { refreshSessionList } from "./sidebar.js";
function bumpSessionList() {
  try { refreshSessionList(); } catch {}
}

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
export async function addChatTile(existingSessionId, projectPath, opts = {}) {
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
    projectPath: pp,
    autoVoice: !!opts.autoVoice
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

  // A new/opened session may not yet be reflected in the sidebar list.
  bumpSessionList();
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

// Reflect the current project path in a chat tile's header badge.
function updateProjectBadge(badge, projectPath) {
  if (projectPath) {
    badge.textContent = projectLabel(projectPath);
    badge.title = projectPath;
    badge.hidden = false;
  } else {
    badge.textContent = "";
    badge.title = "";
    badge.hidden = true;
  }
}

// Switch an existing chat tile to a new working folder. Registers the folder as
// a project (which caches AGENTS.md), starts a fresh session scoped to it, and
// rebinds the tile to that session.
async function setTileProject(tileObj, projectPath) {
  const stream = chatStreams[tileObj.id];
  if (!stream) return;
  try {
    await invoke("add_project", { req: { path: projectPath } });
  } catch (e) {
    console.error("add_project error:", e);
    return;
  }
  let session;
  try {
    session = await invoke("create_session", { req: { projectPath } });
  } catch (e) {
    console.error("Failed to create project session:", e);
    return;
  }
  const newId = field(session, "id", "id");
  stream.sessionId = newId;
  stream.contextTokens = null;
  stream.bubble = null;
  stream.text = "";
  tileObj.chatId = newId;
  tileObj.projectPath = projectPath;

  const chatLog = tileObj.el.querySelector(`[data-chat-stream="${tileObj.id}"]`);
  if (chatLog) {
    chatLog.innerHTML = "";
    showChatWelcome(chatLog);
  }
  updateContextBar(tileObj.id, 0);

  const badge = tileObj.el.querySelector(`[data-project-badge="${tileObj.id}"]`);
  if (badge) updateProjectBadge(badge, projectPath);

  await refreshThreadSelect(tileObj);
  saveLayout();
}

// ===== Voice input (native recorder -> Whisper transcription) =====
// Only one recording at a time (single native AudioRecorder). Tracks which tile
// initiated it so the "voice-recorded" event routes the transcript to the right input.
let voiceTileId = null;

function setMicState(tileObj, micState) {
  const btn = tileObj?.el?.querySelector(`[data-chat-mic="${tileObj.id}"]`);
  if (!btn) return;
  btn.classList.toggle("recording", micState === "recording");
  btn.classList.toggle("transcribing", micState === "transcribing");
  // "transcribing" swaps to a spinning loader; recording/idle use the mic glyph.
  if (micState === "transcribing") {
    btn.innerHTML = icon("loader", 16).replace('class="icon"', 'class="icon spin"');
  } else {
    btn.innerHTML = icon("mic", 16);
  }
  btn.title =
    micState === "recording" ? "Stop recording" :
    micState === "transcribing" ? "Transcribing…" : "Voice input";
}

async function toggleVoiceRecording(tileObj) {
  // Stop the in-progress recording for this tile.
  if (voiceTileId === tileObj.id) {
    setMicState(tileObj, "transcribing");
    try {
      await invoke("stop_voice_recording");
      // The "voice-recorded" event delivers the WAV; voiceTileId stays set until then.
    } catch (e) {
      console.error("stop_voice_recording error:", e);
      voiceTileId = null;
      setMicState(tileObj, "idle");
    }
    return;
  }
  // Another tile is already recording — ignore.
  if (voiceTileId !== null) return;
  try {
    await invoke("start_voice_recording");
    voiceTileId = tileObj.id;
    setMicState(tileObj, "recording");
  } catch (e) {
    console.error("start_voice_recording error:", e);
  }
}

// Called from the global "voice-recorded" listener with the base64 WAV.
export async function onVoiceRecorded(wavBase64) {
  const tileId = voiceTileId;
  voiceTileId = null;
  if (tileId == null) return;
  const tileObj = tiles.find((t) => t.id === tileId);
  try {
    const res = await invoke("transcribe_wav", { wavBase64 });
    const text = (res && (res.text ?? res.Text)) || "";
    if (tileObj) {
      const input = tileObj.el.querySelector(`[data-chat-input="${tileObj.id}"]`);
      if (input && text) {
        input.value = input.value.trim() ? `${input.value.trimEnd()} ${text}` : text;
        autoResize(input);
        input.focus();
        // Auto-respond mode: fire the send immediately and speak the reply when it lands.
        if (tileObj.autoVoice) {
          tileObj.speakNextReply = true;
          const sendBtn = tileObj.el.querySelector(`[data-chat-send="${tileObj.id}"]`);
          if (sendBtn) sendBtn.click();
        }
      }
    }
  } catch (e) {
    console.error("transcribe_wav error:", e);
  } finally {
    if (tileObj) setMicState(tileObj, "idle");
  }
}

// Called from the global "voice-error" listener.
export function onVoiceError(msg) {
  console.error("voice-error:", msg);
  const tileObj = tiles.find((t) => t.id === voiceTileId);
  voiceTileId = null;
  if (tileObj) setMicState(tileObj, "idle");
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
    <div class="chat-search" data-chat-search-bar="${tileObj.id}" hidden>
      <input type="text" placeholder="Search this conversation…" data-chat-search-input="${tileObj.id}" />
      <span class="chat-search-count" data-chat-search-count="${tileObj.id}"></span>
      <button class="chat-search-close" data-chat-search-close="${tileObj.id}" title="Close" aria-label="Close search">${icon("x")}</button>
    </div>
    <div class="todo-panel" data-todo-panel="${tileObj.id}" hidden></div>
    <div class="chat-stream" data-chat-stream="${tileObj.id}"></div>
  `;

  // Add thread select + project badge to tile header
  const header = tileObj.el.querySelector(".tile-header");
  if (header) {
    const spacer = header.querySelector(".tile-spacer");
    if (spacer) {
      // Project badge — always present so the folder button can update it.
      const badge = document.createElement("span");
      badge.className = "project-badge";
      badge.dataset.projectBadge = tileObj.id;
      updateProjectBadge(badge, tileObj.projectPath);
      spacer.before(badge);
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
          if (chatLog) {
            chatLog.innerHTML = "";
            showChatWelcome(chatLog);
          }
          stream.bubble = null;
          stream.text = "";
          hideGoalBanner(tileObj);
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
    <div class="goal-banner" data-goal-banner="${tileObj.id}" hidden>
      <span class="goal-banner-icon">${icon("target")}</span>
      <span class="goal-banner-text" data-goal-text="${tileObj.id}"></span>
      <span class="goal-banner-status" data-goal-status="${tileObj.id}"></span>
      <button class="goal-banner-x" data-goal-clear="${tileObj.id}" title="Clear goal" aria-label="Clear goal">${icon("x")}</button>
    </div>
    <div class="context-bar" data-context-bar="${tileObj.id}">
      <div class="context-fill" data-context-fill="${tileObj.id}"></div>
      <span class="context-label" data-context-label="${tileObj.id}"></span>
    </div>
    <div class="composer-box">
      <div class="composer-attachments" data-chat-attachments="${tileObj.id}" style="display:none;"></div>
      <textarea rows="1" placeholder="Message... (paste or drag an image to share a screenshot)" data-chat-input="${tileObj.id}"></textarea>
      <div class="composer-bar">
        <div class="composer-left">
          <button class="hint-chip" data-chat-folder="${tileObj.id}" title="Set working folder for this chat" aria-label="Set working folder">${icon("folder")}</button>
          <button class="hint-chip" data-chat-attach="${tileObj.id}" title="Attach an image / screenshot" aria-label="Attach">${icon("paperclip")}</button>
          <button class="hint-chip" data-chat-mic="${tileObj.id}" title="Voice input" aria-label="Voice input">${icon("mic")}</button>
          <button class="hint-chip${tileObj.autoVoice ? " active" : ""}" data-chat-autovoice="${tileObj.id}" title="Voice auto-respond: send the transcript immediately and read the reply aloud" aria-label="Toggle voice auto-respond" aria-pressed="${!!tileObj.autoVoice}">${icon("volume")}</button>
          <button class="hint-chip" data-chat-search-btn="${tileObj.id}" title="Search this conversation" aria-label="Search conversation">${icon("search")}</button>
          <button class="hint-chip" data-chat-export="${tileObj.id}" title="Copy conversation as Markdown" aria-label="Export conversation">${icon("download")}</button>
          <button class="hint-chip" data-hint="slash" aria-label="Slash commands">/</button>
          <input type="file" accept="image/*" multiple data-chat-file="${tileObj.id}" style="display:none;" />
        </div>
        <button class="send-btn" data-chat-send="${tileObj.id}">SEND</button>
      </div>
    </div>
  `;

  // Insert composer after tile-body (as sibling, inside tile)
  tileObj.el.appendChild(composer);

  const input = composer.querySelector(`[data-chat-input="${tileObj.id}"]`);
  const sendBtn = composer.querySelector(`[data-chat-send="${tileObj.id}"]`);
  const folderBtn = composer.querySelector(`[data-chat-folder="${tileObj.id}"]`);

  folderBtn.addEventListener("click", async () => {
    const selected = await openDialog({
      directory: true,
      multiple: false,
      title: "Set working folder for this chat",
    });
    if (!selected) return;
    await setTileProject(tileObj, selected);
  });

  const micBtn = composer.querySelector(`[data-chat-mic="${tileObj.id}"]`);
  micBtn.addEventListener("click", () => toggleVoiceRecording(tileObj));

  // Voice auto-respond toggle: when on, a finished transcription is sent without
  // touching the keyboard and the reply is read aloud (hands-free voice loop).
  const autoVoiceBtn = composer.querySelector(`[data-chat-autovoice="${tileObj.id}"]`);
  autoVoiceBtn.addEventListener("click", () => {
    tileObj.autoVoice = !tileObj.autoVoice;
    autoVoiceBtn.classList.toggle("active", tileObj.autoVoice);
    autoVoiceBtn.setAttribute("aria-pressed", String(tileObj.autoVoice));
    saveLayout();
  });

  // Goal banner clear button.
  const goalClearBtn = composer.querySelector(`[data-goal-clear="${tileObj.id}"]`);
  if (goalClearBtn) {
    goalClearBtn.addEventListener("click", async () => {
      const stream = chatStreams[tileObj.id];
      hideGoalBanner(tileObj);
      if (stream && stream.sessionId) {
        try {
          await invoke("set_session_goal", { sessionId: stream.sessionId, goal: "" });
        } catch (e) {
          console.error("Failed to clear goal:", e);
        }
      }
    });
  }

  // ---- Image attachments (paste / drag-drop / file picker) ----
  const attachBtn = composer.querySelector(`[data-chat-attach="${tileObj.id}"]`);
  const fileInput = composer.querySelector(`[data-chat-file="${tileObj.id}"]`);
  const attachStrip = composer.querySelector(`[data-chat-attachments="${tileObj.id}"]`);
  const MAX_IMAGE_BYTES = 10 * 1024 * 1024; // MiniMax limit is 10MB per image

  tileObj.pendingAttachments = [];

  function fileToAttachment(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => {
        const m = /^data:([^;]+);base64,(.*)$/.exec(reader.result || "");
        if (!m) { reject(new Error("unreadable image")); return; }
        resolve({ mediaType: m[1], data: m[2], dataUrl: reader.result });
      };
      reader.onerror = reject;
      reader.readAsDataURL(file);
    });
  }

  function renderAttachmentStrip() {
    const list = tileObj.pendingAttachments;
    if (!list.length) { attachStrip.style.display = "none"; attachStrip.innerHTML = ""; return; }
    attachStrip.style.display = "flex";
    attachStrip.innerHTML = list.map((a, i) =>
      `<div class="composer-thumb"><img src="${a.dataUrl}" alt="attachment" /><button class="composer-thumb-x" data-att-remove="${i}" title="Remove">×</button></div>`
    ).join("");
    attachStrip.querySelectorAll("[data-att-remove]").forEach(btn => {
      btn.addEventListener("click", () => {
        tileObj.pendingAttachments.splice(parseInt(btn.dataset.attRemove, 10), 1);
        renderAttachmentStrip();
      });
    });
  }

  async function addImageFiles(files) {
    for (const file of files) {
      if (!file.type || !file.type.startsWith("image/")) continue;
      if (file.size > MAX_IMAGE_BYTES) {
        addTurn(chatLog, "muninn", `That image is too large (${(file.size / 1048576).toFixed(1)}MB). Max is 10MB.`);
        continue;
      }
      try { tileObj.pendingAttachments.push(await fileToAttachment(file)); } catch { /* skip unreadable */ }
    }
    renderAttachmentStrip();
  }

  // Add an image from a base64 data URL (used by the native Tauri drag-drop handler in
  // events.js, which delivers file paths the OS intercepts before the DOM ever sees them).
  tileObj.addDroppedImage = (dataUrl) => {
    const m = /^data:([^;]+);base64,(.*)$/.exec(dataUrl || "");
    if (!m) return;
    tileObj.pendingAttachments.push({ mediaType: m[1], data: m[2], dataUrl });
    renderAttachmentStrip();
  };

  attachBtn.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    addImageFiles(Array.from(fileInput.files || []));
    fileInput.value = "";
  });

  input.addEventListener("paste", (e) => {
    const imgs = Array.from(e.clipboardData?.items || [])
      .filter(it => it.type && it.type.startsWith("image/"))
      .map(it => it.getAsFile())
      .filter(Boolean);
    if (imgs.length) { e.preventDefault(); addImageFiles(imgs); }
  });

  // Drag-and-drop anywhere over the chat tile.
  [composer, body].forEach(el => {
    el.addEventListener("dragover", (e) => { e.preventDefault(); composer.classList.add("drag-over"); });
    el.addEventListener("dragleave", (e) => { if (e.target === el) composer.classList.remove("drag-over"); });
    el.addEventListener("drop", (e) => {
      const files = Array.from(e.dataTransfer?.files || []).filter(f => f.type && f.type.startsWith("image/"));
      if (!files.length) return;
      e.preventDefault();
      composer.classList.remove("drag-over");
      addImageFiles(files);
    });
  });

  // ---- In-chat search (client-side over loaded turns) ----
  const searchBar = body.querySelector(`[data-chat-search-bar="${tileObj.id}"]`);
  const searchInput = body.querySelector(`[data-chat-search-input="${tileObj.id}"]`);
  const searchCount = body.querySelector(`[data-chat-search-count="${tileObj.id}"]`);
  const chatStreamEl = body.querySelector(`[data-chat-stream="${tileObj.id}"]`);

  function clearSearch() {
    chatStreamEl.querySelectorAll(".turn.search-hit, .turn.search-dim")
      .forEach(t => t.classList.remove("search-hit", "search-dim"));
  }
  function runSearch() {
    const q = searchInput.value.trim().toLowerCase();
    clearSearch();
    if (!q) { searchCount.textContent = ""; return; }
    const turns = Array.from(chatStreamEl.querySelectorAll(".turn"));
    let hits = [];
    for (const t of turns) {
      if ((t.textContent || "").toLowerCase().includes(q)) {
        t.classList.add("search-hit"); hits.push(t);
      } else {
        t.classList.add("search-dim");
      }
    }
    searchCount.textContent = hits.length ? `${hits.length} match${hits.length === 1 ? "" : "es"}` : "no matches";
    if (hits.length) hits[0].scrollIntoView({ block: "center", behavior: "smooth" });
  }
  composer.querySelector(`[data-chat-search-btn="${tileObj.id}"]`).addEventListener("click", () => {
    searchBar.hidden = !searchBar.hidden;
    if (!searchBar.hidden) { searchInput.focus(); }
    else { searchInput.value = ""; clearSearch(); searchCount.textContent = ""; }
  });
  searchInput.addEventListener("input", runSearch);
  searchInput.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { searchBar.hidden = true; searchInput.value = ""; clearSearch(); searchCount.textContent = ""; }
  });
  body.querySelector(`[data-chat-search-close="${tileObj.id}"]`).addEventListener("click", () => {
    searchBar.hidden = true; searchInput.value = ""; clearSearch(); searchCount.textContent = "";
  });

  // ---- Export conversation as Markdown (copy to clipboard) ----
  const exportBtn = composer.querySelector(`[data-chat-export="${tileObj.id}"]`);
  exportBtn.addEventListener("click", async () => {
    const stream = chatStreams[tileObj.id];
    if (!stream || !stream.sessionId) return;
    const original = exportBtn.innerHTML;
    try {
      const md = await invoke("export_session_markdown", { sessionId: stream.sessionId });
      await navigator.clipboard.writeText(md);
      exportBtn.innerHTML = icon("check");
    } catch (err) {
      console.error("export error:", err);
      exportBtn.innerHTML = icon("x");
    }
    setTimeout(() => { exportBtn.innerHTML = original; }, 1200);
  });

  const doSend = async () => {
    const text = input.value.trim();
    const attachments = (tileObj.pendingAttachments || []).slice();
    if (!text && !attachments.length) return;

    const stream = chatStreams[tileObj.id];
    if (!stream) return;

    const chatLog = tileObj.el.querySelector(`[data-chat-stream="${tileObj.id}"]`);
    clearChatWelcome(chatLog);

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

    // Handle /goal command (set/show/clear the session's auto-continue goal).
    if (text.toLowerCase() === "/goal" || text.toLowerCase().startsWith("/goal ")) {
      const rest = text.slice("/goal".length).trim();
      if (!stream.sessionId) {
        addTurn(chatLog, "muninn", "Start a chat before setting a goal.");
        input.value = "";
        autoResize(input);
        return;
      }
      if (rest === "") {
        // Show the current goal.
        try {
          const goal = await invoke("get_session_goal", { sessionId: stream.sessionId });
          if (goal && String(goal).trim()) {
            addTurn(chatLog, "muninn", `🎯 Active goal: ${String(goal)}`);
            showGoalBanner(tileObj, String(goal));
          } else {
            addTurn(chatLog, "muninn", "No active goal.");
          }
        } catch (e) {
          addTurn(chatLog, "muninn", `Error: ${String(e)}`);
        }
        input.value = "";
        autoResize(input);
        return;
      }
      if (rest.toLowerCase() === "clear") {
        try {
          await invoke("set_session_goal", { sessionId: stream.sessionId, goal: "" });
          hideGoalBanner(tileObj);
          addTurn(chatLog, "muninn", "Goal cleared.");
        } catch (e) {
          addTurn(chatLog, "muninn", `Error: ${String(e)}`);
        }
        input.value = "";
        autoResize(input);
        return;
      }
      // /goal <text> — set the goal, show the banner, then send a kickoff message.
      try {
        await invoke("set_session_goal", { sessionId: stream.sessionId, goal: rest });
        showGoalBanner(tileObj, rest);
      } catch (e) {
        addTurn(chatLog, "muninn", `Error: ${String(e)}`);
        input.value = "";
        autoResize(input);
        return;
      }
      // Fall through into the normal send path with the kickoff message.
      input.value = `Work toward this goal until it is fully met: ${rest}`;
      return void doSend();
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

    addTurn(chatLog, "you", text, attachments.map(a => a.dataUrl));
    input.value = "";
    autoResize(input);
    // Clear the pending-attachment strip now that they're sent.
    tileObj.pendingAttachments = [];
    renderAttachmentStrip();
    sendBtn.disabled = true;
    stream.bubble = null;
    stream.text = "";

    try {
      showThinking(tileObj.id, agentName ? `${agentName} thinking` : undefined);
      const req = {
        sessionId: stream.sessionId,
        message,
        provider: "minimax",
        model: ui.loadedSettings["llm/model"] || "MiniMax-M3"
      };
      if (agentId) req.agentId = agentId;
      if (attachments.length) req.attachments = attachments.map(a => ({ mediaType: a.mediaType, data: a.data }));
      await invoke("chat", { req });

      if (stream.bubble) {
        stream.bubble.innerHTML = renderMarkdown(stream.text);
        maybeAutoSpeak(tileObj, stream.text);
        stream.bubble = null;
        stream.text = "";
      }
      hideThinking(tileObj.id);
    } catch (error) {
      hideThinking(tileObj.id);
      tileObj.speakNextReply = false; // don't read errors (or a later reply) aloud
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
      // Reflect the fresh preview / last-message time in the sidebar list.
      bumpSessionList();
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
  // If already playing, stop it. `btn` is null when auto-respond speaks a reply
  // with no visible play button — skip the toggle/state updates in that case.
  if (btn && btn.classList.contains("playing")) {
    if (currentTtsAudio) { currentTtsAudio.pause(); currentTtsAudio = null; }
    btn.classList.remove("playing");
    btn.innerHTML = icon("play", 12);
    return;
  }
  // Auto-speak replaces whatever is currently playing.
  if (!btn && currentTtsAudio) { currentTtsAudio.pause(); currentTtsAudio = null; }

  // Strip markdown for cleaner TTS input
  const plain = text.replace(/```[\s\S]*?```/g, " code block ").replace(/[*_#`>\[\]()!|]/g, "").replace(/\n+/g, " ").trim();
  if (!plain) return;

  if (btn) {
    btn.classList.add("playing");
    btn.innerHTML = "&#9632;";
  }

  try {
    // Use cache key based on first 200 chars (enough to be unique)
    const cacheKey = plain.slice(0, 200);
    let blobUrl = ttsCache.get(cacheKey);

    if (!blobUrl) {
      // Empty provider → the backend resolves it from the `tts/provider` setting.
      const resp = await invoke("tts_speak", { req: { text: plain, provider: "" } });
      if (!resp.audioBase64) {
        if (btn) { btn.classList.remove("playing"); btn.innerHTML = icon("play", 12); }
        return;
      }

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
      if (btn) { btn.classList.remove("playing"); btn.innerHTML = icon("play", 12); }
      currentTtsAudio = null;
    };
    audio.play();
  } catch (e) {
    console.error("TTS error:", e);
    if (btn) { btn.classList.remove("playing"); btn.innerHTML = icon("play", 12); }
  }
}

// Voice auto-respond: speak a finished reply once if this tile's last send came
// from the mic. Consumes the flag so typed follow-ups stay silent. Called from
// both finalization paths (doSend's post-invoke branch and the chat-done event).
export function maybeAutoSpeak(tileObj, text) {
  if (!tileObj || !tileObj.speakNextReply) return;
  tileObj.speakNextReply = false;
  if (text && text.trim()) void playTts(null, text);
}

export function addTurn(chatLog, role, text, images = []) {
  const turn = document.createElement("div");
  turn.className = "turn";
  const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const speakBtn = role === "muninn" ? `<button class="tts-btn" title="Read aloud" aria-label="Read aloud">${icon("play", 12)}</button>` : "";
  const imagesHtml = (images && images.length)
    ? `<div class="turn-images">${images.map(src => `<img class="turn-image" src="${src}" alt="shared image" />`).join("")}</div>`
    : "";
  turn.innerHTML = `
    <div class="turn-header">
      <span class="turn-label ${role}">${role === "you" ? "YOU" : "MUNINN"}</span>
      <span class="turn-time">${time}</span>
      ${speakBtn}
    </div>
    <div class="turn-body">${renderMarkdown(text)}${imagesHtml}</div>
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

// Render a deferred-command card (auto-verify flow: the user was away when a verified
// command needed approval). Styled like the ask_user card. `rec` is a deferred_commands
// record (id, command, verifier_note, ...). `chatLog` is the tile's chat stream element.
export function renderDeferredCard(chatLog, rec) {
  if (!chatLog || !rec) return;
  const id = rec.id;
  // Don't double-render the same deferred command.
  if (chatLog.querySelector(`.deferred-command[data-deferred-id="${id}"]`)) return;

  const card = document.createElement("div");
  card.className = "user-question deferred-command";
  card.dataset.deferredId = String(id);
  const note = rec.verifier_note || rec.verifierNote || "";
  const command = rec.command || "";
  card.innerHTML = `
    <div class="question-header">Command awaiting approval</div>
    <pre class="deferred-command-code">${escapeHtml(command)}</pre>
    ${note ? `<div class="question-body">Safety verifier: ${escapeHtml(note)}</div>` : ""}
    <div class="question-choices">
      <button class="question-choice deferred-approve">Approve &amp; run</button>
      <button class="question-choice deferred-dismiss">Dismiss</button>
    </div>
    <div class="deferred-command-output" hidden></div>
  `;
  chatLog.appendChild(card);
  if (chatLog.parentElement) chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;

  const approveBtn = card.querySelector(".deferred-approve");
  const dismissBtn = card.querySelector(".deferred-dismiss");
  const outputEl = card.querySelector(".deferred-command-output");
  const disableButtons = () => {
    approveBtn.disabled = true;
    dismissBtn.disabled = true;
  };

  approveBtn.addEventListener("click", async () => {
    disableButtons();
    approveBtn.textContent = "Running…";
    try {
      const output = await invoke("approve_deferred_command", { id });
      card.classList.add("user-question-answered");
      approveBtn.textContent = "Approved";
      const capped = String(output || "").slice(0, 4000);
      outputEl.hidden = false;
      outputEl.textContent = capped + (String(output || "").length > 4000 ? "\n… (truncated)" : "");
    } catch (e) {
      console.error("approve_deferred_command failed:", e);
      outputEl.hidden = false;
      outputEl.textContent = "Failed: " + String(e);
      // Re-enable so the user can retry or dismiss.
      approveBtn.disabled = false;
      dismissBtn.disabled = false;
      approveBtn.textContent = "Approve & run";
    }
  });

  dismissBtn.addEventListener("click", async () => {
    disableButtons();
    try {
      await invoke("dismiss_deferred_command", { id });
    } catch (e) {
      console.error("dismiss_deferred_command failed:", e);
    }
    // Collapse the card either way.
    card.classList.add("user-question-answered");
    card.innerHTML = `<div class="question-header">Deferred command dismissed</div>`;
  });
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

/** Empty-chat hero: brand + short guidance. Cleared on first send. */
export function showChatWelcome(chatLog) {
  if (!chatLog) return;
  clearChatWelcome(chatLog);
  const el = document.createElement("div");
  el.className = "chat-welcome";
  el.innerHTML = `
    <div class="chat-welcome-brand">MUNINN</div>
    <p class="chat-welcome-lead">Ask a question, create images, edit video, or work in a project folder.</p>
    <ul class="chat-welcome-hints">
      <li>Type naturally — Muninn can use tools in your workspace</li>
      <li>Paste or attach a screenshot to talk about it</li>
      <li>Use <kbd>/</kbd> for agents and slash commands</li>
    </ul>
  `;
  chatLog.appendChild(el);
}

export function clearChatWelcome(chatLog) {
  if (!chatLog) return;
  chatLog.querySelectorAll(".chat-welcome").forEach((el) => el.remove());
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
      // Recover any image blocks so screenshots re-render on reload.
      const images = (msg.blocks || [])
        .filter(b => b && b.type === "image" && b.source && b.source.type === "base64")
        .map(b => `data:${b.source.media_type};base64,${b.source.data}`);
      timeline.push({
        type: "message",
        createdAt: field(msg, "createdAt", "created_at") || "",
        role: msg.role,
        content: msg.content,
        images
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
    if (!timeline.length) {
      showChatWelcome(chatLog);
    } else {
      for (const item of timeline) {
        if (item.type === "tool") {
          addToolCard(chatLog, item.kind, "", item.status);
        } else {
          const role = item.role === "user" ? "you" : "muninn";
          addTurn(chatLog, role, item.content, item.images || []);
        }
      }
    }
    // Estimate context usage from loaded messages
    estimateContextFromMessages(tileObj.id, chatLog);

    // Render any pending deferred commands for this session at the bottom.
    try {
      const deferred = await invoke("list_deferred_commands", { sessionId: stream.sessionId });
      for (const rec of deferred || []) renderDeferredCard(chatLog, rec);
    } catch (e) {
      console.error("Failed to load deferred commands:", e);
    }

    // Restore the goal banner for this session (hidden when there's no active goal).
    await restoreGoalBanner(tileObj);
  } catch (e) {
    console.error("Failed to load messages:", e);
  }
}

// ---- Goal banner (the "/goal" auto-continue loop) --------------------------------------

/** Show/refresh the goal banner on a tile with the given goal text. */
export function showGoalBanner(tileObj, goalText) {
  if (!tileObj || !tileObj.el) return;
  const banner = tileObj.el.querySelector(`[data-goal-banner="${tileObj.id}"]`);
  const textEl = tileObj.el.querySelector(`[data-goal-text="${tileObj.id}"]`);
  const statusEl = tileObj.el.querySelector(`[data-goal-status="${tileObj.id}"]`);
  if (!banner || !textEl) return;
  textEl.innerHTML = escapeHtml(goalText);
  if (statusEl) statusEl.textContent = "";
  banner.hidden = false;
}

/** Hide the goal banner on a tile. */
export function hideGoalBanner(tileObj) {
  if (!tileObj || !tileObj.el) return;
  const banner = tileObj.el.querySelector(`[data-goal-banner="${tileObj.id}"]`);
  if (banner) banner.hidden = true;
}

/** Update just the iteration/status text of a tile's goal banner (escaped). */
export function setGoalBannerStatus(tileObj, statusText) {
  if (!tileObj || !tileObj.el) return;
  const statusEl = tileObj.el.querySelector(`[data-goal-status="${tileObj.id}"]`);
  if (statusEl) statusEl.innerHTML = escapeHtml(statusText);
}

/** Restore the goal banner for a tile from persisted state. */
async function restoreGoalBanner(tileObj) {
  const stream = chatStreams[tileObj.id];
  if (!stream || !stream.sessionId) {
    hideGoalBanner(tileObj);
    return;
  }
  try {
    const goal = await invoke("get_session_goal", { sessionId: stream.sessionId });
    if (goal && String(goal).trim()) {
      showGoalBanner(tileObj, String(goal));
    } else {
      hideGoalBanner(tileObj);
    }
  } catch (e) {
    console.error("Failed to load session goal:", e);
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

export const CONTEXT_WINDOW = 1000000;

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
