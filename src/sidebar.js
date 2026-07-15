// Sidebar: icon rail + expandable panel with Chat / Apps tabs, session browser,
// agents shortcuts, and footer workspace/model switchers.
//
// Import discipline: this module imports runtime helpers from tilesys.js and
// chat.js. Those functions are only *called* at event time (never at module
// init), so the tilesys<->sidebar import cycle is safe — same pattern as the
// existing tilesys<->chat cycle.

import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  sidebarTabs,
  sidebarNavApps,
  sessionList,
  sessionSearchInput,
  sidebarAgentsList,
  sidebarWorkspace,
  sidebarModel,
  navSettingsBtn,
  sidebarCollapseBtn,
  railBrandBtn,
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
  { id: "chat", label: "Chat", ic: "message-square" },
  { id: "apps", label: "Apps", ic: "grid" },
];

// Apps panel tile types (Logs & Tools live under Settings now).
const APPS_TYPES = ["studio", "content", "web", "feeds", "tasks", "memory"];

const TAB_KEY = "muninn.sidebar-tab";
const COLLAPSE_KEY = "muninn.sidebar-collapsed";

function getActiveTab() {
  const saved = localStorage.getItem(TAB_KEY);
  // Migrate old Create/Data tab ids.
  if (saved === "create" || saved === "data") return "apps";
  return TABS.some((t) => t.id === saved) ? saved : "chat";
}

function setActiveTab(id) {
  localStorage.setItem(TAB_KEY, id);
  renderTabs();
  showPanel(id);
  // Expanding to a tab should un-collapse the panel.
  if (document.body.classList.contains("sidebar-collapsed")) {
    setCollapsed(false);
  }
}

function showPanel(id) {
  document.querySelectorAll(".sidebar-panel").forEach((p) => {
    p.hidden = p.dataset.panel !== id;
  });
  if (id === "chat") {
    refreshSessionList();
    refreshAgentsList();
  } else {
    renderAppsPanel();
  }
}

function renderTabs() {
  if (!sidebarTabs) return;
  const active = getActiveTab();
  sidebarTabs.innerHTML = "";
  for (const tab of TABS) {
    const btn = document.createElement("button");
    btn.className = `rail-btn sidebar-tab${tab.id === active ? " active" : ""}`;
    btn.title = tab.label;
    btn.setAttribute("aria-label", tab.label);
    btn.innerHTML = icon(tab.ic, 16);
    btn.addEventListener("click", () => setActiveTab(tab.id));
    sidebarTabs.appendChild(btn);
  }
}

// ==================== COLLAPSE ====================
function isCollapsed() {
  return localStorage.getItem(COLLAPSE_KEY) === "1";
}

function setCollapsed(collapsed) {
  localStorage.setItem(COLLAPSE_KEY, collapsed ? "1" : "0");
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  if (sidebarCollapseBtn) {
    sidebarCollapseBtn.title = collapsed ? "Expand sidebar" : "Collapse sidebar";
    sidebarCollapseBtn.setAttribute("aria-label", sidebarCollapseBtn.title);
    sidebarCollapseBtn.innerHTML = icon(collapsed ? "chevron-right" : "chevron-left", 15);
  }
}

function wireCollapse() {
  if (sidebarCollapseBtn) {
    sidebarCollapseBtn.addEventListener("click", () => setCollapsed(!isCollapsed()));
  }
  if (railBrandBtn) {
    railBrandBtn.addEventListener("click", () => {
      if (isCollapsed()) setCollapsed(false);
      setActiveTab("chat");
    });
  }
}

