import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { enable as enableAutostart, disable as disableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { WebviewWindow, getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

// ==================== THEME ====================
function applyTheme(pref) {
  let theme = pref;
  if (pref === "system") {
    theme = window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  }
  document.documentElement.setAttribute("data-theme", theme);
}

// Apply immediately to prevent flash
applyTheme(localStorage.getItem("spino-theme") || "dark");

// React to OS theme changes when set to "system"
window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
  const pref = localStorage.getItem("spino-theme") || "dark";
  if (pref === "system") applyTheme("system");
});

// ==================== TILE STATE ====================
const workspace = document.getElementById("workspace");
const sidebarNav = document.getElementById("sidebar-nav");
const sidebarWorkspace = document.getElementById("sidebar-workspace");
const sidebarModel = document.getElementById("sidebar-model");
const newChatBtn = document.getElementById("new-chat-btn");
const navSettingsBtn = document.getElementById("nav-settings");
const navWorkspacesBtn = document.getElementById("nav-workspaces");

let tiles = []; // { id, type, el, chatId?, state? }
let nextTileId = 1;
let focusedTileId = null;

let loadedSettings = {};
let cachedAgents = [];

// Per-chat streaming state keyed by tile id
const chatStreams = {};

// Track Leaflet map instances per tile for cleanup
const tileMaps = {}; // tileId -> [L.map, ...]

// ==================== UTILITIES ====================
function escapeHtml(text) {
  if (text == null) return "";
  const div = document.createElement("div");
  div.textContent = String(text);
  return div.innerHTML;
}

function field(object, camel, snake) {
  return object?.[camel] ?? object?.[snake];
}

function projectLabel(path) {
  if (!path) return "~/";
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] || path;
}

function renderMarkdown(text) {
  if (!text) return "";
  let html = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  // Extract code blocks before other processing
  const codeBlocks = [];
  html = html.replace(/```(\w*)\n([\s\S]*?)```/g, (_, lang, code) => {
    const idx = codeBlocks.length;
    codeBlocks.push(`<pre><code>${code}</code></pre>`);
    return `\x00CB${idx}\x00`;
  });
  html = html.replace(/`([^`]+)`/g, '<code>$1</code>');
  html = html
    .replace(/^### (.+)$/gm, '<h3>$1</h3>')
    .replace(/^## (.+)$/gm, '<h2>$1</h2>')
    .replace(/^# (.+)$/gm, '<h1>$1</h1>');
  // Wrap consecutive <li> runs into <ul> (non-greedy, per group)
  html = html
    .replace(/^\* (.+)$/gm, '<li>$1</li>');
  html = html.replace(/((?:<li>.*<\/li>\n?)+)/g, '<ul>$1</ul>');
  html = html
    .replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>')
    .replace(/\*(.+?)\*/g, '<em>$1</em>');
  html = html.replace(/\n/g, '<br>');
  // Restore code blocks (after <br> replacement so code stays clean)
  html = html.replace(/\x00CB(\d+)\x00/g, (_, idx) => codeBlocks[parseInt(idx)]);
  return html;
}

function feedTimeAgo(dateStr) {
  const now = new Date();
  const then = new Date(dateStr.replace(" ", "T") + "Z");
  const sec = Math.floor((now - then) / 1000);
  if (sec < 60) return "just now";
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const d = Math.floor(hr / 24);
  if (d < 7) return `${d}d ago`;
  return then.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function formatNextRun(isoStr) {
  try {
    const d = new Date(isoStr);
    if (Number.isNaN(d.getTime())) return isoStr;
    const now = new Date();
    const diff = d - now;
    if (diff < 0) return "overdue";
    if (diff < 60000) return "< 1 min";
    if (diff < 3600000) return `${Math.round(diff / 60000)}m`;
    if (diff < 86400000) return `${Math.round(diff / 3600000)}h`;
    return `${Math.round(diff / 86400000)}d`;
  } catch { return isoStr; }
}

function formatFileSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)} MB`;
  return `${(bytes / 1073741824).toFixed(1)} GB`;
}

function formatModifiedAt(unixStr) {
  if (!unixStr) return "";
  const d = new Date(Number(unixStr) * 1000);
  const now = new Date();
  const diff = now - d;
  if (diff < 60000) return "just now";
  if (diff < 3600000) return `${Math.round(diff / 60000)}m ago`;
  if (diff < 86400000) return `${Math.round(diff / 3600000)}h ago`;
  if (diff < 604800000) return `${Math.round(diff / 86400000)}d ago`;
  return d.toLocaleDateString();
}

function parseAllowedTools(input) {
  return input.split(",").map(p => p.trim()).filter(p => p.length > 0);
}

function threadLabel(item, index = 0) {
  const preview = field(item, "preview", "preview");
  const messageCount = field(item, "messageCount", "message_count") ?? 0;
  if (!preview || !preview.trim()) {
    return messageCount > 0 ? `Chat ${index + 1}` : "New chat";
  }
  const cleaned = preview.replace(/\s+/g, " ").replace(/^Tool:\s*/i, "").trim();
  if (!cleaned) return messageCount > 0 ? `Chat ${index + 1}` : "New chat";
  if (cleaned.length <= 34) return cleaned;
  return `${cleaned.slice(0, 34).trimEnd()}...`;
}

// ==================== TILE MANAGEMENT ====================
const TILE_TYPES = [
  { type: "chat",       label: "Chat",       dot: "green" },
  { type: "myFeed",     label: "My Feed",    dot: "amber" },
  { type: "feed",       label: "Feed",       dot: "amber" },
  { type: "tasks",      label: "Tasks",      dot: "blue"  },
  { type: "tools",      label: "Tools",      dot: "purple" },
  { type: "agents",     label: "Agents",     dot: "blue"  },
  { type: "memory",     label: "Memory",     dot: "purple" },
  { type: "content",    label: "Content",    dot: "off"   },
];

function updateWorkspaceGrid() {
  const count = tiles.length;
  if (count === 0) {
    workspace.style.gridTemplateColumns = "1fr";
  } else if (count === 1) {
    workspace.style.gridTemplateColumns = "1fr";
  } else if (count === 2) {
    workspace.style.gridTemplateColumns = "1fr 1fr";
  } else if (count === 3) {
    workspace.style.gridTemplateColumns = "1fr 1fr 1fr";
  } else {
    workspace.style.gridTemplateColumns = `repeat(${count}, 1fr)`;
  }
}

function focusTile(tileId) {
  focusedTileId = tileId;
  for (const t of tiles) {
    t.el.classList.toggle("focused", t.id === tileId);
  }
}

function addTile(type, opts = {}) {
  const id = nextTileId++;
  const tile = document.createElement("div");
  tile.className = "tile";
  tile.dataset.tileId = id;
  tile.dataset.tileType = type;

  const label = TILE_TYPES.find(t => t.type === type)?.label || type;
  const subtitle = opts.subtitle || "";

  tile.innerHTML = `
    <div class="tile-header">
      <span class="tile-title">${label}</span>
      ${subtitle ? `<span class="tile-subtitle">${subtitle}</span>` : ""}
      <span class="tile-spacer"></span>
      <div class="tile-actions">
        <button class="tile-btn pop" title="Pop out">&#x29C9;</button>
        <button class="tile-btn close" title="Close">&times;</button>
      </div>
    </div>
    <div class="tile-body"></div>
  `;

  tile.addEventListener("mousedown", () => focusTile(id));

  tile.querySelector(".tile-btn.pop").addEventListener("click", (e) => {
    e.stopPropagation();
    popOutTile(id);
  });

  tile.querySelector(".tile-btn.close").addEventListener("click", (e) => {
    e.stopPropagation();
    removeTile(id);
  });

  const tileObj = { id, type, el: tile, chatId: opts.chatId || null, projectPath: opts.projectPath || "" };
  tiles.push(tileObj);
  workspace.appendChild(tile);
  updateWorkspaceGrid();
  focusTile(id);
  updateSidebar();
  saveLayout();

  // Render content
  const body = tile.querySelector(".tile-body");
  renderTileContent(tileObj, body);

  return tileObj;
}

function removeTile(tileId) {
  const idx = tiles.findIndex(t => t.id === tileId);
  if (idx === -1) return;
  const tile = tiles[idx];
  tile.el.remove();
  tiles.splice(idx, 1);

  // Clean up chat streaming state
  if (tile.type === "chat") {
    delete chatStreams[tileId];
  }

  // Clean up Leaflet map instances
  if (tileMaps[tileId]) {
    for (const m of tileMaps[tileId]) {
      try { m.remove(); } catch {}
    }
    delete tileMaps[tileId];
  }

  if (focusedTileId === tileId) {
    focusedTileId = tiles.length > 0 ? tiles[tiles.length - 1].id : null;
    for (const t of tiles) {
      t.el.classList.toggle("focused", t.id === focusedTileId);
    }
  }

  updateWorkspaceGrid();
  updateSidebar();
  saveLayout();
}

function getTilesByType(type) {
  return tiles.filter(t => t.type === type);
}

function getActiveChatTile() {
  if (focusedTileId) {
    const focused = tiles.find(t => t.id === focusedTileId && t.type === "chat");
    if (focused) return focused;
  }
  const chats = getTilesByType("chat");
  return chats.length > 0 ? chats[chats.length - 1] : null;
}

function getChatTileBySessionId(sessionId) {
  if (!sessionId) return getActiveChatTile();
  const match = tiles.find(t => t.type === "chat" && chatStreams[t.id]?.sessionId === sessionId);
  return match || null;
}

// ==================== SIDEBAR ====================
function updateSidebar() {
  sidebarNav.innerHTML = "";

  for (const def of TILE_TYPES) {
    const openTiles = getTilesByType(def.type);
    const isOpen = openTiles.length > 0;

    const btn = document.createElement("button");
    btn.className = `nav-item${isOpen ? " open" : ""}`;
    btn.innerHTML = `
      <span class="nav-dot ${isOpen ? def.dot : "off"}"></span>
      ${def.label}
      ${openTiles.length > 1 ? `<span class="nav-count">${openTiles.length}</span>` : ""}
      <span class="nav-pip"></span>
      <button class="nav-add" title="Add ${def.label}">+</button>
    `;

    // Click label to focus existing or add new
    btn.addEventListener("click", (e) => {
      if (e.target.classList.contains("nav-add")) return;
      if (isOpen) {
        focusTile(openTiles[0].id);
      } else {
        openTileForType(def.type);
      }
    });

    // Click + to always add new
    const addBtn = btn.querySelector(".nav-add");
    addBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      openTileForType(def.type);
    });

    sidebarNav.appendChild(btn);
  }

  // Update static sidebar buttons
  const wsOpen = getTilesByType("workspaces").length > 0;
  navWorkspacesBtn.classList.toggle("open", wsOpen);
  const wsDot = navWorkspacesBtn.querySelector(".nav-dot");
  if (wsDot) wsDot.className = `nav-dot ${wsOpen ? "blue" : "off"}`;

  const settingsOpen = getTilesByType("settings").length > 0;
  navSettingsBtn.classList.toggle("open", settingsOpen);
  const settingsDot = navSettingsBtn.querySelector(".nav-dot");
  if (settingsDot) settingsDot.className = `nav-dot ${settingsOpen ? "purple" : "off"}`;
}

function openTileForType(type) {
  if (type === "chat") {
    addChatTile();
  } else {
    // For non-chat types, only allow one instance
    const existing = getTilesByType(type);
    if (existing.length > 0) {
      focusTile(existing[0].id);
      return;
    }
    addTile(type);
  }
}

