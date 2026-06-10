import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { escapeHtml, field, projectLabel } from "./utils.js";
import { addProjectChatTile } from "./chat.js";

// ========== WORKSPACES TILE ==========
export function renderWorkspacesTile(tileObj, body) {
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
        const agentsMd = field(p, "agentsMd", "agents_md") || "";
        const lastOpened = field(p, "lastOpenedAt", "last_opened_at") || "";
        const row = document.createElement("div");
        row.className = "ws-row";
        row.innerHTML = `
          <div class="ws-info">
            <strong class="ws-name">${escapeHtml(name)}</strong>
            <span class="ws-path">${escapeHtml(path)}</span>
            ${agentsMd ? '<span class="ws-has-agents">AGENTS.md</span>' : ""}
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


registerTileRenderer("workspaces", renderWorkspacesTile);
