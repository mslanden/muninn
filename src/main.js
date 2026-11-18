// Entry point: wires sidebar buttons and boots the window.
// Feature modules are imported for their side effect of registering tile renderers.
import "./theme.js";
import "./chat.js";
import "./logs.js";
import "./tasks.js";
import "./skills.js";
import "./memory.js";
import "./content.js";
import "./library.js";
import { openSettingsModal, closeSettingsModal, isSettingsModalOpen } from "./settings.js";
import { openConnections, closeConnections, isConnectionsOpen } from "./connections.js";

import { invoke } from "@tauri-apps/api/core";
import { getTileRenderer } from "./registry.js";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { workspace, newChatBtn, navSettingsBtn, ui } from "./state.js";
import { addTile, isPopout, getPopoutParams, restoreLayout, setupTileKeyboardNav, replaceSingleTileIfAlone, makeDragSource, openTileForType } from "./tilesys.js";
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

// Copy buttons on rendered code blocks (renderMarkdown emits them everywhere).
document.addEventListener("click", async (e) => {
  const btn = e.target.closest(".code-copy");
  if (!btn) return;
  const code = btn.closest(".code-block")?.querySelector("pre");
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code.innerText);
    btn.textContent = "Copied";
  } catch {
    btn.textContent = "Failed";
  }
  setTimeout(() => { btn.textContent = "Copy"; }, 1200);
});

navSettingsBtn.addEventListener("click", () => {
  if (isConnectionsOpen()) closeConnections();
  if (isSettingsModalOpen()) closeSettingsModal();
  else openSettingsModal();
});

document.getElementById("nav-connections")?.addEventListener("click", () => {
  if (isSettingsModalOpen()) closeSettingsModal();
  if (isConnectionsOpen()) closeConnections();
  else openConnections();
});

// "Connections" links inside other windows (e.g. the Settings model section).
document.addEventListener("click", (e) => {
  if (!e.target.closest("[data-open-connections]")) return;
  if (isSettingsModalOpen()) closeSettingsModal();
  openConnections();
});

// Library and Tasks open (or focus) their tile; both can also be dragged into the grid.
const navTasks = document.getElementById("nav-tasks");
if (navTasks) {
  navTasks.addEventListener("click", () => openTileForType("tasks"));
  makeDragSource(navTasks, { type: "tasks" });
}
const navLibrary = document.getElementById("nav-library");
if (navLibrary) {
  navLibrary.addEventListener("click", () => openTileForType("library"));
  makeDragSource(navLibrary, { type: "library" });
}

// ==================== BOOT ====================
async function bootPopout() {
  const { type, chatId } = getPopoutParams();

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

  try { ui.cachedSkills = await invoke("list_skills"); } catch {}

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
  setupTileKeyboardNav();

  try { ui.loadedSettings = await invoke("get_app_settings"); } catch { ui.loadedSettings = {}; }
  updateModelChips();

  try { ui.cachedSkills = await invoke("list_skills"); } catch {}

  const saved = restoreLayout();
  if (saved && saved.length > 0) {
    for (const item of saved) {
      if (item.type === "chat") {
        await addChatTile(item.chatId || "", item.projectPath || "", { autoVoice: !!item.autoVoice });
      } else if (item.type === "settings") {
        continue; // settings is a modal now — don't restore old settings tiles
      } else {
        // Layouts saved before agents became skills still name the old tile type.
        const type = item.type === "agents" ? "skills" : item.type;
        // Skip tiles whose feature has since been removed (e.g. the old Studio tile).
        if (!getTileRenderer(type)) continue;
        addTile(type);
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