// ==================== POP-OUT WINDOWS ====================
function popOutTile(tileId) {
  const tile = tiles.find(t => t.id === tileId);
  if (!tile) return;

  const params = new URLSearchParams();
  params.set("tile", tile.type);
  if (tile.chatId) params.set("chatId", tile.chatId);

  const label = `popout-${tile.type}-${Date.now()}`;
  const popoutUrl = `${window.location.origin}${window.location.pathname}?${params.toString()}`;
  const webview = new WebviewWindow(label, {
    url: popoutUrl,
    width: 700,
    height: 600,
    resizable: true,
    title: (TILE_TYPES.find(t => t.type === tile.type)?.label || tile.type) + " — Spino",
    decorations: false,
  });

  webview.once("tauri://created", () => {
    // Remove from main workspace after pop-out
    removeTile(tileId);
  });

  webview.once("tauri://error", (e) => {
    console.error("Pop-out error:", e);
  });
}

function isPopout() {
  return new URLSearchParams(window.location.search).has("tile");
}

function getPopoutParams() {
  const params = new URLSearchParams(window.location.search);
  return {
    type: params.get("tile"),
    chatId: params.get("chatId") || ""
  };
}

// ==================== LAYOUT PERSISTENCE ====================
function saveLayout() {
  const layout = tiles.map(t => ({ type: t.type, chatId: t.chatId, projectPath: t.projectPath || "" }));
  localStorage.setItem("spinosaur.layout", JSON.stringify(layout));
}

function restoreLayout() {
  try {
    const saved = JSON.parse(localStorage.getItem("spinosaur.layout"));
    if (saved && saved.length > 0) {
      return saved;
    }
  } catch {}
  return null;
}

// ==================== TILE RENDERERS ====================
function renderTileContent(tileObj, body) {
  switch (tileObj.type) {
    case "chat": renderChatTile(tileObj, body); break;
    case "myFeed": renderMyFeedTile(tileObj, body); break;
    case "feed": renderFeedTile(tileObj, body); break;
    case "tasks": renderTasksTile(tileObj, body); break;
    case "tools": renderToolsTile(tileObj, body); break;
    case "agents": renderAgentsTile(tileObj, body); break;
    case "memory": renderMemoryTile(tileObj, body); break;
    case "content": renderContentTile(tileObj, body); break;
    case "workspaces": renderWorkspacesTile(tileObj, body); break;
    case "settings": renderSettingsTile(tileObj, body); break;
  }
}

// ========== CHAT TILE ==========
async function addChatTile(existingSessionId, projectPath) {
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

async function addProjectChatTile(projectPath) {
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

async function refreshThreadSelect(tileObj) {
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

function renderChatTile(tileObj, body) {
  body.classList.add("chat-scroll");
  body.innerHTML = `
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
          addTurn(chatLog, "spino", "Stopping generation...");
        } catch (e) {
          addTurn(chatLog, "spino", `Error: ${String(e)}`);
        }
      } else {
        addTurn(chatLog, "spino", "No active generation to stop.");
      }
      input.value = "";
      autoResize(input);
      return;
    }

    // Handle /init command
    if (text.startsWith("/init")) {
      const initPath = text.replace(/^\/init\s*/, "").trim() || tileObj.projectPath;
      if (!initPath) {
        addTurn(chatLog, "spino", "Usage: /init or /init /path/to/project (use in a project chat or provide a path)");
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
        addTurn(chatLog, "spino", `spino.md saved to ${initPath}/spino.md`);
      } catch (e) {
        hideThinking(tileObj.id);
        addTurn(chatLog, "spino", `Error: ${String(e)}`);
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
          addTurn(chatLog, "spino", parsed.error);
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
        model: loadedSettings["llm/model"] || "MiniMax-M2.7"
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
      addTurn(chatLog, "spino", `Error: ${String(error)}`);
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
    const agentItems = cachedAgents
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

function autoResize(textarea) {
  textarea.style.height = "auto";
  textarea.style.height = `${Math.min(textarea.scrollHeight, 220)}px`;
}

function addTurn(chatLog, role, text) {
  const turn = document.createElement("div");
  turn.className = "turn";
  const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  turn.innerHTML = `
    <div class="turn-header">
      <span class="turn-label ${role}">${role === "you" ? "YOU" : "SPINO"}</span>
      <span class="turn-time">${time}</span>
    </div>
    <div class="turn-body">${renderMarkdown(text)}</div>
  `;
  chatLog.appendChild(turn);
  chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
}

function addToolCard(chatLog, toolName, args, status) {
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
    <span>${toolName}</span>
    ${argsPreview ? `<span class="target">${argsPreview}</span>` : ""}
    <span class="status-badge">${statusText}</span>
  `;

  chatLog.appendChild(card);
  chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
  return card;
}

function showThinking(tileId, label) {
  const stream = chatStreams[tileId];
  if (!stream) return;
  const chatLog = document.querySelector(`[data-chat-stream="${tileId}"]`);
  if (!chatLog) return;

  if (stream.thinkingEl) {
    stream.thinkingEl.querySelector(".thinking-dots").innerHTML =
      `${label || "Thinking"}<span>.</span><span>.</span><span>.</span>`;
    return;
  }

  stream.thinkingEl = document.createElement("div");
  stream.thinkingEl.className = "thinking-turn";
  stream.thinkingEl.innerHTML = `<span class="thinking-dots">${label || "Thinking"}<span>.</span><span>.</span><span>.</span></span>`;
  chatLog.appendChild(stream.thinkingEl);
  chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
}

function hideThinking(tileId) {
  const stream = chatStreams[tileId];
  if (!stream || !stream.thinkingEl) return;
  stream.thinkingEl.remove();
  stream.thinkingEl = null;
}

async function loadMessagesForTile(tileObj) {
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
        const role = item.role === "user" ? "you" : "spino";
        addTurn(chatLog, role, item.content);
      }
    }
    // Estimate context usage from loaded messages
    estimateContextFromMessages(tileObj.id, chatLog);
  } catch (e) {
    console.error("Failed to load messages:", e);
  }
}

function parseSlashCommand(text) {
  const match = text.match(/^\/(\S+)\s*(.*)/s);
  if (!match) return null;
  const command = match[1].toLowerCase();
  const query = match[2].trim();
  const agent = cachedAgents.find(
    a => (field(a, "slashCommand", "slash_command") || "").toLowerCase() === command && field(a, "enabled") !== false
  );
  if (!agent) return { error: `Unknown command /${command}` };
  return { agent, query: query || text };
}

// ========== MY FEED TILE ==========
function renderMyFeedTile(tileObj, body) {
  body.innerHTML = `
    <div class="myfeed-layout">
      <div class="myfeed-sidebar">
        <div class="myfeed-sidebar-header">
          <span class="myfeed-sidebar-title">Feeds</span>
          <button class="myfeed-add-btn tile-btn" data-myfeed-new="${tileObj.id}" title="New feed">+</button>
        </div>
        <div class="myfeed-feed-list" data-myfeed-list="${tileObj.id}"></div>
      </div>
      <div class="myfeed-main">
        <div class="myfeed-form" data-myfeed-form="${tileObj.id}">
          <div class="myfeed-form-title">New Feed</div>
          <input type="text" class="myfeed-input" placeholder="Feed name (e.g. Tech News)" data-myfeed-name="${tileObj.id}" />
          <input type="text" class="myfeed-input" placeholder="Topics (comma-separated, e.g. AI, Rust, startup)" data-myfeed-topics="${tileObj.id}" />
          <div class="myfeed-form-row">
            <label class="myfeed-label">Refresh every</label>
            <select class="myfeed-select" data-myfeed-interval="${tileObj.id}">
              <option value="15">15 min</option>
              <option value="30">30 min</option>
              <option value="60" selected>1 hour</option>
              <option value="180">3 hours</option>
              <option value="360">6 hours</option>
              <option value="1440">Daily</option>
            </select>
          </div>
          <div class="myfeed-form-actions">
            <button class="myfeed-btn primary" data-myfeed-save="${tileObj.id}">Create Feed</button>
            <button class="myfeed-btn" data-myfeed-cancel="${tileObj.id}">Cancel</button>
          </div>
          <div class="myfeed-form-error" data-myfeed-error="${tileObj.id}"></div>
        </div>
        <div class="myfeed-items-panel" data-myfeed-items="${tileObj.id}">
          <div class="myfeed-empty-state">
            <div class="myfeed-empty-icon">◈</div>
            <div class="myfeed-empty-text">Select a feed or create one with +</div>
          </div>
        </div>
      </div>
    </div>
  `;

  let selectedFeedId = null;
  const form = body.querySelector(`[data-myfeed-form="${tileObj.id}"]`);
  const feedList = body.querySelector(`[data-myfeed-list="${tileObj.id}"]`);
  const itemsPanel = body.querySelector(`[data-myfeed-items="${tileObj.id}"]`);
  const nameInput = body.querySelector(`[data-myfeed-name="${tileObj.id}"]`);
  const topicsInput = body.querySelector(`[data-myfeed-topics="${tileObj.id}"]`);
  const intervalSelect = body.querySelector(`[data-myfeed-interval="${tileObj.id}"]`);
  const errorEl = body.querySelector(`[data-myfeed-error="${tileObj.id}"]`);

  function showError(msg) {
    errorEl.textContent = msg;
    errorEl.style.display = msg ? "block" : "none";
  }

  body.querySelector(`[data-myfeed-new="${tileObj.id}"]`).addEventListener("click", () => {
    form.classList.add("visible");
    nameInput.value = "";
    topicsInput.value = "";
    showError("");
    nameInput.focus();
  });

  body.querySelector(`[data-myfeed-cancel="${tileObj.id}"]`).addEventListener("click", () => {
    form.classList.remove("visible");
    showError("");
  });

  body.querySelector(`[data-myfeed-save="${tileObj.id}"]`).addEventListener("click", async () => {
    const name = nameInput.value.trim();
    const topicsRaw = topicsInput.value.trim();
    if (!name) { showError("Feed name is required."); return; }
    if (!topicsRaw) { showError("Enter at least one topic."); return; }
    const topics = topicsRaw.split(",").map(t => t.trim()).filter(t => t.length > 0);
    const intervalMins = parseInt(intervalSelect.value, 10);
    showError("");
    try {
      await invoke("create_ai_feed", { req: { name, topics, refreshIntervalMins: intervalMins } });
      form.classList.remove("visible");
      await loadFeedList();
    } catch (err) {
      showError(String(err));
    }
  });

  async function loadFeedList() {
    let feeds;
    try { feeds = await invoke("get_ai_feeds"); } catch { feeds = []; }
    feedList.innerHTML = "";
    if (!feeds.length) {
      feedList.innerHTML = `<div class="myfeed-no-feeds">No feeds yet</div>`;
      return;
    }
    for (const feed of feeds) {
      const id = feed.id;
      const name = feed.name;
      const topics = (feed.topics || []).join(", ");
      const el = document.createElement("div");
      el.className = "myfeed-feed-item" + (id === selectedFeedId ? " selected" : "");
      el.dataset.feedId = id;
      el.innerHTML = `
        <div class="myfeed-feed-item-top">
          <span class="myfeed-feed-name">${escapeHtml(name)}</span>
          <button class="myfeed-feed-del tile-btn" data-del-id="${id}" title="Delete feed">&times;</button>
        </div>
        <div class="myfeed-feed-topics">${escapeHtml(topics)}</div>
      `;
      el.querySelector(`[data-del-id="${id}"]`).addEventListener("click", async (e) => {
        e.stopPropagation();
        await invoke("delete_ai_feed", { req: { id } });
        if (selectedFeedId === id) {
          selectedFeedId = null;
          showItemsEmpty();
        }
        await loadFeedList();
      });
      el.addEventListener("click", () => selectFeed(id, name));
      feedList.appendChild(el);
    }
  }

  function showItemsEmpty() {
    itemsPanel.innerHTML = `
      <div class="myfeed-empty-state">
        <div class="myfeed-empty-icon">◈</div>
        <div class="myfeed-empty-text">Select a feed or create one with +</div>
      </div>
    `;
  }

  async function selectFeed(id, name) {
    selectedFeedId = id;
    // Highlight selected
    feedList.querySelectorAll(".myfeed-feed-item").forEach(el => {
      el.classList.toggle("selected", Number(el.dataset.feedId) === id);
    });
    await loadFeedItems(id, name);
  }

  async function loadFeedItems(feedId, feedName) {
    itemsPanel.innerHTML = `<div class="myfeed-loading">Loading…</div>`;
    try {
      const items = await invoke("get_ai_feed_items", { req: { id: feedId } });
      renderFeedItems(feedId, feedName, items);
    } catch (err) {
      itemsPanel.innerHTML = `<div class="myfeed-error-msg">${escapeHtml(err)}</div>`;
    }
  }

  function renderFeedItems(feedId, feedName, items) {
    itemsPanel.innerHTML = "";

    const toolbar = document.createElement("div");
    toolbar.className = "myfeed-items-toolbar";
    toolbar.innerHTML = `
      <span class="myfeed-items-title">${escapeHtml(feedName)}</span>
      <span class="myfeed-spacer"></span>
      <button class="myfeed-btn myfeed-refresh-btn" data-refresh-id="${feedId}">&#8635; Refresh</button>
    `;
    toolbar.querySelector(`[data-refresh-id="${feedId}"]`).addEventListener("click", async (e) => {
      const btn = e.currentTarget;
      btn.textContent = "Refreshing…";
      btn.disabled = true;
      try {
        const result = await invoke("refresh_ai_feed", { req: { id: feedId } });
        const added = result.itemsAdded ?? result.items_added ?? 0;
        await loadFeedItems(feedId, feedName);
        // Re-get the toolbar button after re-render and show count briefly
        const newBtn = itemsPanel.querySelector(`[data-refresh-id="${feedId}"]`);
        if (newBtn) {
          newBtn.textContent = `+ ${added} added`;
          setTimeout(() => { if (newBtn) newBtn.textContent = "↻ Refresh"; }, 2500);
        }
      } catch (err) {
        btn.textContent = "↻ Refresh";
        btn.disabled = false;
        const errEl = document.createElement("div");
        errEl.className = "myfeed-error-msg";
        errEl.textContent = String(err);
        itemsPanel.insertBefore(errEl, itemsPanel.firstChild);
        setTimeout(() => errEl.remove(), 6000);
      }
    });
    itemsPanel.appendChild(toolbar);

    if (!items.length) {
      const empty = document.createElement("div");
      empty.className = "myfeed-empty-state";
      empty.innerHTML = `
        <div class="myfeed-empty-icon">◈</div>
        <div class="myfeed-empty-text">No items yet — click Refresh to fetch content</div>
      `;
      itemsPanel.appendChild(empty);
      return;
    }

    const list = document.createElement("div");
    list.className = "myfeed-cards";
    for (const item of items) {
      const id = item.id;
      const isRead = item.isRead ?? item.is_read ?? false;
      const title = item.title || "";
      const summary = item.summary || "";
      const url = item.url || "";
      const sourceName = item.sourceName ?? item.source_name ?? "";
      const createdAt = item.createdAt ?? item.created_at ?? "";

      const card = document.createElement("div");
      card.className = "myfeed-card" + (isRead ? " read" : "");
      card.innerHTML = `
        <div class="myfeed-card-header">
          <span class="myfeed-card-source">${escapeHtml(sourceName)}</span>
          <span class="myfeed-card-time">${feedTimeAgo(createdAt)}</span>
        </div>
        <div class="myfeed-card-title">${escapeHtml(title)}</div>
        ${summary ? `<div class="myfeed-card-summary">${escapeHtml(summary)}</div>` : ""}
        ${url ? `<a class="myfeed-card-link" href="${escapeHtml(url)}" target="_blank" rel="noopener">Read more ↗</a>` : ""}
      `;
      if (!isRead) {
        card.addEventListener("click", async () => {
          await invoke("mark_ai_feed_item_read", { req: { id } });
          card.classList.add("read");
        });
      }
      list.appendChild(card);
    }
    itemsPanel.appendChild(list);
  }

  loadFeedList();
}

