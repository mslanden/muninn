import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { escapeHtml, field } from "./utils.js";
import { logError, emptyState } from "./uikit.js";

// ========== TOOLS TILE ==========
export function renderToolsTile(tileObj, body) {
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
    try {
      items = await invoke("get_tools");
    } catch (err) {
      logError("tools/get_tools", err);
      items = [];
    }
    const filtered = query ? items.filter(i => (field(i, "name", "name") || "").toLowerCase().includes(query)) : items;

    list.innerHTML = "";
    if (!filtered.length) {
      list.appendChild(emptyState({ icon: "⚙", text: query ? "No tools match your search" : "No tools" }));
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
          <input type="checkbox" ${enabled ? "checked" : ""} aria-label="Enable ${escapeHtml(name)}" />
          <span class="toggle-track"></span>
        </label>
      `;

      row.querySelector("input").addEventListener("change", async (e) => {
        const checked = e.target.checked;
        try {
          await invoke("set_tool_enabled", { req: { toolId, enabled: checked } });
        } catch (err) {
          logError("tools/set_tool_enabled", err);
          e.target.checked = !checked; // revert the toggle on failure
        }
      });

      list.appendChild(row);
    }
  }

  doRefresh();
}


registerTileRenderer("tools", renderToolsTile);
