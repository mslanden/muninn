// ========== SCHEDULE WIDGET ==========
// Local date string YYYY-MM-DD (not UTC — schedules are local-time).
function todayLocalDate() {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

// options.allowOnce: when true, show a Recurring/Once toggle; in Once mode
// getData() returns { once: true, at: "<datetime-local value>" }. Defaults to
// false so existing consumers (feeds) keep the recurring-only behavior.
export function renderScheduleWidget(container, initialData, options = {}) {
  const allowOnce = !!options.allowOnce;
  const DAYS = [
    { key: "mon", label: "Mon", full: "Monday" },
    { key: "tue", label: "Tue", full: "Tuesday" },
    { key: "wed", label: "Wed", full: "Wednesday" },
    { key: "thu", label: "Thu", full: "Thursday" },
    { key: "fri", label: "Fri", full: "Friday" },
    { key: "sat", label: "Sat", full: "Saturday" },
    { key: "sun", label: "Sun", full: "Sunday" },
  ];

  const state = {
    mode: "recurring", // "recurring" | "once"
    onceAt: "",
    startDate: "",
    endDate: "",
    days: {}
  };

  container.innerHTML = `
    <div class="schedule-widget">
      ${allowOnce ? `
      <div class="schedule-mode-toggle">
        <button type="button" class="schedule-widget-mode active" data-sched-mode="recurring">Recurring</button>
        <button type="button" class="schedule-widget-mode" data-sched-mode="once">Once</button>
      </div>` : ""}
      <div class="schedule-recurring">
        <div class="schedule-dates">
          <div class="schedule-date-field">
            <label class="schedule-label">Start date</label>
            <input type="date" class="feeds-input schedule-start" />
          </div>
          <div class="schedule-date-field">
            <label class="schedule-label">End date <span class="schedule-opt">(optional)</span></label>
            <input type="date" class="feeds-input schedule-end" />
          </div>
        </div>
        <div class="schedule-days">
          ${DAYS.map(d => `<label class="schedule-day-btn" data-day="${d.key}"><input type="checkbox" data-day-check="${d.key}" /><span>${d.label}</span></label>`).join("")}
        </div>
        <div class="schedule-day-configs"></div>
      </div>
      ${allowOnce ? `
      <div class="schedule-once" style="display:none">
        <label class="schedule-label">Run at</label>
        <input type="datetime-local" class="feeds-input schedule-once-at" />
      </div>` : ""}
    </div>
  `;

  const startInput = container.querySelector(".schedule-start");
  const endInput = container.querySelector(".schedule-end");
  const configsContainer = container.querySelector(".schedule-day-configs");
  const dayBtns = container.querySelectorAll(".schedule-day-btn");
  const recurringSection = container.querySelector(".schedule-recurring");
  const onceSection = container.querySelector(".schedule-once");
  const onceInput = container.querySelector(".schedule-once-at");
  const modeBtns = container.querySelectorAll(".schedule-widget-mode");

  startInput.addEventListener("change", () => { state.startDate = startInput.value; });
  endInput.addEventListener("change", () => { state.endDate = endInput.value; });
  if (onceInput) onceInput.addEventListener("change", () => { state.onceAt = onceInput.value; });

  function applyMode() {
    if (!allowOnce) { state.mode = "recurring"; return; }
    recurringSection.style.display = state.mode === "once" ? "none" : "";
    if (onceSection) onceSection.style.display = state.mode === "once" ? "flex" : "none";
    modeBtns.forEach(b => b.classList.toggle("active", b.dataset.schedMode === state.mode));
  }

  modeBtns.forEach(btn => {
    btn.addEventListener("click", () => {
      state.mode = btn.dataset.schedMode === "once" ? "once" : "recurring";
      applyMode();
    });
  });

  function rebuildDayConfigs() {
    configsContainer.innerHTML = "";
    for (const d of DAYS) {
      if (!state.days[d.key]) continue;
      const cfg = state.days[d.key];
      const row = document.createElement("div");
      row.className = "schedule-day-row";
      const timesHtml = (cfg.times || ["09:00"]).map((t, i) =>
        `<input type="time" class="feeds-input schedule-time-input" data-day-time="${d.key}" data-time-idx="${i}" value="${t}" />` +
        `<button class="schedule-time-del" data-day-tdel="${d.key}" data-tdel-idx="${i}" title="Remove">&times;</button>`
      ).join("");
      row.innerHTML = `
        <span class="schedule-day-name">${d.full}</span>
        <select class="feeds-select schedule-mode-select" data-day-mode="${d.key}">
          <option value="times"${cfg.mode === "times" ? " selected" : ""}>Set times</option>
          <option value="continuous"${cfg.mode === "continuous" ? " selected" : ""}>Continuous</option>
        </select>
        <div class="schedule-times-wrap" style="display:${cfg.mode === "times" ? "flex" : "none"}">
          ${timesHtml}
          <button class="schedule-time-add" data-day-tadd="${d.key}" title="Add time">+</button>
        </div>
        <div class="schedule-interval-wrap" style="display:${cfg.mode === "continuous" ? "flex" : "none"}">
          <span class="schedule-label">every</span>
          <input type="number" class="feeds-input schedule-interval-input" data-day-interval="${d.key}" value="${cfg.intervalMins || 60}" min="1" />
          <span class="schedule-label">min</span>
        </div>
      `;
      configsContainer.appendChild(row);

      // Mode toggle
      row.querySelector(`[data-day-mode="${d.key}"]`).addEventListener("change", (e) => {
        cfg.mode = e.target.value;
        rebuildDayConfigs();
      });
      // Time inputs
      row.querySelectorAll(`[data-day-time="${d.key}"]`).forEach(inp => {
        inp.addEventListener("change", () => {
          const idx = parseInt(inp.dataset.timeIdx, 10);
          cfg.times[idx] = inp.value;
        });
      });
      // Delete time
      row.querySelectorAll(`[data-day-tdel="${d.key}"]`).forEach(btn => {
        btn.addEventListener("click", () => {
          const idx = parseInt(btn.dataset.tdelIdx, 10);
          cfg.times.splice(idx, 1);
          if (cfg.times.length === 0) cfg.times.push("09:00");
          rebuildDayConfigs();
        });
      });
      // Add time
      const addBtn = row.querySelector(`[data-day-tadd="${d.key}"]`);
      if (addBtn) {
        addBtn.addEventListener("click", () => {
          cfg.times.push("09:00");
          rebuildDayConfigs();
        });
      }
      // Interval input
      const intervalInput = row.querySelector(`[data-day-interval="${d.key}"]`);
      if (intervalInput) {
        intervalInput.addEventListener("change", () => {
          cfg.intervalMins = parseInt(intervalInput.value, 10) || 60;
        });
      }
    }
  }

  // Day toggle buttons
  dayBtns.forEach(btn => {
    const key = btn.dataset.day;
    const checkbox = btn.querySelector(`[data-day-check="${key}"]`);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) {
        state.days[key] = { mode: "times", times: ["09:00"], intervalMins: 60 };
        btn.classList.add("active");
        // Start-date trap: checking a day with no start date would produce a
        // schedule the backend silently never runs. Default to today.
        if (!state.startDate) {
          state.startDate = todayLocalDate();
          startInput.value = state.startDate;
        }
      } else {
        delete state.days[key];
        btn.classList.remove("active");
      }
      rebuildDayConfigs();
    });
  });

  function setData(data) {
    if (!data) return;
    if (data.once) {
      state.mode = "once";
      state.onceAt = data.at || "";
      if (onceInput) onceInput.value = state.onceAt;
      applyMode();
      return;
    }
    state.mode = "recurring";
    applyMode();
    state.startDate = data.startDate || "";
    state.endDate = data.endDate || "";
    startInput.value = state.startDate;
    endInput.value = state.endDate;
    state.days = {};
    dayBtns.forEach(btn => {
      const key = btn.dataset.day;
      const checkbox = btn.querySelector(`[data-day-check="${key}"]`);
      const dayCfg = data.days && data.days[key];
      if (dayCfg) {
        state.days[key] = { ...dayCfg };
        if (dayCfg.mode === "times" && !dayCfg.times) state.days[key].times = ["09:00"];
        checkbox.checked = true;
        btn.classList.add("active");
      } else {
        checkbox.checked = false;
        btn.classList.remove("active");
      }
    });
    rebuildDayConfigs();
  }

  function getData() {
    if (allowOnce && state.mode === "once") {
      if (!state.onceAt) return null;
      return { once: true, at: state.onceAt };
    }
    if (!state.startDate && Object.keys(state.days).length === 0) return null;
    const days = {};
    let anyDay = false;
    for (const d of DAYS) {
      if (state.days[d.key]) {
        anyDay = true;
        const cfg = state.days[d.key];
        if (cfg.mode === "times") {
          days[d.key] = { mode: "times", times: [...(cfg.times || ["09:00"])] };
        } else {
          days[d.key] = { mode: "continuous", intervalMins: cfg.intervalMins || 60 };
        }
      } else {
        days[d.key] = null;
      }
    }
    // Never emit an empty startDate with days selected — the backend would
    // silently never schedule the task.
    const startDate = state.startDate || (anyDay ? todayLocalDate() : "");
    return {
      version: 1,
      startDate,
      endDate: state.endDate || null,
      days
    };
  }

  function clear() {
    state.mode = "recurring";
    state.onceAt = "";
    if (onceInput) onceInput.value = "";
    applyMode();
    state.startDate = "";
    state.endDate = "";
    state.days = {};
    startInput.value = "";
    endInput.value = "";
    dayBtns.forEach(btn => {
      btn.classList.remove("active");
      btn.querySelector("input[type=checkbox]").checked = false;
    });
    rebuildDayConfigs();
  }

  if (initialData) setData(initialData);

  return { getData, setData, clear };
}