// ========== FEED TILE ==========
function renderFeedTile(tileObj, body) {
  body.innerHTML = `
    <div class="feed-toolbar">
      <span class="feed-badge" data-feed-badge="${tileObj.id}">0 unread</span>
      <div class="feed-actions">
        <button class="tile-btn" data-feed-refresh="${tileObj.id}" title="Refresh">&#8635;</button>
        <button class="tile-btn" data-feed-read="${tileObj.id}" title="Mark all read">&#10003;</button>
        <button class="tile-btn" data-feed-clear="${tileObj.id}" title="Clear">&#8856;</button>
      </div>
    </div>
    <div class="feed-list" data-feed-list="${tileObj.id}"></div>
  `;

  body.querySelector(`[data-feed-refresh="${tileObj.id}"]`).addEventListener("click", () => refreshFeedTile(tileObj.id));
  body.querySelector(`[data-feed-read="${tileObj.id}"]`).addEventListener("click", async () => {
    await invoke("mark_all_feed_read");
    await refreshFeedTile(tileObj.id);
  });
  body.querySelector(`[data-feed-clear="${tileObj.id}"]`).addEventListener("click", async () => {
    await invoke("clear_feed_items");
    await refreshFeedTile(tileObj.id);
  });

  refreshFeedTile(tileObj.id);
}

async function refreshFeedTile(tileId) {
  const list = document.querySelector(`[data-feed-list="${tileId}"]`);
  const badge = document.querySelector(`[data-feed-badge="${tileId}"]`);
  if (!list) return;

  let items;
  try { items = await invoke("get_feed_items", { req: { limit: 200, unreadOnly: false } }); } catch { items = []; }
  const unread = items.filter(item => !field(item, "isRead", "is_read")).length;

  if (badge) {
    badge.textContent = `${unread} unread`;
    badge.classList.toggle("has-unread", unread > 0);
  }

  list.innerHTML = "";
  if (!items.length) {
    list.innerHTML = `<div class="feed-empty">No activity yet</div>`;
    return;
  }

  for (const item of items) {
    const id = field(item, "id", "id");
    const isRead = !!field(item, "isRead", "is_read");
    const kind = field(item, "kind", "kind") || "response";
    const title = field(item, "title", "title");
    const message = field(item, "message", "message");
    const createdAt = field(item, "createdAt", "created_at");

    const dotColor = kind === "tool" ? "blue" : kind === "message" ? "green" : "amber";
    const el = document.createElement("div");
    el.className = "feed-item";
    el.innerHTML = `
      <span class="feed-dot ${isRead ? "gray" : dotColor}"></span>
      <div class="feed-info">
        <strong>${escapeHtml(title)}</strong>
        <span>${escapeHtml(message)}</span>
        <span class="feed-time">${feedTimeAgo(createdAt)}</span>
      </div>
      <span class="feed-item-delete" title="Remove">&times;</span>
    `;

    if (!isRead) {
      el.addEventListener("click", async (e) => {
        if (e.target.classList.contains("feed-item-delete")) return;
        await invoke("mark_feed_item_read", { req: { id } });
        await refreshFeedTile(tileId);
      });
    }

    el.querySelector(".feed-item-delete").addEventListener("click", async (e) => {
      e.stopPropagation();
      try { await invoke("remove_feed_item", { req: { id } }); } catch {}
      await refreshFeedTile(tileId);
    });

    list.appendChild(el);
  }
}

