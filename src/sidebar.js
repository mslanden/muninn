// Categorized sidebar: a tab rail (Chat / Create / Data) plus a chat-history
// browser (pinned + recent sessions with pin/rename/delete). Split out of
// tilesys.js to keep that module focused on tile lifecycle.
//
// Import discipline: this module imports runtime helpers from tilesys.js and
// chat.js. Those functions are only *called* at event time (never at module
// init), so the tilesys<->sidebar import cycle is safe — same pattern as the
// existing tilesys<->chat cycle.

import { invoke } from "@tauri-apps/api/core";
import {
  sidebarTabs,
  sidebarNavCreate,
  sidebarNavData,
  sessionList,
  sessionSearchInput,
  navSettingsBtn,
  tiles,
  chatStreams,
  ui,
} from "./state.js";
import { escapeHtml, field, threadLabel, feedTimeAgo, projectLabel } from "./utils.js";
import { icon } from "./icons.js";
import {
  TILE_TYPES,
  getTilesByType,
  focusTile,
  openTileForType,
  removeTile,
} from "./tilesys.js";
import { addChatTile } from "./chat.js";

// ==================== TABS ====================
const TABS = [
  { id: "chat",   label: "Chat",   ic: "message-square" },
  { id: "create", label: "Create", ic: "sparkles" },
  { id: "data",   label: "Data",   ic: "database" },
];

// Which TILE_TYPES belong under each category panel (order matters).
const CREATE_TYPES = ["studio", "content", "web"];
const DATA_TYPES = ["feeds", "tasks", "agents", "memory", "logs", "tools"];

const TAB_KEY = "muninn.sidebar-tab";

function getActiveTab() {
  const saved = localStorage.getItem(TAB_KEY);
  return TABS.some((t) => t.id === saved) ? saved : "chat";
}

function setActiveTab(id) {
  localStorage.setItem(TAB_KEY, id);
  renderTabs();
  showPanel(id);
}

function showPanel(id) {
  document.querySelectorAll(".sidebar-panel").forEach((p) => {
    p.hidden = p.dataset.panel !== id;
  });
  if (id === "chat") {
    refreshSessionList();
  } else {
    renderNavPanel(id);
  }
}

function renderTabs() {
  if (!sidebarTabs) return;
  const active = getActiveTab();
  sidebarTabs.innerHTML = "";
  for (const tab of TABS) {
    const btn = document.createElement("button");
    btn.className = `sidebar-tab${tab.id === active ? " active" : ""}`;
    btn.title = tab.label;
    btn.innerHTML = `${icon(tab.ic, 15)}<span>${escapeHtml(tab.label)}</span>`;
    btn.addEventListener("click", () => setActiveTab(tab.id));
    sidebarTabs.appendChild(btn);
  }
}

// ==================== NAV-ITEM PANELS (Create / Data) ====================
function makeNavItem(def) {
  const openTiles = getTilesByType(def.type);
  const isOpen = openTiles.length > 0;

  const btn = document.createElement("button");
  btn.className = `nav-item${isOpen ? " open" : ""}`;
  btn.title = def.label;
  btn.innerHTML = `
    <span class="nav-dot ${isOpen ? def.dot : "off"}"></span>
    ${escapeHtml(def.label)}
    ${openTiles.length > 1 ? `<span class="nav-count">${openTiles.length}</span>` : ""}
    <span class="nav-pip"></span>
    <button class="nav-add" title="Add ${escapeHtml(def.label)}">+</button>
  `;

  btn.addEventListener("click", (e) => {
    if (e.target.classList.contains("nav-add")) return;
    if (isOpen) {
      focusTile(openTiles[0].id);
    } else {
      openTileForType(def.type);
    }
  });

  const addBtn = btn.querySelector(".nav-add");
  addBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    openTileForType(def.type);
  });

  return btn;
}

