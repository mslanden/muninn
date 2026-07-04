import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { escapeHtml, field, formatNextRun, renderMarkdown } from "./utils.js";
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

// ========== TASKS TILE ==========
export function renderTasksTile(tileObj, body) {
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
      <div class="task-schedule-container" data-task-schedule-widget="${tileObj.id}"></div>
      <label class="task-chain-label">When this finishes, run:</label>
      <div class="task-chain-options" data-task-chain="${tileObj.id}"></div>
      <label class="task-chain-label">Run only if (when triggered by another task):</label>
      <input type="text" placeholder="e.g. only if there are unread emails" data-task-condition="${tileObj.id}" />
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
  const saveBtn = body.querySelector(`[data-task-save="${tileObj.id}"]`);
  const resetBtn = body.querySelector(`[data-task-reset="${tileObj.id}"]`);
  const taskSchedContainer = body.querySelector(`[data-task-schedule-widget="${tileObj.id}"]`);
  const taskSchedWidget = renderScheduleWidget(taskSchedContainer, null);

  newBtn.addEventListener("click", () => {
    editingTaskId = null;
    nameInput.value = "";
    descInput.value = "";
    condInput.value = "";
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
    const scheduleConfig = schedData ? JSON.stringify(schedData) : "";
    const chainIds = selectedChainIds();
    const chainNext = chainIds.length ? JSON.stringify(chainIds) : "";
    const conditions = condInput.value.trim();
    try {
      if (editingTaskId) {
        await invoke("update_task", {
          req: { id: editingTaskId, name, description: descInput.value.trim(), scheduleConfig, chainNext, conditions }
        });
      } else {
        await invoke("add_task", {
          req: { name, description: descInput.value.trim(), scheduleConfig, chainNext, conditions }
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
    const nameById = new Map(items.map(t => [field(t, "id", "id"), field(t, "name", "name")]));
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
      const scheduleConfig = field(item, "scheduleConfig", "schedule_config");
      const nextRun = field(item, "nextRun", "next_run");
      const status = field(item, "status", "status");
      const chainIds = parseChain(field(item, "chainNext", "chain_next"));
      const chainNames = chainIds.map(cid => nameById.get(cid)).filter(Boolean);
      const conditions = field(item, "conditions", "conditions") || "";
      const conditionStatus = field(item, "conditionStatus", "condition_status") || "";

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
          ${nextRun ? `<span>Next: ${formatNextRun(nextRun)}</span>` : ""}
        </div>
        ${chainNames.length ? `<div class="task-chain-badge">→ then: ${escapeHtml(chainNames.join(", "))}</div>` : ""}
        ${conditions ? `<div class="task-condition">only if: ${escapeHtml(conditions)}${conditionStatus ? ` <span class="task-condition-status">(${escapeHtml(conditionStatus)})</span>` : ""}</div>` : ""}
        <div class="task-actions">
          <select class="task-status-select" data-action="status">
            <option value="scheduled" ${status === "scheduled" ? "selected" : ""}>scheduled</option>
            <option value="running" ${status === "running" ? "selected" : ""}>running</option>
            <option value="completed" ${status === "completed" ? "selected" : ""}>completed</option>
          </select>
          <button class="task-action-btn" data-action="history">History</button>
          <button class="task-action-btn" data-action="edit">Edit</button>
          <button class="task-action-btn danger" data-action="delete">Delete</button>
        </div>
        <div class="task-runs" data-task-runs hidden></div>
      `;

      card.querySelector('[data-action="edit"]').addEventListener("click", () => {
        editingTaskId = id;
        nameInput.value = name;
        descInput.value = description || "";
        if (scheduleConfig) {
          try { taskSchedWidget.setData(JSON.parse(scheduleConfig)); } catch { taskSchedWidget.clear(); }
        } else {
          taskSchedWidget.clear();
        }
        populateChainOptions(id, chainIds);
        condInput.value = conditions;
        saveBtn.textContent = "Update Task";
        form.classList.add("visible");
      });

      card.querySelector('[data-action="status"]').addEventListener("change", async (e) => {
        try { await invoke("set_task_status", { req: { id, status: e.target.value } }); } catch {}
        doRefresh();
      });

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