// ========== TASKS TILE ==========
function renderTasksTile(tileObj, body) {
  body.innerHTML = `
    <div class="tasks-toolbar">
      <button class="tasks-filter active" data-filter="all">All</button>
      <button class="tasks-filter" data-filter="scheduled">Scheduled</button>
      <button class="tasks-filter" data-filter="running">Running</button>
      <button class="tasks-filter" data-filter="completed">Completed</button>
      <span class="tasks-spacer"></span>
      <button class="tasks-new" data-tasks-new="${tileObj.id}">+ New</button>
    </div>
    <div class="task-form" data-task-form="${tileObj.id}">
      <input type="text" placeholder="Task name" data-task-name="${tileObj.id}" />
      <textarea rows="2" placeholder="Description" data-task-desc="${tileObj.id}"></textarea>
      <div class="form-row">
        <select data-task-preset="${tileObj.id}">
          <option value="">Schedule preset</option>
          <option value="every 5 minutes">Every 5 min</option>
          <option value="every 30 minutes">Every 30 min</option>
          <option value="every 1 hour">Every hour</option>
          <option value="every 6 hours">Every 6 hours</option>
          <option value="every 24 hours">Daily</option>
        </select>
        <input type="text" placeholder="Custom schedule" data-task-schedule="${tileObj.id}" />
      </div>
      <input type="text" placeholder="Next run (ISO)" data-task-next="${tileObj.id}" />
      <div class="form-row">
        <button class="form-btn" data-task-save="${tileObj.id}">Save Task</button>
        <button class="form-btn" data-task-reset="${tileObj.id}">Cancel</button>
      </div>
    </div>
    <div class="tasks-list" data-tasks-list="${tileObj.id}"></div>
  `;

  let currentFilter = "all";
  let editingTaskId = null;

  const filters = body.querySelectorAll(".tasks-filter");
  filters.forEach(f => {
    f.addEventListener("click", () => {
      filters.forEach(x => x.classList.remove("active"));
      f.classList.add("active");
      currentFilter = f.dataset.filter;
      doRefresh();
    });
  });

  const form = body.querySelector(`[data-task-form="${tileObj.id}"]`);
  const newBtn = body.querySelector(`[data-tasks-new="${tileObj.id}"]`);
  const nameInput = body.querySelector(`[data-task-name="${tileObj.id}"]`);
  const descInput = body.querySelector(`[data-task-desc="${tileObj.id}"]`);
  const presetSelect = body.querySelector(`[data-task-preset="${tileObj.id}"]`);
  const scheduleInput = body.querySelector(`[data-task-schedule="${tileObj.id}"]`);
  const nextInput = body.querySelector(`[data-task-next="${tileObj.id}"]`);
  const saveBtn = body.querySelector(`[data-task-save="${tileObj.id}"]`);
  const resetBtn = body.querySelector(`[data-task-reset="${tileObj.id}"]`);

  presetSelect.addEventListener("change", () => {
    if (presetSelect.value) scheduleInput.value = presetSelect.value;
  });

  newBtn.addEventListener("click", () => {
    editingTaskId = null;
    nameInput.value = "";
    descInput.value = "";
    presetSelect.value = "";
    scheduleInput.value = "";
    nextInput.value = "";
    saveBtn.textContent = "Save Task";
    form.classList.add("visible");
  });

  resetBtn.addEventListener("click", () => {
    form.classList.remove("visible");
    editingTaskId = null;
  });

  saveBtn.addEventListener("click", async () => {
    const name = nameInput.value.trim();
    if (!name) return;
    try {
      if (editingTaskId) {
        await invoke("update_task", {
          req: { id: editingTaskId, name, description: descInput.value.trim(), schedule: scheduleInput.value.trim(), nextRun: nextInput.value.trim() }
        });
      } else {
        await invoke("add_task", {
          req: { name, description: descInput.value.trim(), schedule: scheduleInput.value.trim(), nextRun: nextInput.value.trim() }
        });
      }
      form.classList.remove("visible");
      editingTaskId = null;
      doRefresh();
    } catch (err) {
      console.error("Save task error:", err);
    }
  });

  async function doRefresh() {
    const list = body.querySelector(`[data-tasks-list="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_tasks"); } catch { items = []; }
    const filtered = currentFilter === "all" ? items : items.filter(i => field(i, "status", "status") === currentFilter);

    list.innerHTML = "";
    if (!filtered.length) {
      list.innerHTML = `<div class="feed-empty">No tasks</div>`;
      return;
    }

    for (const item of filtered) {
      const id = field(item, "id", "id");
      const name = field(item, "name", "name");
      const description = field(item, "description", "description");
      const schedule = field(item, "schedule", "schedule");
      const nextRun = field(item, "nextRun", "next_run");
      const status = field(item, "status", "status");

      const card = document.createElement("div");
      card.className = "task-card";
      card.innerHTML = `
        <div class="task-top">
          <span class="task-name">${escapeHtml(name)}</span>
          <span class="task-status ${escapeHtml(status)}">${escapeHtml(status)}</span>
        </div>
        ${description ? `<div class="task-desc">${escapeHtml(description)}</div>` : ""}
        <div class="task-meta">
          <span class="task-schedule">
            <span class="task-schedule-dot ${status === "running" ? "active" : "idle"}"></span>
            ${escapeHtml(schedule) || "No schedule"}
          </span>
          ${nextRun ? `<span>Next: ${formatNextRun(nextRun)}</span>` : ""}
        </div>
        <div class="task-actions">
          <select class="task-status-select" data-action="status">
            <option value="scheduled" ${status === "scheduled" ? "selected" : ""}>scheduled</option>
            <option value="running" ${status === "running" ? "selected" : ""}>running</option>
            <option value="completed" ${status === "completed" ? "selected" : ""}>completed</option>
          </select>
          <button class="task-action-btn" data-action="edit">Edit</button>
          <button class="task-action-btn danger" data-action="delete">Delete</button>
        </div>
      `;

      card.querySelector('[data-action="edit"]').addEventListener("click", () => {
        editingTaskId = id;
        nameInput.value = name;
        descInput.value = description || "";
        scheduleInput.value = schedule || "";
        nextInput.value = nextRun || "";
        saveBtn.textContent = "Update Task";
        form.classList.add("visible");
      });

      card.querySelector('[data-action="status"]').addEventListener("change", async (e) => {
        try { await invoke("set_task_status", { req: { id, status: e.target.value } }); } catch {}
        doRefresh();
      });

      card.querySelector('[data-action="delete"]').addEventListener("click", async () => {
        await invoke("remove_task", { req: { id } });
        doRefresh();
      });

      list.appendChild(card);
    }
  }

  doRefresh();
}

// ========== TOOLS TILE ==========
function renderToolsTile(tileObj, body) {
  body.innerHTML = `
    <div class="tools-search">
      <input type="text" placeholder="Search tools..." data-tools-search="${tileObj.id}" />
    </div>
    <div class="tools-list" data-tools-list="${tileObj.id}"></div>
  `;

  const searchInput = body.querySelector(`[data-tools-search="${tileObj.id}"]`);
  searchInput.addEventListener("input", () => doRefresh(searchInput.value.toLowerCase()));

  async function doRefresh(query = "") {
    const list = body.querySelector(`[data-tools-list="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_tools"); } catch { items = []; }
    const filtered = query ? items.filter(i => (field(i, "name", "name") || "").toLowerCase().includes(query)) : items;

    list.innerHTML = "";
    if (!filtered.length) {
      list.innerHTML = `<div class="feed-empty">No tools</div>`;
      return;
    }

    for (const item of filtered) {
      const toolId = field(item, "toolId", "tool_id");
      const name = field(item, "name", "name");
      const description = field(item, "description", "description");
      const enabled = !!field(item, "enabled", "enabled");

      const row = document.createElement("div");
      row.className = "tool-item";
      row.innerHTML = `
        <div class="tool-info">
          <div class="tool-name">${escapeHtml(name)}</div>
          <div class="tool-desc">${escapeHtml(description)}</div>
        </div>
        <label class="toggle-switch">
          <input type="checkbox" ${enabled ? "checked" : ""} />
          <span class="toggle-track"></span>
        </label>
      `;

      row.querySelector("input").addEventListener("change", async (e) => {
        await invoke("set_tool_enabled", { req: { toolId, enabled: e.target.checked } });
      });

      list.appendChild(row);
    }
  }

  doRefresh();
}

