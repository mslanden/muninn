import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { getTileRenderer } from "./registry.js";
import { workspace, tiles, chatStreams, tileMaps, ui } from "./state.js";
import { escapeHtml } from "./utils.js";
import { addChatTile } from "./chat.js";
import { updateSidebar } from "./sidebar.js";

// Re-exported so existing importers (main.js) can keep importing updateSidebar
// from tilesys.js. The implementation lives in sidebar.js.
export { updateSidebar };

// ==================== TILE MANAGEMENT ====================
export const TILE_TYPES = [
  { type: "chat",       label: "Chat",       dot: "green" },
  { type: "feeds",      label: "Feeds",      dot: "amber" },
  { type: "web",        label: "Web",        dot: "blue"  },
  { type: "tasks",      label: "Tasks",      dot: "blue"  },
  { type: "agents",     label: "Agents",     dot: "blue"  },
  { type: "memory",     label: "Memory",     dot: "purple" },
  { type: "content",    label: "Content",    dot: "off"   },
  { type: "studio",     label: "Studio",     dot: "green" },
  // Still renderable (Settings sub-pages / restored layouts) but not in Apps nav.
  { type: "logs",       label: "Activity",   dot: "amber" },
  { type: "tools",      label: "Tools",      dot: "purple" },
];

export function updateWorkspaceGrid() {
  // Column/row flow is owned by CSS (.workspace uses a wrapping auto-fit grid).
  // Clear any legacy inline override and expose the count for potential styling.
  workspace.style.gridTemplateColumns = "";
  workspace.dataset.tileCount = String(tiles.length);
}

export function focusTile(tileId) {
  ui.focusedTileId = tileId;
  for (const t of tiles) {
    t.el.classList.toggle("focused", t.id === tileId);
  }
}