function renderNavPanel(id) {
  const container = id === "create" ? sidebarNavCreate : id === "data" ? sidebarNavData : null;
  if (!container) return;
  const typeList = id === "create" ? CREATE_TYPES : DATA_TYPES;
  container.innerHTML = "";
  for (const type of typeList) {
    const def = TILE_TYPES.find((t) => t.type === type);
    if (!def) continue;
    container.appendChild(makeNavItem(def));
  }
}

// ==================== CHAT HISTORY BROWSER ====================
// Debounced search over get_sessions.
let searchTimer = null;
let searchQuery = "";
let renaming = null; // sessionId currently in inline-rename mode

// Which sessions currently have an open chat tile (highlight + click routing).
function tileForSession(sessionId) {
  return tiles.find(
    (t) => t.type === "chat" && (t.chatId === sessionId || chatStreams[t.id]?.sessionId === sessionId)
  );
}

function focusedSessionId() {
  const t = tiles.find((x) => x.id === ui.focusedTileId && x.type === "chat");
  return t ? (t.chatId || chatStreams[t.id]?.sessionId) : null;
}

function timeOf(s) {
  const raw = field(s, "lastMessageAt", "last_message_at") || field(s, "createdAt", "created_at") || "";
  if (!raw) return "";
  try { return feedTimeAgo(raw); } catch { return ""; }
}

// Build one session row. All user-derived strings escaped (innerHTML sink).
function sessionRow(s, index) {
  const sid = field(s, "id", "id");
  const title = field(s, "title", "title");
  const label = (title && title.trim()) ? title.trim() : threadLabel(s, index);
  const pinned = !!field(s, "pinned", "pinned");
  const pp = field(s, "projectPath", "project_path") || "";
  const open = !!tileForSession(sid);
  const isFocused = focusedSessionId() === sid;

  const row = document.createElement("div");
  row.className = `session-item${open ? " open" : ""}${isFocused ? " active" : ""}`;
  row.dataset.sessionId = sid;
  row.dataset.projectPath = pp;

  const projectTag = pp
    ? `<span class="session-project" title="${escapeHtml(pp)}">${escapeHtml(projectLabel(pp))}</span>`
    : "";

  row.innerHTML = `
    <div class="session-main">
      <span class="session-label">${escapeHtml(label)}</span>
      <span class="session-meta">
        <span class="session-time">${escapeHtml(timeOf(s))}</span>
        ${projectTag}
      </span>
    </div>
    <div class="session-item-actions">
      <button class="session-act session-pin${pinned ? " pinned" : ""}" data-act="pin" title="${pinned ? "Unpin" : "Pin"}" aria-label="${pinned ? "Unpin" : "Pin"}">${icon("pin", 13)}</button>
      <button class="session-act" data-act="rename" title="Rename" aria-label="Rename">${icon("pencil", 13)}</button>
      <button class="session-act session-del" data-act="delete" title="Delete" aria-label="Delete">${icon("trash", 13)}</button>
    </div>
  `;

  // Open / focus on row click (ignore clicks on action buttons or the rename input).
  const main = row.querySelector(".session-main");
  main.addEventListener("click", () => openSession(sid, pp));

  const pinBtn = row.querySelector('[data-act="pin"]');
  pinBtn.addEventListener("click", async (e) => {
    e.stopPropagation();
    try {
      await invoke("set_session_pinned", { sessionId: sid, pinned: !pinned });
    } catch (err) { console.error("set_session_pinned failed:", err); }
    refreshSessionList();
  });

  row.querySelector('[data-act="rename"]').addEventListener("click", (e) => {
    e.stopPropagation();
    startRename(row, sid, label);
  });

  row.querySelector('[data-act="delete"]').addEventListener("click", async (e) => {
    e.stopPropagation();
    if (!confirm(`Delete this chat?\n\n${label}`)) return;
    try {
      await invoke("delete_session", { sessionId: sid });
    } catch (err) { console.error("delete_session failed:", err); }
    // Close any open tile bound to this session.
    const t = tileForSession(sid);
    if (t) removeTile(t.id);
    refreshSessionList();
  });

  // If this row is the one being renamed, immediately enter edit mode after render.
  if (renaming === sid) startRename(row, sid, label);

  return row;
}

