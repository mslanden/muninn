import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { ui } from "./state.js";
import { escapeHtml, field, formatNextRun, renderMarkdown } from "./utils.js";
import { emptyState } from "./uikit.js";
import { renderScheduleWidget } from "./schedule.js";
import { addChatTile } from "./chat.js";

// Relative "time ago" for a past ISO timestamp.
function formatAgo(isoStr) {
  try {
    const d = new Date(isoStr);
    if (Number.isNaN(d.getTime())) return isoStr || "";
    const diff = Date.now() - d.getTime();
    if (diff < 60000) return "just now";
    if (diff < 3600000) return `${Math.round(diff / 60000)}m ago`;
    if (diff < 86400000) return `${Math.round(diff / 3600000)}h ago`;
    return `${Math.round(diff / 86400000)}d ago`;
  } catch { return isoStr || ""; }
}

// Last path component, tolerating trailing slashes.
function pathBasename(p) {
  const s = String(p).replace(/[\\/]+$/, "");
  const i = Math.max(s.lastIndexOf("/"), s.lastIndexOf("\\"));
  return i >= 0 ? s.slice(i + 1) : s;
}

// Resolve an agent id to its display name via the shared agents cache;
// falls back to the raw id when the agent is unknown.
function agentDisplayName(agentId) {
  const agents = Array.isArray(ui.cachedAgents) ? ui.cachedAgents : [];
  const a = agents.find(x => String(field(x, "agentId", "agent_id")) === String(agentId));
  return a ? field(a, "name", "name") : String(agentId);
}