export function addTile(type, opts = {}) {
  const id = ui.nextTileId++;
  const tile = document.createElement("div");
  tile.className = "tile";
  tile.dataset.tileId = id;
  tile.dataset.tileType = type;
  // Make tiles reachable by keyboard and announced as labelled regions.
  tile.tabIndex = 0;
  tile.setAttribute("role", "region");

  const label = TILE_TYPES.find(t => t.type === type)?.label || type;
  const subtitle = opts.subtitle || "";
  tile.setAttribute("aria-label", subtitle ? `${label} — ${subtitle}` : label);

  tile.innerHTML = `
    <div class="tile-header">
      <span class="tile-title">${escapeHtml(label)}</span>
      ${subtitle ? `<span class="tile-subtitle">${escapeHtml(subtitle)}</span>` : ""}
      <span class="tile-spacer"></span>
      <div class="tile-actions">
        <button class="tile-btn pop" title="Pop out">&#x29C9;</button>
        <button class="tile-btn close" title="Close">&times;</button>
      </div>
    </div>
    <div class="tile-body"></div>
  `;

  tile.addEventListener("mousedown", () => focusTile(id));
  // Track focus reached via keyboard (Tab) so the focused-tile state stays in sync.
  tile.addEventListener("focusin", () => focusTile(id));

  tile.querySelector(".tile-btn.pop").addEventListener("click", (e) => {
    e.stopPropagation();
    popOutTile(id);
  });

  tile.querySelector(".tile-btn.close").addEventListener("click", (e) => {
    e.stopPropagation();
    removeTile(id);
  });

  const tileObj = {
    id,
    type,
    el: tile,
    chatId: opts.chatId || null,
    projectPath: opts.projectPath || "",
    webUrl: opts.webUrl || ""
  };
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

export function removeTile(tileId) {
  const idx = tiles.findIndex(t => t.id === tileId);
  if (idx === -1) return;
  const tile = tiles[idx];
  if (typeof tile.cleanup === "function") {
    try { tile.cleanup(); } catch {}
  }
  tile.el.remove();
  tiles.splice(idx, 1);

  // Clean up chat streaming state
  if (tile.type === "chat") {
    delete chatStreams[tileId];
  }

  // Clean up Mapbox GL map instances
  if (tileMaps[tileId]) {
    for (const m of tileMaps[tileId]) {
      try { m.remove(); } catch {}
    }
    delete tileMaps[tileId];
  }

  if (ui.focusedTileId === tileId) {
    ui.focusedTileId = tiles.length > 0 ? tiles[tiles.length - 1].id : null;
    for (const t of tiles) {
      t.el.classList.toggle("focused", t.id === ui.focusedTileId);
    }
  }

  updateWorkspaceGrid();
  updateSidebar();
  saveLayout();
}

// Global keyboard shortcuts for moving between tiles:
//   Cmd/Ctrl+1..9  → focus the Nth tile
//   Cmd/Ctrl+]     → focus the next tile (wraps)
//   Cmd/Ctrl+[     → focus the previous tile (wraps)
// Focusing the tile element drives the focused-tile state via the `focusin`
// handler and shows the keyboard focus ring.
export function setupTileKeyboardNav() {
  document.addEventListener("keydown", (e) => {
    if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return;

    if (e.key >= "1" && e.key <= "9") {
      const idx = Number(e.key) - 1;
      if (idx < tiles.length) {
        e.preventDefault();
        tiles[idx].el.focus();
      }
      return;
    }

    if (e.key === "[" || e.key === "]") {
      if (tiles.length === 0) return;
      e.preventDefault();
      const cur = tiles.findIndex(t => t.id === ui.focusedTileId);
      const base = cur === -1 ? 0 : cur;
      const delta = e.key === "]" ? 1 : -1;
      const next = (base + delta + tiles.length) % tiles.length;
      tiles[next].el.focus();
    }
  });
}

export function getTilesByType(type) {
  return tiles.filter(t => t.type === type);
}

export function getActiveChatTile() {
  if (ui.focusedTileId) {
    const focused = tiles.find(t => t.id === ui.focusedTileId && t.type === "chat");
    if (focused) return focused;
  }
  const chats = getTilesByType("chat");
  return chats.length > 0 ? chats[chats.length - 1] : null;
}

export function getChatTileBySessionId(sessionId) {
  if (!sessionId) return getActiveChatTile();
  const match = tiles.find(t => t.type === "chat" && chatStreams[t.id]?.sessionId === sessionId);
  return match || null;
}

// ==================== SIDEBAR ====================
// The categorized sidebar (tabs + chat-history browser) lives in sidebar.js.
// updateSidebar is re-exported from there at the top of this module.

export function openTileForType(type) {
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

// Open a URL in a dedicated Tauri webview window — a real native webview, so sites that
// refuse iframe embedding (X-Frame-Options / frame-ancestors) still load here. Reuses an
// existing window for the same URL on repeat clicks instead of spawning duplicates.
// ==================== POP-OUT WINDOWS ====================
export function popOutTile(tileId) {
  const tile = tiles.find(t => t.id === tileId);
  if (!tile) return;

  const params = new URLSearchParams();
  params.set("tile", tile.type);
  if (tile.chatId) params.set("chatId", tile.chatId);
  if (tile.webUrl) params.set("url", tile.webUrl);

  const label = `popout-${tile.type}-${Date.now()}`;
  const popoutUrl = `${window.location.origin}${window.location.pathname}?${params.toString()}`;
  const webview = new WebviewWindow(label, {
    url: popoutUrl,
    width: 700,
    height: 600,
    resizable: true,
    title: (TILE_TYPES.find(t => t.type === tile.type)?.label || tile.type) + " — Muninn",
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

export function isPopout() {
  return new URLSearchParams(window.location.search).has("tile");
}

export function getPopoutParams() {
  const params = new URLSearchParams(window.location.search);
  return {
    type: params.get("tile"),
    chatId: params.get("chatId") || "",
    url: params.get("url") || ""
  };
}

// ==================== LAYOUT PERSISTENCE ====================
export function saveLayout() {
  const layout = tiles.map(t => ({
    type: t.type,
    chatId: t.chatId,
    projectPath: t.projectPath || "",
    webUrl: t.webUrl || ""
  }));
  localStorage.setItem("muninn.layout", JSON.stringify(layout));
}

export function restoreLayout() {
  try {
    const saved = JSON.parse(localStorage.getItem("muninn.layout"));
    if (saved && saved.length > 0) {
      return saved;
    }
  } catch {}
  return null;
}


export function renderTileContent(tileObj, body) {
  const renderer = getTileRenderer(tileObj.type);
  if (renderer) renderer(tileObj, body);
}