async function openSession(sessionId, projectPath) {
  const existing = tileForSession(sessionId);
  if (existing) {
    focusTile(existing.id);
    return;
  }
  await addChatTile(sessionId, projectPath || "");
  refreshSessionList();
}

// Swap the label for an inline input. Enter commits via rename_session; Escape cancels.
function startRename(row, sid, currentLabel) {
  renaming = sid;
  const main = row.querySelector(".session-main");
  if (!main || main.querySelector(".session-rename-input")) return;
  const input = document.createElement("input");
  input.type = "text";
  input.className = "session-rename-input";
  input.value = currentLabel;
  main.innerHTML = "";
  main.appendChild(input);
  input.focus();
  input.select();

  const commit = async () => {
    const val = input.value.trim();
    renaming = null;
    try {
      await invoke("rename_session", { sessionId: sid, title: val });
    } catch (err) { console.error("rename_session failed:", err); }
    refreshSessionList();
  };
  const cancel = () => { renaming = null; refreshSessionList(); };

  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); commit(); }
    else if (e.key === "Escape") { e.preventDefault(); cancel(); }
  });
  input.addEventListener("blur", () => { if (renaming === sid) commit(); });
}

function sectionHeader(text) {
  const h = document.createElement("div");
  h.className = "session-section";
  h.textContent = text;
  return h;
}

let sessionListSeq = 0;
export async function refreshSessionList() {
  if (!sessionList) return;
  // Don't clobber an in-progress inline rename.
  if (renaming) return;
  const seq = ++sessionListSeq;
  let sessions = [];
  try {
    sessions = await invoke("get_sessions", {
      req: { limit: 40, projectPath: null, query: searchQuery || null },
    });
  } catch (err) {
    console.error("get_sessions failed:", err);
    sessions = [];
  }
  // A newer refresh started while we awaited — drop this stale result.
  if (seq !== sessionListSeq) return;
  if (!Array.isArray(sessions)) sessions = [];

  const pinned = sessions.filter((s) => field(s, "pinned", "pinned"));
  const recent = sessions.filter((s) => !field(s, "pinned", "pinned")).slice(0, 30);

  sessionList.innerHTML = "";

  if (pinned.length === 0 && recent.length === 0) {
    const empty = document.createElement("div");
    empty.className = "session-empty";
    empty.textContent = searchQuery ? "No chats match." : "No chats yet.";
    sessionList.appendChild(empty);
    return;
  }

  if (pinned.length) {
    sessionList.appendChild(sectionHeader("Pinned"));
    pinned.forEach((s, i) => sessionList.appendChild(sessionRow(s, i)));
  }
  if (recent.length) {
    sessionList.appendChild(sectionHeader("Recent"));
    recent.forEach((s, i) => sessionList.appendChild(sessionRow(s, i)));
  }
}

function wireSearch() {
  if (!sessionSearchInput) return;
  sessionSearchInput.addEventListener("input", () => {
    searchQuery = sessionSearchInput.value.trim();
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => refreshSessionList(), 250);
  });
}

// ==================== PUBLIC ENTRY ====================
// Called by tilesys.js on every tile add/remove/focus. Re-renders the tab rail,
// the active nav panel, and (cheaply) the session list highlight — without
// clobbering the search input's value/focus (it lives outside the re-rendered
// #session-list) or an in-progress rename (guarded in refreshSessionList).
export function updateSidebar() {
  renderTabs();
  const active = getActiveTab();
  showPanel(active);

  // Settings button (lives outside the panels).
  const settingsOpen = getTilesByType("settings").length > 0;
  if (navSettingsBtn) {
    navSettingsBtn.classList.toggle("open", settingsOpen);
    const dot = navSettingsBtn.querySelector(".nav-dot");
    if (dot) dot.className = `nav-dot ${settingsOpen ? "purple" : "off"}`;
  }
}

// One-time wiring at boot (search input). Safe to call once.
export function initSidebar() {
  wireSearch();
  updateSidebar();
}