// ==================== APPS PANEL ====================
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
  `;

  btn.addEventListener("click", () => {
    if (isOpen) {
      focusTile(openTiles[0].id);
    } else {
      openTileForType(def.type);
    }
  });

  return btn;
}

function renderAppsPanel() {
  if (!sidebarNavApps) return;
  sidebarNavApps.innerHTML = "";
  for (const type of APPS_TYPES) {
    const def = TILE_TYPES.find((t) => t.type === type);
    if (!def) continue;
    sidebarNavApps.appendChild(makeNavItem(def));
  }
}

// ==================== AGENTS (under Chat) ====================
export async function refreshAgentsList() {
  if (!sidebarAgentsList) return;
  let agents = ui.cachedAgents;
  if (!Array.isArray(agents) || !agents.length) {
    try {
      agents = await invoke("get_agents");
      ui.cachedAgents = agents;
    } catch {
      agents = [];
    }
  }
  if (!Array.isArray(agents)) agents = [];

  const enabled = agents.filter((a) => field(a, "enabled", "enabled"));
  sidebarAgentsList.innerHTML = "";

  // Manage link always present.
  const manage = document.createElement("button");
  manage.className = "nav-item agents-manage";
  manage.title = "Manage agents";
  manage.innerHTML = `
    <span class="nav-dot ${getTilesByType("agents").length ? "blue" : "off"}"></span>
    Manage agents
  `;
  manage.addEventListener("click", () => openTileForType("agents"));
  sidebarAgentsList.appendChild(manage);

  for (const a of enabled.slice(0, 8)) {
    const name = field(a, "name", "name") || "Agent";
    const slash = field(a, "slashCommand", "slash_command") || "";
    const btn = document.createElement("button");
    btn.className = "agent-shortcut";
    btn.title = slash ? `Run /${slash}` : `Open ${name}`;
    btn.innerHTML = `
      <span class="agent-shortcut-name">${escapeHtml(name)}</span>
      ${slash ? `<span class="agent-shortcut-slash">/${escapeHtml(slash)}</span>` : ""}
    `;
    btn.addEventListener("click", () => {
      if (slash) {
        insertAgentSlash(slash);
      } else {
        openTileForType("agents");
      }
    });
    sidebarAgentsList.appendChild(btn);
  }
}

/** Prefill the focused (or newest) chat composer with `/slash ` and focus it. */
function insertAgentSlash(slash) {
  const chatTiles = getTilesByType("chat");
  let tile = chatTiles.find((t) => t.id === ui.focusedTileId);
  if (!tile) tile = chatTiles[chatTiles.length - 1];
  if (!tile) {
    openTileForType("agents");
    return;
  }
  focusTile(tile.id);
  const input = tile.el.querySelector(`[data-chat-input="${tile.id}"]`);
  if (!input) return;
  const prefix = `/${slash} `;
  if (!input.value.startsWith(prefix)) {
    input.value = prefix + input.value.replace(/^\/\S*\s*/, "");
  }
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

// ==================== CHAT HISTORY BROWSER ====================
let searchTimer = null;
let searchQuery = "";
let renaming = null; // sessionId currently in inline-rename mode

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

  const main = row.querySelector(".session-main");
  main.addEventListener("click", () => openSession(sid, pp));

  row.querySelector('[data-act="pin"]').addEventListener("click", async (e) => {
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
    const t = tileForSession(sid);
    if (t) removeTile(t.id);
    refreshSessionList();
  });

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

// ==================== FOOTER SWITCHERS ====================
let openPopover = null;

function closePopover() {
  if (openPopover) {
    openPopover.remove();
    openPopover = null;
  }
}

function placePopover(anchor, el) {
  closePopover();
  document.body.appendChild(el);
  openPopover = el;
  const rect = anchor.getBoundingClientRect();
  const pad = 8;
  el.style.left = `${Math.max(pad, rect.left)}px`;
  el.style.bottom = `${window.innerHeight - rect.top + 6}px`;
  // Clamp width to viewport.
  requestAnimationFrame(() => {
    const w = el.offsetWidth;
    const maxLeft = window.innerWidth - w - pad;
    el.style.left = `${Math.min(Math.max(pad, rect.left), maxLeft)}px`;
  });
}

async function showWorkspacePopover(anchor) {
  const pop = document.createElement("div");
  pop.className = "footer-popover";
  pop.innerHTML = `<div class="footer-popover-title">Workspace</div><div class="footer-popover-list"></div>`;
  const list = pop.querySelector(".footer-popover-list");

  // Current default directory
  let currentRoot = "~/";
  try {
    const pathInfo = await invoke("get_workspace_root");
    currentRoot = field(pathInfo, "workspaceRoot", "workspace_root") || "~/";
  } catch {}

  const addRow = (label, path, isCurrent) => {
    const btn = document.createElement("button");
    btn.className = `footer-popover-item${isCurrent ? " current" : ""}`;
    btn.innerHTML = `
      <span class="footer-popover-label">${escapeHtml(label)}</span>
      ${path ? `<span class="footer-popover-path">${escapeHtml(path)}</span>` : ""}
    `;
    btn.addEventListener("click", async () => {
      closePopover();
      if (!path) return;
      try {
        await invoke("set_workspace_root", { req: { workspaceRoot: path } });
        if (sidebarWorkspace) {
          sidebarWorkspace.textContent = projectLabel(path);
          sidebarWorkspace.title = path;
        }
      } catch (e) {
        console.error("set_workspace_root failed:", e);
      }
    });
    list.appendChild(btn);
  };

  addRow(projectLabel(currentRoot) || "Default", currentRoot, true);

  try {
    const projects = await invoke("get_projects");
    for (const p of projects || []) {
      const path = field(p, "path", "path");
      const name = field(p, "name", "name") || projectLabel(path);
      if (path === currentRoot) continue;
      addRow(name, path, false);
    }
  } catch {}

  const browse = document.createElement("button");
  browse.className = "footer-popover-item footer-popover-action";
  browse.innerHTML = `${icon("folder", 13)} Choose folder…`;
  browse.addEventListener("click", async () => {
    closePopover();
    try {
      const selected = await openDialog({
        directory: true,
        multiple: false,
        title: "Set default workspace folder",
      });
      if (!selected) return;
      const path = typeof selected === "string" ? selected : selected;
      await invoke("set_workspace_root", { req: { workspaceRoot: path } });
      if (sidebarWorkspace) {
        sidebarWorkspace.textContent = projectLabel(path);
        sidebarWorkspace.title = path;
      }
    } catch (e) {
      console.error("workspace pick failed:", e);
    }
  });
  list.appendChild(browse);

  placePopover(anchor, pop);
}

const MODEL_PRESETS = [
  { provider: "minimax", model: "MiniMax-M3", label: "MiniMax M3" },
  { provider: "meta", model: "muse-spark-1.1", label: "Muse Spark 1.1" },
  { provider: "openai", model: "gpt-4.1", label: "GPT-4.1" },
  { provider: "openai", model: "gpt-4o", label: "GPT-4o" },
  { provider: "anthropic", model: "claude-sonnet-4-5", label: "Claude Sonnet 4.5" },
  { provider: "groq", model: "llama-3.3-70b-versatile", label: "Llama 3.3 70B" },
  { provider: "google", model: "gemini-2.5-pro", label: "Gemini 2.5 Pro" },
  { provider: "xai", model: "grok-3", label: "Grok 3" },
  { provider: "openrouter", model: "openrouter/auto", label: "OpenRouter Auto" },
];

async function showModelPopover(anchor) {
  const pop = document.createElement("div");
  pop.className = "footer-popover";
  pop.innerHTML = `<div class="footer-popover-title">Model</div><div class="footer-popover-list"></div>`;
  const list = pop.querySelector(".footer-popover-list");

  const currentModel = (ui.loadedSettings && ui.loadedSettings["llm/model"]) || "";
  const currentProv = (ui.loadedSettings && ui.loadedSettings["llm/provider"]) || "minimax";

  for (const preset of MODEL_PRESETS) {
    const isCurrent = preset.model === currentModel && preset.provider === currentProv;
    const btn = document.createElement("button");
    btn.className = `footer-popover-item${isCurrent ? " current" : ""}`;
    btn.innerHTML = `
      <span class="footer-popover-label">${escapeHtml(preset.label)}</span>
      <span class="footer-popover-path">${escapeHtml(preset.provider)}</span>
    `;
    btn.addEventListener("click", async () => {
      closePopover();
      try {
        await invoke("set_app_settings", {
          req: { settings: { "llm/provider": preset.provider, "llm/model": preset.model } },
        });
        ui.loadedSettings = {
          ...ui.loadedSettings,
          "llm/provider": preset.provider,
          "llm/model": preset.model,
        };
        if (sidebarModel) {
          sidebarModel.textContent = preset.model;
          sidebarModel.title = `${preset.provider} · ${preset.model}`;
        }
      } catch (e) {
        console.error("set model failed:", e);
      }
    });
    list.appendChild(btn);
  }

  const custom = document.createElement("button");
  custom.className = "footer-popover-item footer-popover-action";
  custom.innerHTML = `${icon("settings", 13)} Open Settings…`;
  custom.addEventListener("click", () => {
    closePopover();
    openTileForType("settings");
  });
  list.appendChild(custom);

  placePopover(anchor, pop);
}

function wireFooterSwitchers() {
  if (sidebarWorkspace) {
    sidebarWorkspace.addEventListener("click", (e) => {
      e.stopPropagation();
      if (openPopover) { closePopover(); return; }
      showWorkspacePopover(sidebarWorkspace);
    });
  }
  if (sidebarModel) {
    sidebarModel.addEventListener("click", (e) => {
      e.stopPropagation();
      if (openPopover) { closePopover(); return; }
      showModelPopover(sidebarModel);
    });
  }
  document.addEventListener("click", (e) => {
    if (!openPopover) return;
    if (openPopover.contains(e.target)) return;
    if (e.target === sidebarWorkspace || e.target === sidebarModel) return;
    closePopover();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closePopover();
  });
}

// ==================== PUBLIC ENTRY ====================
export function updateSidebar() {
  renderTabs();
  const active = getActiveTab();
  showPanel(active);

  const settingsOpen = getTilesByType("settings").length > 0;
  if (navSettingsBtn) {
    navSettingsBtn.classList.toggle("open", settingsOpen);
    const dot = navSettingsBtn.querySelector(".nav-dot");
    if (dot) dot.className = `nav-dot ${settingsOpen ? "purple" : "off"}`;
  }
}

export function initSidebar() {
  wireSearch();
  wireCollapse();
  wireFooterSwitchers();
  setCollapsed(isCollapsed());
  updateSidebar();
}
