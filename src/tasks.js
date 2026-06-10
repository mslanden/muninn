import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { escapeHtml, field, formatNextRun } from "./utils.js";
import { renderScheduleWidget } from "./schedule.js";

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
      <div class="form-row">
        <button class="form-btn" data-task-save="${tileObj.id}">Save Task</button>
        <button class="form-btn" data-task-reset="${tileObj.id}">Cancel</button>
      </div>
    </div>
    <div class="tasks-list" data-tasks-list="${tileObj.id}"></div>
  `;

  let currentFilter = "all";
  let editingTaskId = null;

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
  const saveBtn = body.querySelector(`[data-task-save="${tileObj.id}"]`);
  const resetBtn = body.querySelector(`[data-task-reset="${tileObj.id}"]`);
  const taskSchedContainer = body.querySelector(`[data-task-schedule-widget="${tileObj.id}"]`);
  const taskSchedWidget = renderScheduleWidget(taskSchedContainer, null);

  newBtn.addEventListener("click", () => {
    editingTaskId = null;
    nameInput.value = "";
    descInput.value = "";
    taskSchedWidget.clear();
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
    try {
      if (editingTaskId) {
        await invoke("update_task", {
          req: { id: editingTaskId, name, description: descInput.value.trim(), scheduleConfig }
        });
      } else {
        await invoke("add_task", {
          req: { name, description: descInput.value.trim(), scheduleConfig }
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
        <div class="task-actions">
          <select class="task-status-select" data-action="status">
            <option value="scheduled" ${status === "scheduled" ? "selected" : ""}>scheduled</option>
            <option value="running" ${status === "running" ? "selected" : ""}>running</option>
            <option value="completed" ${status === "completed" ? "selected" : ""}>completed</option>
          </select>
          <button class="task-action-btn" data-action="edit">Edit</button>
          <button class="task-action-btn danger" data-action="delete">Delete</button>
        </div>
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
        saveBtn.textContent = "Update Task";
        form.classList.add("visible");
      });

      card.querySelector('[data-action="status"]').addEventListener("change", async (e) => {
        try { await invoke("set_task_status", { req: { id, status: e.target.value } }); } catch {}
        doRefresh();
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