// ========== AGENTS TILE ==========
function renderAgentsTile(tileObj, body) {
  body.innerHTML = `
    <div class="agents-toolbar">
      <span class="agents-count" data-agents-count="${tileObj.id}">0 agents</span>
      <span class="agents-spacer"></span>
      <button class="agents-new" data-agents-new="${tileObj.id}">+ New</button>
    </div>
    <div class="agent-form" data-agent-form="${tileObj.id}">
      <input type="text" placeholder="Agent name" data-agent-name="${tileObj.id}" />
      <input type="text" placeholder="Slash command" data-agent-slash="${tileObj.id}" />
      <textarea rows="2" placeholder="Description" data-agent-desc="${tileObj.id}"></textarea>
      <textarea rows="3" placeholder="System prompt" data-agent-system="${tileObj.id}"></textarea>
      <div class="form-row">
        <input type="number" value="5" min="1" max="20" data-agent-iters="${tileObj.id}" style="width:80px" />
        <input type="text" placeholder="Allowed tools (comma-separated)" data-agent-tools="${tileObj.id}" />
      </div>
      <label class="checkbox-inline">
        <input type="checkbox" checked data-agent-enabled="${tileObj.id}" /> Enabled
      </label>
      <div class="form-row">
        <button class="form-btn" data-agent-save="${tileObj.id}">Save Agent</button>
        <button class="form-btn" data-agent-reset="${tileObj.id}">Cancel</button>
      </div>
    </div>
    <div class="agents-list" data-agents-list="${tileObj.id}"></div>
  `;

  let editingAgentId = null;
  const form = body.querySelector(`[data-agent-form="${tileObj.id}"]`);
  const newBtn = body.querySelector(`[data-agents-new="${tileObj.id}"]`);
  const nameInput = body.querySelector(`[data-agent-name="${tileObj.id}"]`);
  const slashInput = body.querySelector(`[data-agent-slash="${tileObj.id}"]`);
  const descInput = body.querySelector(`[data-agent-desc="${tileObj.id}"]`);
  const systemInput = body.querySelector(`[data-agent-system="${tileObj.id}"]`);
  const itersInput = body.querySelector(`[data-agent-iters="${tileObj.id}"]`);
  const toolsInput = body.querySelector(`[data-agent-tools="${tileObj.id}"]`);
  const enabledInput = body.querySelector(`[data-agent-enabled="${tileObj.id}"]`);
  const saveBtn = body.querySelector(`[data-agent-save="${tileObj.id}"]`);
  const resetBtn = body.querySelector(`[data-agent-reset="${tileObj.id}"]`);

  newBtn.addEventListener("click", () => {
    editingAgentId = null;
    nameInput.value = "";
    slashInput.value = "";
    descInput.value = "";
    systemInput.value = "";
    itersInput.value = "5";
    toolsInput.value = "";
    enabledInput.checked = true;
    saveBtn.textContent = "Save Agent";
    form.classList.add("visible");
  });

  resetBtn.addEventListener("click", () => {
    form.classList.remove("visible");
    editingAgentId = null;
  });

  saveBtn.addEventListener("click", async () => {
    const name = nameInput.value.trim();
    if (!name) return;
    const payload = {
      name,
      description: descInput.value.trim(),
      slashCommand: slashInput.value.trim(),
      systemPrompt: systemInput.value.trim(),
      maxIterations: parseInt(itersInput.value || "5", 10),
      enabled: enabledInput.checked,
      allowedTools: parseAllowedTools(toolsInput.value)
    };
    if (editingAgentId) {
      await invoke("update_agent", { req: { agentId: editingAgentId, ...payload } });
    } else {
      await invoke("add_agent", { req: payload });
    }
    form.classList.remove("visible");
    editingAgentId = null;
    doRefresh();
  });

  async function doRefresh() {
    const list = body.querySelector(`[data-agents-list="${tileObj.id}"]`);
    const countEl = body.querySelector(`[data-agents-count="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_agents"); } catch { items = []; }
    cachedAgents = items;

    countEl.textContent = `${items.length} agent${items.length !== 1 ? "s" : ""}`;
    list.innerHTML = "";

    if (!items.length) {
      list.innerHTML = `<div class="feed-empty">No agents configured</div>`;
      return;
    }

    for (const item of items) {
      const agentId = field(item, "agentId", "agent_id");
      const name = field(item, "name", "name");
      const description = field(item, "description", "description");
      const slash = field(item, "slashCommand", "slash_command");
      const maxIterations = field(item, "maxIterations", "max_iterations");
      const enabled = !!field(item, "enabled", "enabled");
      const toolsJson = field(item, "allowedToolsJson", "allowed_tools_json") || "[]";
      let tools = [];
      try { tools = JSON.parse(toolsJson); } catch { tools = []; }

      const card = document.createElement("div");
      card.className = `agent-card${enabled ? "" : " disabled"}`;

      const toolTags = tools.length
        ? tools.map(t => `<span class="agent-tag tool">${escapeHtml(t)}</span>`).join("")
        : `<span class="agent-tag">no tools</span>`;

      card.innerHTML = `
        <div class="agent-top">
          <div class="agent-name">
            ${escapeHtml(name)}
            ${slash ? `<span class="agent-slash">/${escapeHtml(slash)}</span>` : ""}
          </div>
          <label class="toggle-switch">
            <input type="checkbox" ${enabled ? "checked" : ""} />
            <span class="toggle-track"></span>
          </label>
        </div>
        <div class="agent-desc">${escapeHtml(description) || "No description"}</div>
        <div class="agent-meta">
          ${toolTags}
          <span class="agent-iters">${maxIterations} iters</span>
        </div>
        <div class="agent-actions">
          <button class="task-action-btn" data-action="edit">Edit</button>
          <button class="task-action-btn danger" data-action="delete">Delete</button>
        </div>
      `;

      card.querySelector("input[type=checkbox]").addEventListener("change", async (e) => {
        await invoke("set_agent_enabled", { req: { agentId, enabled: e.target.checked } });
        doRefresh();
      });

      card.querySelector('[data-action="edit"]').addEventListener("click", () => {
        editingAgentId = agentId;
        nameInput.value = name;
        slashInput.value = slash || "";
        descInput.value = description || "";
        systemInput.value = field(item, "systemPrompt", "system_prompt") || "";
        itersInput.value = String(maxIterations || 5);
        toolsInput.value = tools.join(", ");
        enabledInput.checked = enabled;
        saveBtn.textContent = "Update Agent";
        form.classList.add("visible");
      });

      card.querySelector('[data-action="delete"]').addEventListener("click", async () => {
        await invoke("remove_agent", { req: { agentId } });
        doRefresh();
      });

      list.appendChild(card);
    }
  }

  doRefresh();
}

// ========== MEMORY TILE ==========
function renderMemoryTile(tileObj, body) {
  body.innerHTML = `
    <div class="memory-tabs">
      <button class="memory-tab active" data-mtab="profile">Profile</button>
      <button class="memory-tab" data-mtab="entities">Entities</button>
      <button class="memory-tab" data-mtab="timeline">Timeline</button>
      <button class="memory-tab" data-mtab="history">History</button>
    </div>
    <div data-memory-view="profile" class="profile-section">
      <div class="profile-field">
        <div class="profile-label">NAME</div>
        <input type="text" data-mem-name="${tileObj.id}" />
      </div>
      <div class="profile-field">
        <div class="profile-label">ABOUT</div>
        <textarea rows="2" data-mem-about="${tileObj.id}"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">IDENTITY</div>
        <textarea rows="2" data-mem-identity="${tileObj.id}" placeholder="Role, job, background"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">PERSONALITY</div>
        <textarea rows="2" data-mem-personality="${tileObj.id}" placeholder="Communication style, temperament"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">GOALS</div>
        <textarea rows="2" data-mem-goals="${tileObj.id}" placeholder="What you're working toward"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">PREFERENCES</div>
        <textarea rows="2" data-mem-preferences="${tileObj.id}" placeholder="Likes, dislikes, tool preferences"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">CURRENT FOCUS</div>
        <textarea rows="2" data-mem-currentstate="${tileObj.id}" placeholder="What you're currently doing"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">PATTERNS</div>
        <textarea rows="2" data-mem-patterns="${tileObj.id}" placeholder="Recurring behaviors or habits"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">NOTES</div>
        <textarea rows="3" data-mem-profile="${tileObj.id}" placeholder="Additional notes"></textarea>
      </div>
      <button class="form-btn" data-mem-save="${tileObj.id}">Save Profile</button>
    </div>
    <div data-memory-view="entities" class="hidden" style="padding:6px;">
      <div style="padding:6px 6px 0;">
        <div class="form-row" style="margin-bottom:8px;">
          <input type="text" placeholder="Kind" data-ent-kind="${tileObj.id}" style="width:100px" />
          <input type="text" placeholder="Name" data-ent-name="${tileObj.id}" />
        </div>
        <input type="text" placeholder="Aliases (comma-separated)" data-ent-aliases="${tileObj.id}" style="margin-bottom:6px;" />
        <textarea rows="2" placeholder="Summary" data-ent-summary="${tileObj.id}" style="margin-bottom:6px;"></textarea>
        <div class="form-row" style="margin-bottom:8px;">
          <button class="form-btn" data-ent-save="${tileObj.id}">Save Entity</button>
          <button class="form-btn" data-ent-reset="${tileObj.id}">Reset</button>
        </div>
      </div>
      <div class="memory-section-label">RELATIONSHIPS</div>
      <div class="memory-list" data-relationships-list="${tileObj.id}"></div>
      <div class="memory-section-label">ENTITIES</div>
      <div class="memory-list" data-entities-list="${tileObj.id}"></div>
    </div>
    <div data-memory-view="timeline" class="hidden" style="padding:6px;">
      <div style="padding:6px 6px 0;">
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Description" data-ep-desc="${tileObj.id}" />
          <input type="text" placeholder="Timestamp" data-ep-time="${tileObj.id}" style="width:140px" />
          <button class="form-btn" data-ep-add="${tileObj.id}">Add</button>
        </div>
      </div>
      <div class="memory-list" data-episodes-list="${tileObj.id}"></div>
    </div>
    <div data-memory-view="history" class="hidden" style="padding:6px;">
      <div style="padding:6px 6px 0;">
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Search messages..." data-hist-search="${tileObj.id}" />
          <button class="form-btn" data-hist-go="${tileObj.id}">Search</button>
        </div>
      </div>
      <div class="memory-list" data-history-list="${tileObj.id}"></div>
      <div class="memory-list hidden" data-history-detail="${tileObj.id}" style="border-top:1px solid var(--border);"></div>
    </div>
  `;

  let activeTab = "profile";
  let editingEntityId = null;

  const tabs = body.querySelectorAll(".memory-tab");
  const views = {
    profile: body.querySelector('[data-memory-view="profile"]'),
    entities: body.querySelector('[data-memory-view="entities"]'),
    timeline: body.querySelector('[data-memory-view="timeline"]'),
    history: body.querySelector('[data-memory-view="history"]')
  };

  tabs.forEach(tab => {
    tab.addEventListener("click", () => {
      activeTab = tab.dataset.mtab;
      tabs.forEach(t => t.classList.toggle("active", t.dataset.mtab === activeTab));
      Object.entries(views).forEach(([k, v]) => v.classList.toggle("hidden", k !== activeTab));
    });
  });

  // Profile
  const memName = body.querySelector(`[data-mem-name="${tileObj.id}"]`);
  const memAbout = body.querySelector(`[data-mem-about="${tileObj.id}"]`);
  const memProfile = body.querySelector(`[data-mem-profile="${tileObj.id}"]`);
  const memIdentity = body.querySelector(`[data-mem-identity="${tileObj.id}"]`);
  const memPersonality = body.querySelector(`[data-mem-personality="${tileObj.id}"]`);
  const memGoals = body.querySelector(`[data-mem-goals="${tileObj.id}"]`);
  const memPreferences = body.querySelector(`[data-mem-preferences="${tileObj.id}"]`);
  const memCurrentState = body.querySelector(`[data-mem-currentstate="${tileObj.id}"]`);
  const memPatterns = body.querySelector(`[data-mem-patterns="${tileObj.id}"]`);

  body.querySelector(`[data-mem-save="${tileObj.id}"]`).addEventListener("click", async () => {
    await invoke("set_memory_profile", {
      req: {
        userName: memName.value.trim(),
        userAbout: memAbout.value.trim(),
        profileText: memProfile.value.trim(),
        identity: memIdentity.value.trim(),
        personality: memPersonality.value.trim(),
        goals: memGoals.value.trim(),
        preferences: memPreferences.value.trim(),
        currentState: memCurrentState.value.trim(),
        patterns: memPatterns.value.trim(),
      }
    });
    refreshProfile();
  });

  async function refreshProfile() {
    let profile;
    try { profile = await invoke("get_memory_profile"); } catch { return; }
    memName.value = field(profile, "userName", "user_name") || "";
    memAbout.value = field(profile, "userAbout", "user_about") || "";
    memProfile.value = field(profile, "profileText", "profile_text") || "";
    memIdentity.value = field(profile, "identity", "identity") || "";
    memPersonality.value = field(profile, "personality", "personality") || "";
    memGoals.value = field(profile, "goals", "goals") || "";
    memPreferences.value = field(profile, "preferences", "preferences") || "";
    memCurrentState.value = field(profile, "currentState", "current_state") || "";
    memPatterns.value = field(profile, "patterns", "patterns") || "";
  }

  // Entities
  const entKind = body.querySelector(`[data-ent-kind="${tileObj.id}"]`);
  const entName = body.querySelector(`[data-ent-name="${tileObj.id}"]`);
  const entSummary = body.querySelector(`[data-ent-summary="${tileObj.id}"]`);
  const entAliases = body.querySelector(`[data-ent-aliases="${tileObj.id}"]`);

  body.querySelector(`[data-ent-save="${tileObj.id}"]`).addEventListener("click", async () => {
    const kind = entKind.value.trim();
    const name = entName.value.trim();
    if (!kind || !name) return;
    const aliases = entAliases.value.trim() ? entAliases.value.split(",").map(a => a.trim()).filter(Boolean) : [];
    await invoke("upsert_entity", { req: { id: editingEntityId, kind, name, summary: entSummary.value.trim(), aliases } });
    editingEntityId = null;
    entKind.value = "";
    entName.value = "";
    entSummary.value = "";
    entAliases.value = "";
    refreshEntities();
    refreshRelationships();
  });

  body.querySelector(`[data-ent-reset="${tileObj.id}"]`).addEventListener("click", () => {
    editingEntityId = null;
    entKind.value = "";
    entName.value = "";
    entSummary.value = "";
    entAliases.value = "";
  });

  async function refreshEntities() {
    const list = body.querySelector(`[data-entities-list="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_entities"); } catch { items = []; }
    list.innerHTML = "";

    if (!items.length) {
      list.innerHTML = `<div class="feed-empty">No entities yet</div>`;
      return;
    }

    for (const item of items) {
      const id = field(item, "id", "id");
      const kind = field(item, "kind", "kind");
      const name = field(item, "name", "name");
      const summary = field(item, "summary", "summary");
      const aliases = field(item, "aliases", "aliases") || [];

      const kindMap = { person: "person", place: "place", project: "project", organization: "organization", concept: "concept", tool: "tool", thing: "concept" };
      const kindClass = kindMap[kind] || "concept";

      const card = document.createElement("div");
      card.className = "entity-card";
      card.innerHTML = `
        <div class="entity-top">
          <span class="entity-kind ${kindClass}">${escapeHtml(kind)}</span>
          <span class="entity-name">${escapeHtml(name)}</span>
          <span class="entity-delete" title="Delete entity">&times;</span>
        </div>
        ${aliases.length ? `<div class="entity-aliases">aka ${escapeHtml(aliases.join(", "))}</div>` : ""}
        <div class="entity-summary">${escapeHtml(summary) || "No summary"}</div>
      `;

      card.querySelector(".entity-delete").addEventListener("click", async (e) => {
        e.stopPropagation();
        await invoke("delete_entity", { req: { id } });
        refreshEntities();
        refreshRelationships();
      });

      card.addEventListener("click", (e) => {
        if (e.target.classList.contains("entity-delete")) return;
        editingEntityId = id;
        entKind.value = kind;
        entName.value = name;
        entSummary.value = summary || "";
        entAliases.value = aliases.join(", ");
      });

      list.appendChild(card);
    }
  }

  // Relationships
  async function refreshRelationships() {
    const list = body.querySelector(`[data-relationships-list="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_relationships"); } catch { items = []; }
    list.innerHTML = "";

    if (!items.length) {
      list.innerHTML = `<div class="feed-empty">No relationships yet</div>`;
      return;
    }

    for (const item of items) {
      const id = field(item, "id", "id");
      const fromName = field(item, "fromName", "from_name");
      const toName = field(item, "toName", "to_name");
      const label = field(item, "label", "label");

      const el = document.createElement("div");
      el.className = "relationship-card";
      el.innerHTML = `
        <span class="rel-from">${escapeHtml(fromName)}</span>
        <span class="rel-arrow">&rarr;</span>
        <span class="rel-label">${escapeHtml(label)}</span>
        <span class="rel-arrow">&rarr;</span>
        <span class="rel-to">${escapeHtml(toName)}</span>
        <span class="entity-delete" title="Delete">&times;</span>
      `;
      el.querySelector(".entity-delete").addEventListener("click", async () => {
        await invoke("delete_relationship", { req: { id } });
        refreshRelationships();
      });
      list.appendChild(el);
    }
  }

  // Episodes
  const epDesc = body.querySelector(`[data-ep-desc="${tileObj.id}"]`);
  const epTime = body.querySelector(`[data-ep-time="${tileObj.id}"]`);

  body.querySelector(`[data-ep-add="${tileObj.id}"]`).addEventListener("click", async () => {
    const description = epDesc.value.trim();
    if (!description) return;
    await invoke("add_episode", { req: { description, timestamp: epTime.value.trim() } });
    epDesc.value = "";
    epTime.value = "";
    refreshEpisodes();
  });

  async function refreshEpisodes() {
    const list = body.querySelector(`[data-episodes-list="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_episodes"); } catch { items = []; }
    list.innerHTML = "";

    if (!items.length) {
      list.innerHTML = `<div class="feed-empty">No timeline events yet</div>`;
      return;
    }

    for (const item of items) {
      const id = field(item, "id", "id");
      const description = field(item, "description", "description");
      const timestamp = field(item, "timestamp", "timestamp");
      const createdAt = field(item, "createdAt", "created_at");
      const entityNames = field(item, "entityNames", "entity_names") || [];

      const el = document.createElement("div");
      el.className = "entity-card";
      el.innerHTML = `
        <div class="entity-top">
          <span class="entity-name">${escapeHtml(description)}</span>
          <span class="entity-delete" title="Delete">&times;</span>
        </div>
        <div class="entity-summary">${escapeHtml(timestamp || createdAt)}</div>
        ${entityNames.length ? `<div class="episode-entities">${entityNames.map(n => `<span class="episode-entity-tag">${escapeHtml(n)}</span>`).join("")}</div>` : ""}
      `;
      el.querySelector(".entity-delete").addEventListener("click", async (e) => {
        e.stopPropagation();
        await invoke("delete_episode", { req: { id } });
        refreshEpisodes();
      });
      list.appendChild(el);
    }
  }

  // History
  const histSearch = body.querySelector(`[data-hist-search="${tileObj.id}"]`);
  const histList = body.querySelector(`[data-history-list="${tileObj.id}"]`);
  const histDetail = body.querySelector(`[data-history-detail="${tileObj.id}"]`);

  body.querySelector(`[data-hist-go="${tileObj.id}"]`).addEventListener("click", () => refreshHistory());
  histSearch.addEventListener("keydown", (e) => { if (e.key === "Enter") refreshHistory(); });

  async function refreshHistory() {
    const query = histSearch.value.trim();
    histDetail.classList.add("hidden");
    histDetail.innerHTML = "";

    let sessions;
    try {
      if (query) {
        sessions = await invoke("search_messages", { req: { query, limit: 30 } });
      } else {
        sessions = await invoke("get_sessions", { req: { limit: 30 } });
      }
    } catch { sessions = []; }

    histList.innerHTML = "";
    if (!sessions.length) {
      histList.innerHTML = `<div class="feed-empty">${query ? "No matches" : "No conversations yet"}</div>`;
      return;
    }

    for (const session of sessions) {
      const sid = field(session, "id", "id");
      const preview = field(session, "preview", "preview") || "";
      const msgCount = field(session, "messageCount", "message_count") || 0;
      const lastAt = field(session, "lastMessageAt", "last_message_at") || field(session, "createdAt", "created_at") || "";

      const row = document.createElement("div");
      row.className = "entity-card";
      row.innerHTML = `
        <div class="entity-top">
          <span class="entity-name">${escapeHtml(preview.substring(0, 80)) || "Empty session"}</span>
          <span class="entity-delete" title="Delete session">&times;</span>
        </div>
        <div class="entity-summary">${lastAt} &middot; ${msgCount} msgs</div>
      `;

      row.querySelector(".entity-delete").addEventListener("click", async (e) => {
        e.stopPropagation();
        try { await invoke("delete_session", { sessionId: sid }); } catch {}
        refreshHistory();
      });

      row.addEventListener("click", async (e) => {
        if (e.target.classList.contains("entity-delete")) return;
        const messages = await invoke("get_messages", { sessionId: sid });
        histDetail.classList.remove("hidden");
        histDetail.innerHTML = "";
        for (const msg of messages) {
          const role = field(msg, "role", "role");
          const content = field(msg, "content", "content") || "";
          const el = document.createElement("div");
          el.className = `history-msg history-msg--${role}`;
          el.innerHTML = `<div class="history-role">${escapeHtml(role)}</div><div class="history-content">${escapeHtml(content.substring(0, 500))}${content.length > 500 ? "..." : ""}</div>`;
          histDetail.appendChild(el);
        }
      });

      histList.appendChild(row);
    }
  }

  refreshProfile();
  refreshEntities();
  refreshRelationships();
  refreshEpisodes();
  refreshHistory();
}

