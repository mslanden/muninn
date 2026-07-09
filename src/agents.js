import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { ui } from "./state.js";
import { escapeHtml, field, parseAllowedTools } from "./utils.js";
import { refreshAgentsList } from "./sidebar.js";

// ========== AGENTS TILE ==========
export function renderAgentsTile(tileObj, body) {
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
    ui.cachedAgents = items;
    try { refreshAgentsList(); } catch { /* sidebar absent in pop-out */ }

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


registerTileRenderer("agents", renderAgentsTile);
