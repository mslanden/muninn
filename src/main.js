// Entry point: wires sidebar buttons and boots the window.
// Feature modules are imported for their side effect of registering tile renderers.
import "./theme.js";
import "./web.js";
import "./chat.js";
import "./feeds.js";
import "./logs.js";
import "./tasks.js";
import "./toolspanel.js";
import "./agents.js";
import "./memory.js";
import "./content.js";
import "./studio.js";
import "./settings.js";

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { workspace, newChatBtn, navSettingsBtn, ui } from "./state.js";
import { addTile, isPopout, getPopoutParams, restoreLayout, setupTileKeyboardNav, openTileForType, replaceSingleTileIfAlone, makeDragSource } from "./tilesys.js";
import { addChatTile, updateModelChips } from "./chat.js";
import { initSidebar, refreshSessionList } from "./sidebar.js";
import { setupEventListeners } from "./events.js";
import { isQuickAsk, bootQuickAsk } from "./quickask.js";

// ==================== SIDEBAR BUTTONS ====================
newChatBtn.addEventListener("click", async () => {
  replaceSingleTileIfAlone();
  await addChatTile();
  try { refreshSessionList(); } catch {}
});
makeDragSource(newChatBtn, { type: "chat" });

navSettingsBtn.addEventListener("click", () => openTileForType("settings"));
makeDragSource(navSettingsBtn, { type: "settings" });

// ==================== BOOT ====================
async function bootPopout() {
  const { type, chatId, url } = getPopoutParams();

  // Hide sidebar, make workspace full width
  const sidebar = document.querySelector(".sidebar");
  const shell = document.querySelector(".shell");
  if (sidebar) sidebar.style.display = "none";
  if (shell) shell.style.gridTemplateColumns = "1fr";
  // No sidebar in pop-outs — hide the titlebar's settings/collapse cluster too.
  const titlebarActions = document.querySelector(".titlebar-actions");
  if (titlebarActions) titlebarActions.style.display = "none";

  await setupEventListeners();
  try { ui.loadedSettings = await invoke("get_app_settings"); } catch { ui.loadedSettings = {}; }
  ui.mapboxToken = ui.loadedSettings["maps/mapbox_api_key"] || null;

  try { ui.cachedAgents = await invoke("get_agents"); } catch {}

  if (type === "chat") {
    await addChatTile(chatId);
  } else if (type === "settings") {
    addTile("settings");
  } else if (type === "web") {
    addTile("web", { webUrl: url || "" });
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
  setupTileKeyboardNav();

  try { ui.loadedSettings = await invoke("get_app_settings"); } catch { ui.loadedSettings = {}; }
  ui.mapboxToken = ui.loadedSettings["maps/mapbox_api_key"] || null;
  updateModelChips();

  try { ui.cachedAgents = await invoke("get_agents"); } catch {}

  const saved = restoreLayout();
  if (saved && saved.length > 0) {
    for (const item of saved) {
      if (item.type === "chat") {
        await addChatTile(item.chatId || "", item.projectPath || "", { autoVoice: !!item.autoVoice });
      } else if (item.type === "settings") {
        addTile("settings");
      } else {
        addTile(item.type, { webUrl: item.webUrl || "" });
      }
    }
  } else {
    await addChatTile();
  }

  initSidebar();
}

(async () => {
  try {
    if (isQuickAsk()) {
      await bootQuickAsk();
    } else if (isPopout()) {
      await bootPopout();
    } else {
      await bootMain();
    }
  } catch (error) {
    console.error("Boot error:", error);
  }
})();