// ========== CONTENT TILE ==========
const KIND_ICONS = { image: "🖼", audio: "🎵", video: "🎬", pdf: "📄", text: "📝", file: "📎" };

function renderContentTile(tileObj, body) {
  body.innerHTML = `
    <div class="content-toolbar">
      <select data-content-filter="${tileObj.id}">
        <option value="all">All</option>
        <option value="image">Images</option>
        <option value="audio">Audio</option>
        <option value="video">Video</option>
        <option value="pdf">PDF</option>
        <option value="text">Text</option>
      </select>
      <span class="tasks-spacer"></span>
      <span class="muted" data-content-path="${tileObj.id}" style="font-size:10px;"></span>
      <button class="tile-btn" data-content-open="${tileObj.id}" title="Open folder">&#128193;</button>
      <button class="tile-btn" data-content-refresh="${tileObj.id}" title="Refresh">&#8635;</button>
    </div>
    <div class="content-list" data-content-list="${tileObj.id}"></div>
  `;

  const filterSelect = body.querySelector(`[data-content-filter="${tileObj.id}"]`);
  const listEl = body.querySelector(`[data-content-list="${tileObj.id}"]`);
  filterSelect.addEventListener("change", () => doRefresh());

  body.querySelector(`[data-content-open="${tileObj.id}"]`).addEventListener("click", async () => {
    try { await invoke("open_content_dir"); } catch {}
  });

  body.querySelector(`[data-content-refresh="${tileObj.id}"]`).addEventListener("click", () => doRefresh());

  // Drag-and-drop
  listEl.addEventListener("dragover", (e) => { e.preventDefault(); listEl.classList.add("drag-over"); });
  listEl.addEventListener("dragleave", () => listEl.classList.remove("drag-over"));
  listEl.addEventListener("drop", (e) => { e.preventDefault(); listEl.classList.remove("drag-over"); });

  async function doRefresh() {
    const pathLabel = body.querySelector(`[data-content-path="${tileObj.id}"]`);

    try {
      const paths = await invoke("get_content_path");
      const dir = field(paths, "contentDir", "content_dir");
      pathLabel.textContent = dir;
    } catch {}

    let items;
    try { items = await invoke("get_content_files"); } catch { items = []; }

    const filter = filterSelect.value;
    const filtered = filter === "all" ? items : items.filter(i => field(i, "kind", "kind") === filter);

    listEl.innerHTML = "";
    if (!filtered.length) {
      listEl.innerHTML = `<div class="feed-empty">${filter === "all" ? "Drop files here or add to content folder" : `No ${filter} files`}</div>`;
      return;
    }

    for (const item of filtered) {
      const name = field(item, "name", "name");
      const kind = field(item, "kind", "kind");
      const size = field(item, "size", "size");
      const path = field(item, "path", "path");
      const modifiedAt = field(item, "modifiedAt", "modified_at");

      const row = document.createElement("div");
      row.className = "content-row";

      // Thumbnail or icon
      let thumbHtml;
      if (kind === "image") {
        const src = convertFileSrc(path);
        thumbHtml = `<img class="content-thumb" src="${src}" loading="lazy" />`;
      } else {
        thumbHtml = `<div class="content-icon">${KIND_ICONS[kind] || KIND_ICONS.file}</div>`;
      }

      row.innerHTML = `
        ${thumbHtml}
        <div class="content-meta">
          <strong>${name} <span class="content-badge content-badge--${kind}">${kind}</span></strong>
          <span class="muted">${formatFileSize(size)} · ${formatModifiedAt(modifiedAt)}</span>
        </div>
        <button class="task-action-btn danger">Delete</button>
      `;

      // Preview on row click (but not on delete button)
      row.addEventListener("click", (e) => {
        if (e.target.closest("button")) return;
        showContentPreview(item);
      });

      row.querySelector("button").addEventListener("click", async (e) => {
        e.stopPropagation();
        await invoke("delete_content_file", { req: { kind, name } });
        doRefresh();
      });

      listEl.appendChild(row);
    }
  }

  doRefresh();
}

function showContentPreview(item) {
  const name = field(item, "name", "name");
  const kind = field(item, "kind", "kind");
  const path = field(item, "path", "path");

  const overlay = document.createElement("div");
  overlay.className = "content-preview-overlay";

  function escHandler(e) {
    if (e.key === "Escape") close();
  }

  const close = () => {
    document.removeEventListener("keydown", escHandler);
    overlay.remove();
  };

  overlay.innerHTML = `
    <div class="content-preview-header">
      <span>${escapeHtml(name)}</span>
      <div style="display:flex;gap:6px;">
        <button data-preview-open>Open in App</button>
        <button data-preview-close>Close</button>
      </div>
    </div>
    <div class="content-preview-body"></div>
  `;

  const previewBody = overlay.querySelector(".content-preview-body");
  overlay.querySelector("[data-preview-close]").addEventListener("click", close);
  overlay.querySelector("[data-preview-open]").addEventListener("click", async () => {
    try { await invoke("open_content_file", { req: { path } }); } catch {}
  });
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) close();
  });

  if (kind === "image") {
    const img = document.createElement("img");
    img.src = convertFileSrc(path);
    previewBody.appendChild(img);
  } else if (kind === "audio") {
    const audio = document.createElement("audio");
    audio.controls = true;
    audio.src = convertFileSrc(path);
    previewBody.appendChild(audio);
  } else if (kind === "video") {
    const video = document.createElement("video");
    video.controls = true;
    video.src = convertFileSrc(path);
    previewBody.appendChild(video);
  } else if (kind === "text") {
    const pre = document.createElement("pre");
    pre.textContent = "Loading...";
    previewBody.appendChild(pre);
    invoke("read_content_text", { req: { path } }).then(resp => {
      pre.textContent = field(resp, "content", "content");
    }).catch(() => { pre.textContent = "Failed to load file."; });
  } else {
    previewBody.innerHTML = `<div style="color:var(--muted);text-align:center;padding:40px;">No preview available — use "Open in App" to view this file.</div>`;
  }

  document.addEventListener("keydown", escHandler);

  document.body.appendChild(overlay);
}

// ========== WORKSPACES TILE ==========
function renderWorkspacesTile(tileObj, body) {
  body.innerHTML = `
    <div class="ws-toolbar">
      <button class="form-btn" data-ws-browse="${tileObj.id}">+ Add Project Folder</button>
    </div>
    <div class="ws-list" data-ws-list="${tileObj.id}"></div>
  `;

  const browseBtn = body.querySelector(`[data-ws-browse="${tileObj.id}"]`);
  const list = body.querySelector(`[data-ws-list="${tileObj.id}"]`);

  async function refresh() {
    try {
      const projects = await invoke("get_projects");
      list.innerHTML = "";
      if (projects.length === 0) {
        list.innerHTML = '<div class="feed-empty">No projects registered</div>';
        return;
      }
      for (const p of projects) {
        const path = field(p, "path", "path");
        const name = field(p, "name", "name") || projectLabel(path);
        const spinoMd = field(p, "spinoMd", "spino_md") || "";
        const lastOpened = field(p, "lastOpenedAt", "last_opened_at") || "";
        const row = document.createElement("div");
        row.className = "ws-row";
        row.innerHTML = `
          <div class="ws-info">
            <strong class="ws-name">${escapeHtml(name)}</strong>
            <span class="ws-path">${escapeHtml(path)}</span>
            ${spinoMd ? '<span class="ws-has-spino">spino.md</span>' : ""}
          </div>
          <div class="ws-actions">
            <button class="task-action-btn ws-open">Open</button>
            <button class="task-action-btn ws-init">Init</button>
            <button class="task-action-btn danger ws-remove">&times;</button>
          </div>
        `;
        row.querySelector(".ws-open").addEventListener("click", () => addProjectChatTile(path));
        row.querySelector(".ws-init").addEventListener("click", async () => {
          row.querySelector(".ws-init").textContent = "...";
          try {
            await invoke("init_project", { req: { path } });
            row.querySelector(".ws-init").textContent = "Done";
            await refresh();
          } catch (e) {
            row.querySelector(".ws-init").textContent = "Err";
            console.error("init_project error:", e);
          }
        });
        row.querySelector(".ws-remove").addEventListener("click", async () => {
          try {
            await invoke("remove_project", { req: { path } });
            await refresh();
          } catch (e) {
            console.error("remove_project error:", e);
          }
        });
        list.appendChild(row);
      }
    } catch (e) {
      list.innerHTML = `<div class="feed-empty">Error loading projects</div>`;
      console.error("get_projects error:", e);
    }
  }

  browseBtn.addEventListener("click", async () => {
    const selected = await openDialog({
      directory: true,
      multiple: false,
      title: "Select Project Folder",
    });
    if (!selected) return;
    try {
      await invoke("add_project", { req: { path: selected } });
      await refresh();
    } catch (e) {
      console.error("add_project error:", e);
    }
  });

  refresh();
}