// ========== TASKS TILE ==========
export function renderTasksTile(tileObj, body) {
  body.innerHTML = `
    <div class="tasks-toolbar">
      <button class="tasks-filter active" data-filter="all">All</button>
      <button class="tasks-filter" data-filter="scheduled">Scheduled</button>
      <button class="tasks-filter" data-filter="running">Running</button>
      <button class="tasks-filter" data-filter="paused">Paused</button>
      <button class="tasks-filter" data-filter="completed">Completed</button>
      <button class="tasks-filter" data-filter="failed">Failed</button>
      <span class="tasks-spacer"></span>
      <button class="tasks-new" data-tasks-new="${tileObj.id}">+ New</button>
    </div>
    <div class="task-form" data-task-form="${tileObj.id}">
      <input type="text" placeholder="Task name" data-task-name="${tileObj.id}" />
      <textarea rows="2" placeholder="Description" data-task-desc="${tileObj.id}"></textarea>
      <div class="task-schedule-container" data-task-schedule-widget="${tileObj.id}"></div>
      <label class="task-chain-label">When this finishes, run:</label>
      <div class="task-chain-options" data-task-chain="${tileObj.id}"></div>
      <label class="task-chain-label">Run only if (when triggered by another task):</label>
      <input type="text" placeholder="e.g. only if there are unread emails" data-task-condition="${tileObj.id}" />
      <label class="task-chain-label">Workspace (optional) <span class="task-form-hint">file &amp; shell tools scoped to this folder</span></label>
      <div class="task-workspace-row">
        <input type="text" readonly placeholder="No workspace (unscoped)" data-task-workspace="${tileObj.id}" />
        <button class="task-action-btn" data-task-workspace-pick="${tileObj.id}">Choose…</button>
        <button class="task-action-btn" data-task-workspace-clear="${tileObj.id}" title="Clear workspace">✕</button>
      </div>
      <label class="task-chain-label">Run as (optional)</label>
      <select data-task-agent="${tileObj.id}">
        <option value="">Default assistant</option>
      </select>
      <div class="form-row">
        <button class="form-btn" data-task-save="${tileObj.id}">Save Task</button>
        <button class="form-btn" data-task-reset="${tileObj.id}">Cancel</button>
      </div>
    </div>
    <div class="tasks-list" data-tasks-list="${tileObj.id}"></div>
  `;

  let currentFilter = "all";
  let editingTaskId = null;
  let allTasks = [];

  // Parse a stored chain_next value (JSON array of ids, or blank) into an array of numbers.
  function parseChain(raw) {
    if (!raw) return [];
    try {
      const arr = JSON.parse(raw);
      return Array.isArray(arr) ? arr.map(Number).filter(n => !Number.isNaN(n)) : [];
    } catch { return []; }
  }

  // Render the chain checkbox list: every other task is a candidate downstream step.
  function populateChainOptions(excludeId, selectedIds) {
    const box = body.querySelector(`[data-task-chain="${tileObj.id}"]`);
    const candidates = allTasks.filter(t => field(t, "id", "id") !== excludeId);
    if (!candidates.length) {
      box.innerHTML = `<span class="task-chain-empty">No other tasks to chain to yet.</span>`;
      return;
    }
    const selected = new Set(selectedIds);
    box.innerHTML = candidates.map(t => {
      const id = field(t, "id", "id");
      const name = field(t, "name", "name");
      return `<label class="task-chain-item">
        <input type="checkbox" value="${id}" ${selected.has(id) ? "checked" : ""} />
        ${escapeHtml(name)}
      </label>`;
    }).join("");
  }

  function selectedChainIds() {
    const box = body.querySelector(`[data-task-chain="${tileObj.id}"]`);
    return Array.from(box.querySelectorAll('input[type="checkbox"]:checked')).map(c => Number(c.value));
  }

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
  const condInput = body.querySelector(`[data-task-condition="${tileObj.id}"]`);
  const workspaceInput = body.querySelector(`[data-task-workspace="${tileObj.id}"]`);
  const workspacePickBtn = body.querySelector(`[data-task-workspace-pick="${tileObj.id}"]`);
  const workspaceClearBtn = body.querySelector(`[data-task-workspace-clear="${tileObj.id}"]`);
  const agentSelect = body.querySelector(`[data-task-agent="${tileObj.id}"]`);
  const saveBtn = body.querySelector(`[data-task-save="${tileObj.id}"]`);
  const resetBtn = body.querySelector(`[data-task-reset="${tileObj.id}"]`);
  const taskSchedContainer = body.querySelector(`[data-task-schedule-widget="${tileObj.id}"]`);
  const taskSchedWidget = renderScheduleWidget(taskSchedContainer, null, { allowOnce: true });

  workspacePickBtn.addEventListener("click", async () => {
    const selected = await openDialog({
      directory: true,
      multiple: false,
      title: "Choose workspace folder for this task",
    });
    if (!selected) return;
    workspaceInput.value = selected;
  });

  workspaceClearBtn.addEventListener("click", () => {
    workspaceInput.value = "";
  });

  // Fill the "Run as" select: default assistant + one option per enabled
  // custom agent (shared ui.cachedAgents cache, fetched on first use — same
  // pattern as sidebar.js). A selected agent that no longer exists/is
  // disabled still gets an option so editing doesn't silently drop it.
  async function populateAgentOptions(selectedId) {
    let agents = ui.cachedAgents;
    if (!Array.isArray(agents) || !agents.length) {
      try {
        agents = await invoke("get_agents");
        ui.cachedAgents = agents;
      } catch { agents = []; }
    }
    if (!Array.isArray(agents)) agents = [];
    const sel = String(selectedId || "");
    const enabled = agents.filter(a => field(a, "enabled", "enabled"));
    let found = !sel;
    const opts = [`<option value="">Default assistant</option>`];
    for (const a of enabled) {
      const aid = String(field(a, "agentId", "agent_id"));
      if (aid === sel) found = true;
      opts.push(`<option value="${escapeHtml(aid)}"${aid === sel ? " selected" : ""}>${escapeHtml(field(a, "name", "name"))}</option>`);
    }
    if (!found) {
      opts.push(`<option value="${escapeHtml(sel)}" selected>${escapeHtml(agentDisplayName(sel))}</option>`);
    }
    agentSelect.innerHTML = opts.join("");
  }

  newBtn.addEventListener("click", () => {
    editingTaskId = null;
    nameInput.value = "";
    descInput.value = "";
    condInput.value = "";
    workspaceInput.value = "";
    populateAgentOptions("");
    taskSchedWidget.clear();
    populateChainOptions(null, []);
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
    const schedData = taskSchedWidget.getData();
    // One-shot ("Once" mode) → nextRun (ISO local YYYY-MM-DDTHH:MM:SS) and NO
    // scheduleConfig; recurring → scheduleConfig as before.
    let scheduleConfig = "";
    let nextRun = "";
    if (schedData && schedData.once) {
      // datetime-local gives YYYY-MM-DDTHH:MM — normalize to include seconds.
      nextRun = schedData.at && schedData.at.length === 16 ? `${schedData.at}:00` : (schedData.at || "");
    } else if (schedData) {
      scheduleConfig = JSON.stringify(schedData);
    }
    const chainIds = selectedChainIds();
    const chainNext = chainIds.length ? JSON.stringify(chainIds) : "";
    const conditions = condInput.value.trim();
    const projectPath = workspaceInput.value.trim();
    const agentId = agentSelect.value;
    try {
      if (editingTaskId) {
        await invoke("update_task", {
          req: { id: editingTaskId, name, description: descInput.value.trim(), scheduleConfig, nextRun, chainNext, conditions, projectPath, agentId }
        });
      } else {
        await invoke("add_task", {
          req: { name, description: descInput.value.trim(), scheduleConfig, nextRun, chainNext, conditions, projectPath, agentId }
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
    allTasks = items;
    // Agent names on cards resolve through ui.cachedAgents — make sure it's
    // populated when any task targets a custom agent.
    if ((!Array.isArray(ui.cachedAgents) || !ui.cachedAgents.length)
        && items.some(t => field(t, "agentId", "agent_id"))) {
      try { ui.cachedAgents = await invoke("get_agents"); } catch {}
    }
    const nameById = new Map(items.map(t => [field(t, "id", "id"), field(t, "name", "name")]));
    const filtered = currentFilter === "all" ? items : items.filter(i => field(i, "status", "status") === currentFilter);

    list.innerHTML = "";
    if (!filtered.length) {
      list.replaceChildren(emptyState({ icon: "list-checks", text: "No tasks yet — schedule one with + New" }));
      return;
    }

    for (const item of filtered) {
      const id = field(item, "id", "id");
      const name = field(item, "name", "name");
      const description = field(item, "description", "description");
      const schedule = field(item, "schedule", "schedule");
      const scheduleConfig = field(item, "scheduleConfig", "schedule_config");
      const nextRun = field(item, "nextRun", "next_run");
      const status = field(item, "status", "status");
      const chainIds = parseChain(field(item, "chainNext", "chain_next"));
      const chainNames = chainIds.map(cid => nameById.get(cid)).filter(Boolean);
      const conditions = field(item, "conditions", "conditions") || "";
      const conditionStatus = field(item, "conditionStatus", "condition_status") || "";
      const projectPath = field(item, "projectPath", "project_path") || "";
      const agentId = field(item, "agentId", "agent_id") || "";
      const result = field(item, "result", "result") || "";

      // Build schedule summary for display
      let scheduleSummary = "";
      if (scheduleConfig) {
        try {
          const cfg = JSON.parse(scheduleConfig);
          const activeDays = Object.entries(cfg.days || {}).filter(([, v]) => v).map(([k]) => k);
          scheduleSummary = activeDays.length ? activeDays.join(", ") : "Scheduled";
        } catch { scheduleSummary = "Scheduled"; }
      } else if (schedule) {
        scheduleSummary = schedule;
      }

      // A scheduled task with no next_run: broken if it HAS a schedule (bad
      // start date), legitimate "manual / chain-only" if it has none.
      const hasSchedule = !!(scheduleConfig || schedule);
      let deadBadge = "";
      if (status === "scheduled" && !nextRun) {
        deadBadge = hasSchedule
          ? `<div class="task-dead-badge warn">⚠ will never run — check the start date</div>`
          : `<div class="task-dead-badge neutral">manual / chain-only</div>`;
      }

      // Pause/Resume toggle (scheduled ⇄ paused); completed/failed get a
      // "Re-arm" back to scheduled. `running` and `failed` are engine-managed
      // and never user-settable.
      let toggleBtn = "";
      if (status === "scheduled") {
        toggleBtn = `<button class="task-action-btn" data-action="toggle" data-target="paused">Pause</button>`;
      } else if (status === "paused") {
        toggleBtn = `<button class="task-action-btn" data-action="toggle" data-target="scheduled">Resume</button>`;
      } else if (status === "completed" || status === "failed") {
        toggleBtn = `<button class="task-action-btn" data-action="toggle" data-target="scheduled">Re-arm</button>`;
      }

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
            ${escapeHtml(scheduleSummary) || "No schedule"}
          </span>
          ${nextRun ? `<span>Next: ${escapeHtml(formatNextRun(nextRun))}</span>` : ""}
        </div>
        ${deadBadge}
        ${status === "failed" && result ? `<div class="task-fail-result">${escapeHtml(result.length > 400 ? `${result.slice(0, 400)}…` : result)}</div>` : ""}
        ${chainNames.length ? `<div class="task-chain-badge">→ then: ${escapeHtml(chainNames.join(", "))}</div>` : ""}
        ${conditions ? `<div class="task-condition">only if: ${escapeHtml(conditions)}${conditionStatus ? ` <span class="task-condition-status">(${escapeHtml(conditionStatus)})</span>` : ""}</div>` : ""}
        ${projectPath ? `<div class="task-workspace-badge" title="${escapeHtml(projectPath)}">in: ${escapeHtml(pathBasename(projectPath))}</div>` : ""}
        ${agentId ? `<div class="task-agent-badge">as: ${escapeHtml(agentDisplayName(agentId))}</div>` : ""}
        <div class="task-actions">
          <button class="task-action-btn" data-action="run" ${status === "running" ? "disabled" : ""}>▶ Run</button>
          ${toggleBtn}
          <button class="task-action-btn" data-action="history">History</button>
          <button class="task-action-btn" data-action="edit">Edit</button>
          <button class="task-action-btn danger" data-action="delete">Delete</button>
          <span class="task-run-error" data-run-error></span>
        </div>
        <div class="task-runs" data-task-runs hidden></div>
      `;

      card.querySelector('[data-action="edit"]').addEventListener("click", () => {
        editingTaskId = id;
        nameInput.value = name;
        descInput.value = description || "";
        if (scheduleConfig) {
          try { taskSchedWidget.setData(JSON.parse(scheduleConfig)); } catch { taskSchedWidget.clear(); }
        } else if (!schedule && nextRun) {
          // One-shot task: nextRun but no schedule/scheduleConfig → Once mode.
          // datetime-local wants YYYY-MM-DDTHH:MM.
          taskSchedWidget.clear();
          taskSchedWidget.setData({ once: true, at: String(nextRun).slice(0, 16) });
        } else {
          taskSchedWidget.clear();
        }
        populateChainOptions(id, chainIds);
        condInput.value = conditions;
        workspaceInput.value = projectPath;
        populateAgentOptions(agentId);
        saveBtn.textContent = "Update Task";
        form.classList.add("visible");
      });

      const runBtn = card.querySelector('[data-action="run"]');
      const runErrEl = card.querySelector("[data-run-error]");
      runBtn.addEventListener("click", async () => {
        runBtn.disabled = true;
        runErrEl.textContent = "";
        try {
          await invoke("run_task_now", { req: { id } });
          doRefresh();
        } catch (err) {
          console.error("Run task error:", err);
          runErrEl.textContent = String(err);
          runBtn.disabled = false;
          setTimeout(() => { runErrEl.textContent = ""; }, 6000);
        }
      });

      const toggle = card.querySelector('[data-action="toggle"]');
      if (toggle) {
        toggle.addEventListener("click", async () => {
          // Only scheduled/completed/paused are user-settable; the toggle only
          // ever targets scheduled or paused.
          try { await invoke("set_task_status", { req: { id, status: toggle.dataset.target } }); } catch (err) { console.error("Set status error:", err); }
          doRefresh();
        });
      }

      const runsPanel = card.querySelector("[data-task-runs]");
      card.querySelector('[data-action="history"]').addEventListener("click", async () => {
        if (!runsPanel.hidden) { runsPanel.hidden = true; return; }
        runsPanel.hidden = false;
        runsPanel.innerHTML = `<div class="task-runs-empty">Loading…</div>`;
        let runs;
        try { runs = await invoke("get_task_runs", { taskId: id }); } catch { runs = []; }
        if (!runs.length) {
          runsPanel.innerHTML = `<div class="task-runs-empty">No runs yet</div>`;
          return;
        }
        runsPanel.innerHTML = runs.map((r, i) => {
          const status = field(r, "status", "status");
          const kind = field(r, "triggerKind", "trigger_kind");
          const by = field(r, "triggeredBy", "triggered_by");
          const note = field(r, "note", "note") || "";
          const out = field(r, "output", "output") || "";
          const finished = field(r, "finishedAt", "finished_at");
          const trig = kind === "chain"
            ? `← ${escapeHtml(nameById.get(by) || `task #${by}`)}`
            : "scheduled";
          return `<div class="task-run" data-run="${i}">
            <div class="task-run-head">
              <span class="task-run-dot ${escapeHtml(status)}"></span>
              <span class="task-run-trig">${trig}</span>
              <span class="task-run-time">${escapeHtml(formatAgo(finished))}</span>
            </div>
            ${note ? `<div class="task-run-note">${escapeHtml(note)}</div>` : ""}
            <div class="task-run-out" data-run-out hidden>${renderMarkdown(out)}</div>
          </div>`;
        }).join("");
        // Expand/collapse output on click; offer a transcript link per run.
        runsPanel.querySelectorAll(".task-run").forEach((el, i) => {
          const outEl = el.querySelector("[data-run-out]");
          el.querySelector(".task-run-head").addEventListener("click", () => {
            outEl.hidden = !outEl.hidden;
          });
          const sid = field(runs[i], "sessionId", "session_id");
          if (sid) {
            const link = document.createElement("button");
            link.className = "task-run-open";
            link.textContent = "Open transcript";
            link.addEventListener("click", (e) => { e.stopPropagation(); addChatTile(sid, ""); });
            el.appendChild(link);
          }
        });
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


registerTileRenderer("tasks", renderTasksTile);