// ========== SETTINGS TILE ==========
function renderSettingsTile(tileObj, body) {
  body.innerHTML = `
    <div class="settings-section">
      <div class="settings-group">
        <h3>Profile</h3>
        <input type="text" placeholder="Your name" data-s-name="${tileObj.id}" style="margin-bottom:6px;" />
        <textarea rows="2" placeholder="About you" data-s-about="${tileObj.id}"></textarea>
      </div>
      <div class="settings-group">
        <h3>Model</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Model name (e.g. MiniMax-M2.7)" data-s-model="${tileObj.id}" style="flex:1;" />
        </div>
      </div>
      <div class="settings-group">
        <h3>API Keys</h3>
        <input type="password" placeholder="OpenAI API key" data-s-openai="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="MiniMax API key" data-s-minimax="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="Web search API key" data-s-search="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="Together AI key (images)" data-s-together="${tileObj.id}" style="margin-bottom:6px;" />
      </div>
      <div class="settings-group">
        <h3>Image Generation</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <select data-s-imgprov="${tileObj.id}">
            <option value="">None</option>
            <option value="together">Together</option>
            <option value="openai">OpenAI</option>
          </select>
          <select data-s-imgmodel="${tileObj.id}" style="flex:1;"></select>
          <button class="form-btn" data-s-fetch-img-models="${tileObj.id}">Fetch</button>
        </div>
        <span class="muted" data-s-img-status="${tileObj.id}" style="font-size:10px;"></span>
      </div>
      <div class="settings-group">
        <h3>Workspace</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Workspace path" data-s-workspace="${tileObj.id}" />
          <button class="form-btn" data-s-setws="${tileObj.id}">Set</button>
          <button class="form-btn" data-s-savews="${tileObj.id}">Save</button>
        </div>
        <select data-s-saved-ws="${tileObj.id}" style="margin-bottom:6px;">
          <option value="">Saved workspaces</option>
        </select>
        <div class="form-row" style="margin-bottom:6px;">
          <button class="form-btn" data-s-usews="${tileObj.id}">Use Selected</button>
          <button class="form-btn danger" data-s-rmws="${tileObj.id}">Remove</button>
        </div>
      </div>
      <div class="settings-group">
        <h3>Appearance</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <select data-s-theme="${tileObj.id}">
            <option value="dark">Dark</option>
            <option value="light">Light</option>
            <option value="system">System</option>
          </select>
        </div>
      </div>
      <div class="settings-group">
        <h3>System</h3>
        <label class="checkbox-inline">
          <input type="checkbox" data-s-autostart="${tileObj.id}" /> Launch at login
        </label>
      </div>
      <div class="form-row" style="margin-top:12px;">
        <button class="form-btn" data-s-save="${tileObj.id}">Save Settings</button>
        <span class="muted" data-s-status="${tileObj.id}"></span>
      </div>
    </div>
  `;

  const nameInput = body.querySelector(`[data-s-name="${tileObj.id}"]`);
  const aboutInput = body.querySelector(`[data-s-about="${tileObj.id}"]`);
  const modelSelect = body.querySelector(`[data-s-model="${tileObj.id}"]`);
  const openaiInput = body.querySelector(`[data-s-openai="${tileObj.id}"]`);
  const minimaxInput = body.querySelector(`[data-s-minimax="${tileObj.id}"]`);
  const searchInput = body.querySelector(`[data-s-search="${tileObj.id}"]`);
  const togetherInput = body.querySelector(`[data-s-together="${tileObj.id}"]`);
  const imgProvSelect = body.querySelector(`[data-s-imgprov="${tileObj.id}"]`);
  const imgModelSelect = body.querySelector(`[data-s-imgmodel="${tileObj.id}"]`);
  const wsInput = body.querySelector(`[data-s-workspace="${tileObj.id}"]`);
  const savedWsSelect = body.querySelector(`[data-s-saved-ws="${tileObj.id}"]`);
  const autostartInput = body.querySelector(`[data-s-autostart="${tileObj.id}"]`);
  const themeSelect = body.querySelector(`[data-s-theme="${tileObj.id}"]`);
  const statusEl = body.querySelector(`[data-s-status="${tileObj.id}"]`);
  const imgStatusEl = body.querySelector(`[data-s-img-status="${tileObj.id}"]`);

  function ensureOption(select, value) {
    if (!value) return;
    const exists = Array.from(select.options).some(o => o.value === value);
    if (!exists) {
      const opt = document.createElement("option");
      opt.value = value;
      opt.textContent = value;
      select.appendChild(opt);
    }
    select.value = value;
  }

  // Load settings
  async function loadS() {
    const s = await invoke("get_app_settings");
    loadedSettings = s;
    nameInput.value = s["user/name"] || "";
    aboutInput.value = s["user/about"] || "";

    const currentModel = s["llm/model"] || "MiniMax-M2.7";
    modelSelect.value = currentModel;

    openaiInput.value = "";
    minimaxInput.value = "";
    searchInput.value = "";
    togetherInput.value = "";
    openaiInput.placeholder = s["llm/openai_api_key"] ? "OpenAI key (saved)" : "OpenAI API key";
    minimaxInput.placeholder = s["llm/minimax_api_key"] ? "MiniMax key (saved)" : "MiniMax API key";
    searchInput.placeholder = s["search/brave_api_key"] ? "Search key (saved)" : "Web search key";
    togetherInput.placeholder = s["imagegen/together_api_key"] ? "Together key (saved)" : "Together AI key";
    imgProvSelect.value = s["imagegen/provider"] || "";

    const currentImgModel = s["imagegen/model"] || "";
    ensureOption(imgModelSelect, currentImgModel);

    sidebarModel.textContent = currentModel;

    try { autostartInput.checked = await isAutostartEnabled(); } catch {}

    themeSelect.value = localStorage.getItem("spino-theme") || "dark";

    // Workspace
    const pathInfo = await invoke("get_workspace_root");
    const workspaceRoot = field(pathInfo, "workspaceRoot", "workspace_root");
    wsInput.value = workspaceRoot;
    sidebarWorkspace.textContent = projectLabel(workspaceRoot);

    // Saved workspaces
    const items = await invoke("list_saved_workspaces");
    savedWsSelect.innerHTML = '<option value="">Saved workspaces</option>';
    for (const item of items) {
      const path = field(item, "path", "path");
      const opt = document.createElement("option");
      opt.value = path;
      opt.textContent = path;
      savedWsSelect.appendChild(opt);
    }
    if (workspaceRoot) savedWsSelect.value = workspaceRoot;
  }

  // Save
  body.querySelector(`[data-s-save="${tileObj.id}"]`).addEventListener("click", async () => {
    const updates = {
      "user/name": nameInput.value.trim(),
      "user/about": aboutInput.value.trim(),
      "llm/provider": "minimax",
      "llm/model": modelSelect.value.trim(),
      "imagegen/provider": imgProvSelect.value.trim(),
      "imagegen/model": imgModelSelect.value.trim()
    };
    if (openaiInput.value.trim()) updates["llm/openai_api_key"] = openaiInput.value.trim();
    if (minimaxInput.value.trim()) updates["llm/minimax_api_key"] = minimaxInput.value.trim();
    if (searchInput.value.trim()) updates["search/brave_api_key"] = searchInput.value.trim();
    if (togetherInput.value.trim()) updates["imagegen/together_api_key"] = togetherInput.value.trim();

    await invoke("set_app_settings", { req: { settings: updates } });
    statusEl.textContent = "Saved";
    loadedSettings = { ...loadedSettings, ...updates };
    sidebarModel.textContent = updates["llm/model"];
    setTimeout(() => { statusEl.textContent = ""; }, 2000);
    await loadS();
  });

  // Fetch image models
  body.querySelector(`[data-s-fetch-img-models="${tileObj.id}"]`).addEventListener("click", async () => {
    const prov = imgProvSelect.value;
    if (!prov) { imgStatusEl.textContent = "Select a provider first"; return; }
    imgStatusEl.textContent = "Fetching...";
    try {
      const models = await invoke("fetch_models", { provider: prov });
      // Filter to image-related models by type or name
      const imgModels = models.filter(m => {
        if (m.type === "image") return true;
        const id = (m.id || "").toLowerCase();
        return id.includes("image") || id.includes("flux") || id.includes("dall") ||
               id.includes("stable") || id.includes("sdxl") || id.includes("qwen-image");
      });
      const useModels = imgModels.length > 0 ? imgModels : models;
      const currentVal = imgModelSelect.value;
      imgModelSelect.innerHTML = '<option value="">Default</option>';
      for (const m of useModels) {
        const opt = document.createElement("option");
        opt.value = m.id;
        opt.textContent = m.id;
        imgModelSelect.appendChild(opt);
      }
      if (useModels.some(m => m.id === currentVal)) {
        imgModelSelect.value = currentVal;
      }
      imgStatusEl.textContent = `${useModels.length} models loaded`;
      setTimeout(() => { imgStatusEl.textContent = ""; }, 3000);
    } catch (e) {
      imgStatusEl.textContent = String(e);
    }
  });

  // Workspace controls
  body.querySelector(`[data-s-setws="${tileObj.id}"]`).addEventListener("click", async () => {
    try {
      const result = await invoke("set_workspace_root", { req: { workspaceRoot: wsInput.value.trim() } });
      wsInput.value = field(result, "workspaceRoot", "workspace_root");
      sidebarWorkspace.textContent = projectLabel(wsInput.value);
      statusEl.textContent = "Workspace set";
      setTimeout(() => { statusEl.textContent = ""; }, 2000);
    } catch (e) { statusEl.textContent = String(e); }
  });

  body.querySelector(`[data-s-savews="${tileObj.id}"]`).addEventListener("click", async () => {
    const path = wsInput.value.trim();
    if (!path) return;
    await invoke("save_workspace", { req: { path } });
    await loadS();
    statusEl.textContent = "Workspace saved";
    setTimeout(() => { statusEl.textContent = ""; }, 2000);
  });

  body.querySelector(`[data-s-usews="${tileObj.id}"]`).addEventListener("click", async () => {
    const selected = savedWsSelect.value;
    if (!selected) return;
    const result = await invoke("set_workspace_root", { req: { workspaceRoot: selected } });
    wsInput.value = field(result, "workspaceRoot", "workspace_root");
    sidebarWorkspace.textContent = projectLabel(wsInput.value);
  });

  body.querySelector(`[data-s-rmws="${tileObj.id}"]`).addEventListener("click", async () => {
    const selected = savedWsSelect.value;
    if (!selected) return;
    await invoke("remove_saved_workspace", { req: { path: selected } });
    await loadS();
  });

  // Theme
  themeSelect.addEventListener("change", () => {
    const val = themeSelect.value;
    localStorage.setItem("spino-theme", val);
    applyTheme(val);
  });

  // Autostart
  autostartInput.addEventListener("change", async () => {
    try {
      if (autostartInput.checked) await enableAutostart();
      else await disableAutostart();
    } catch (e) {
      autostartInput.checked = !autostartInput.checked;
    }
  });

  loadS();
}

const CONTEXT_WINDOW = 204800;

function updateContextBar(tileId, tokens) {
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

function estimateContextFromMessages(tileId, chatLog) {
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

// ==================== TAURI EVENT LISTENERS ====================
async function setupEventListeners() {
  // Chat token - route to the correct chat tile by session_id
  await listen("chat-token", (event) => {
    const { session_id, token } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;

    hideThinking(tile.id);

    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    if (!stream.bubble) {
      const turn = document.createElement("div");
      turn.className = "turn";
      const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      turn.innerHTML = `
        <div class="turn-header">
          <span class="turn-label spino">SPINO</span>
          <span class="turn-time">${time}</span>
        </div>
        <div class="turn-body"></div>
      `;
      chatLog.appendChild(turn);
      stream.bubble = turn.querySelector(".turn-body");
    }

    stream.text += token;
    stream.bubble.innerHTML = renderMarkdown(stream.text);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
  });

  // Tool execution events — show tool cards in the chat stream
  await listen("tool-executed", (event) => {
    const { session_id, tool, args, status } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;

    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    // Finalize any open text bubble before showing a tool card
    if (stream.bubble && status === "running") {
      stream.bubble.innerHTML = renderMarkdown(stream.text);
      stream.bubble = null;
      stream.text = "";
    }

    if (status === "running") {
      const card = addToolCard(chatLog, tool, args || "{}", "running");
      stream.toolElement = card;
    } else if (stream.toolElement) {
      const badge = stream.toolElement.querySelector(".status-badge");
      if (badge) badge.textContent = status;
      const dot = stream.toolElement.querySelector(".dot");
      if (dot) dot.classList.remove("running");
      stream.toolElement = null;
    }
  });

  await listen("chat-done", (event) => {
    const { session_id } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;

    if (stream.bubble) {
      stream.bubble.innerHTML = renderMarkdown(stream.text);
      stream.bubble = null;
      stream.text = "";
    }
    hideThinking(tile.id);
  });

  await listen("chat-usage", (event) => {
    const { session_id, input_tokens, output_tokens } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;
    const total = input_tokens + output_tokens;
    stream.contextTokens = total;
    updateContextBar(tile.id, total);
  });

  await listen("tool-start", (event) => {
    const { sessionId, toolName, arguments: args } = event.payload;
    const tile = getChatTileBySessionId(sessionId);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    if (stream.toolElement) {
      stream.toolElement.remove();
    }
    stream.toolElement = addToolCard(chatLog, toolName, args, "running");
  });

  await listen("tool-complete", (event) => {
    const { sessionId, toolName, output, status } = event.payload;
    const tile = getChatTileBySessionId(sessionId);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;

    if (stream.toolElement) {
      stream.toolElement.remove();
      stream.toolElement = null;
    }

    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (chatLog) {
      addToolCard(chatLog, toolName, "", status || "done");
    }
  });

  await listen("agent-iteration", (event) => {
    const { sessionId, agentName, iteration, maxIterations } = event.payload;
    const tile = getChatTileBySessionId(sessionId);
    if (!tile) return;
    showThinking(tile.id, `${agentName} (${iteration}/${maxIterations})`);
  });

  await listen("map-places", (event) => {
    const { session_id, query, results } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    const mapId = "map-" + Date.now();
    const el = document.createElement("div");
    el.className = "turn";
    el.innerHTML = `
      <div class="turn-body">
        <div class="map-title">Places: ${escapeHtml(query)}</div>
        <div id="${mapId}" class="map-container"></div>
        <div class="map-results">
          ${results.map((r, i) => `<div class="map-result">${i + 1}. <strong>${escapeHtml(r.name)}</strong></div>`).join("")}
        </div>
      </div>
    `;
    chatLog.appendChild(el);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;

    setTimeout(() => {
      const map = L.map(mapId).setView([results[0]?.lat || 0, results[0]?.lon || 0], 13);
      L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", { attribution: "\u00a9 OpenStreetMap" }).addTo(map);
      const bounds = [];
      results.forEach(r => {
        const lat = parseFloat(r.lat), lon = parseFloat(r.lon);
        L.marker([lat, lon]).addTo(map).bindPopup(r.name);
        bounds.push([lat, lon]);
      });
      if (bounds.length > 1) map.fitBounds(bounds, { padding: [30, 30] });
      if (tile) { (tileMaps[tile.id] = tileMaps[tile.id] || []).push(map); }
    }, 100);
  });

  await listen("map-route", (event) => {
    const { session_id, from, to, distance_km, duration_min, geometry, steps } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    const mapId = "map-" + Date.now();
    const distText = distance_km < 1 ? `${Math.round(distance_km * 1000)}m` : `${distance_km.toFixed(1)}km`;
    const durText = duration_min < 60 ? `${Math.round(duration_min)} min` : `${Math.floor(duration_min / 60)}h ${Math.round(duration_min % 60)}m`;

    const el = document.createElement("div");
    el.className = "turn";
    el.innerHTML = `
      <div class="turn-body">
        <div class="map-title">${escapeHtml(from)} \u2192 ${escapeHtml(to)}</div>
        <div class="map-summary">${distText} \u00b7 ${durText}</div>
        <div id="${mapId}" class="map-container"></div>
        ${steps && steps.length > 0 ? `<details class="map-steps"><summary>Directions (${steps.length} steps)</summary>${steps.map(s => `<div class="map-step">${escapeHtml(s.instruction)} (${s.distance}m)</div>`).join("")}</details>` : ""}
      </div>
    `;
    chatLog.appendChild(el);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;

    setTimeout(() => {
      const coords = geometry.map(c => [c[1], c[0]]);
      const map = L.map(mapId).setView(coords[0] || [0, 0], 13);
      L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", { attribution: "\u00a9 OpenStreetMap" }).addTo(map);
      L.polyline(coords, { color: "#4A90D9", weight: 4 }).addTo(map);
      L.marker(coords[0]).addTo(map).bindPopup(from);
      L.marker(coords[coords.length - 1]).addTo(map).bindPopup(to);
      map.fitBounds(coords, { padding: [30, 30] });
      if (tile) { (tileMaps[tile.id] = tileMaps[tile.id] || []).push(map); }
    }, 100);
  });

  await listen("memory-updated", () => {
    // Refresh any open memory tiles
    const memTiles = getTilesByType("memory");
    for (const t of memTiles) {
      const body = t.el.querySelector(".tile-body");
      if (body) renderMemoryTile(t, body);
    }
  });

  await listen("tasks-changed", () => {
    const taskTiles = getTilesByType("tasks");
    for (const t of taskTiles) {
      const body = t.el.querySelector(".tile-body");
      if (body) renderTasksTile(t, body);
    }
  });

  await listen("content-changed", () => {
    const contentTiles = getTilesByType("content");
    for (const t of contentTiles) {
      const body = t.el.querySelector(".tile-body");
      if (body) renderContentTile(t, body);
    }
  });

  // Drag-and-drop file import (Tauri native drag-drop provides file paths)
  await listen("tauri://drag-drop", async (event) => {
    const paths = event.payload?.paths;
    if (!paths || !paths.length) return;
    try {
      await invoke("import_content_files", { req: { paths } });
    } catch {}
  });

  await listen("image-generated", async (event) => {
    const { session_id, path, prompt } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;
    const stream = chatStreams[tile.id];

    // Finalize any open text bubble before inserting the image
    if (stream && stream.bubble) {
      stream.bubble.innerHTML = renderMarkdown(stream.text);
      stream.bubble = null;
      stream.text = "";
    }

    // Load image as base64 data URL (reliable across all Tauri configs)
    let src;
    try {
      src = await invoke("read_image_base64", { path });
    } catch {
      src = convertFileSrc(path); // fallback
    }

    const el = document.createElement("div");
    el.className = "turn image-turn";
    el.innerHTML = `
      <div class="turn-header">
        <span class="turn-label spino">SPINO</span>
        <span class="turn-time">${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</span>
      </div>
      <div class="turn-body">
        <img src="${src}" alt="${prompt.replace(/"/g, '&quot;')}" style="max-width:100%; border-radius:8px; margin:8px 0;" />
        <p class="image-caption">${escapeHtml(prompt)}</p>
      </div>
    `;
    chatLog.appendChild(el);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
  });
}

// ==================== SIDEBAR BUTTONS ====================
newChatBtn.addEventListener("click", () => addChatTile());

navWorkspacesBtn.addEventListener("click", () => {
  const existing = getTilesByType("workspaces");
  if (existing.length > 0) {
    focusTile(existing[0].id);
  } else {
    addTile("workspaces");
  }
});

navSettingsBtn.addEventListener("click", () => {
  const existing = getTilesByType("settings");
  if (existing.length > 0) {
    focusTile(existing[0].id);
  } else {
    addTile("settings");
  }
});

// ==================== BOOT ====================
async function bootPopout() {
  const { type, chatId } = getPopoutParams();

  // Hide sidebar, make workspace full width
  const sidebar = document.querySelector(".sidebar");
  const shell = document.querySelector(".shell");
  if (sidebar) sidebar.style.display = "none";
  if (shell) shell.style.gridTemplateColumns = "1fr";

  await setupEventListeners();
  try { loadedSettings = await invoke("get_app_settings"); } catch { loadedSettings = {}; }

  try { cachedAgents = await invoke("get_agents"); } catch {}

  if (type === "chat") {
    await addChatTile(chatId);
  } else if (type === "settings") {
    addTile("settings");
  } else {
    addTile(type);
  }

  // Replace tile chrome buttons with window controls
  const actions = workspace.querySelector(".tile-actions");
  if (actions) {
    const appWindow = getCurrentWebviewWindow();
    actions.innerHTML = `
      <button class="tile-btn" title="Minimize">&#8211;</button>
      <button class="tile-btn close" title="Close">&times;</button>
    `;
    actions.querySelector('[title="Minimize"]').addEventListener("click", () => appWindow.minimize());
    actions.querySelector('[title="Close"]').addEventListener("click", () => appWindow.close());
  }

  // Make tile header draggable for the pop-out window
  const header = workspace.querySelector(".tile-header");
  if (header) header.setAttribute("data-tauri-drag-region", "");
}

async function bootMain() {
  await setupEventListeners();

  try { loadedSettings = await invoke("get_app_settings"); } catch { loadedSettings = {}; }
  sidebarModel.textContent = loadedSettings["llm/model"] || "MiniMax-M2.7";

  try {
    const pathInfo = await invoke("get_workspace_root");
    const workspaceRoot = field(pathInfo, "workspaceRoot", "workspace_root");
    sidebarWorkspace.textContent = projectLabel(workspaceRoot);
  } catch { sidebarWorkspace.textContent = "~/"; }


  try { cachedAgents = await invoke("get_agents"); } catch {}

  const saved = restoreLayout();
  if (saved && saved.length > 0) {
    for (const item of saved) {
      if (item.type === "chat") {
        await addChatTile(item.chatId || "", item.projectPath || "");
      } else if (item.type === "settings") {
        addTile("settings");
      } else if (item.type === "workspaces") {
        addTile("workspaces");
      } else {
        addTile(item.type);
      }
    }
  } else {
    await addChatTile();
  }

  updateSidebar();
}

(async () => {
  try {
    if (isPopout()) {
      await bootPopout();
    } else {
      await bootMain();
    }
  } catch (error) {
    console.error("Boot error:", error);
  }
})();
