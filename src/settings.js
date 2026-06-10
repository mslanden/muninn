import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { enable as enableAutostart, disable as disableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
import { workspace, sidebarWorkspace, sidebarModel, ui } from "./state.js";
import { applyTheme } from "./theme.js";
import { field, projectLabel } from "./utils.js";

// ========== SETTINGS TILE ==========
export function renderSettingsTile(tileObj, body) {
  body.innerHTML = `
    <div class="settings-section">
      <div class="settings-group">
        <h3>Profile</h3>
        <input type="text" placeholder="Your name" data-s-name="${tileObj.id}" style="margin-bottom:6px;" />
        <textarea rows="2" placeholder="About you" data-s-about="${tileObj.id}"></textarea>
        <input type="text" placeholder="Your location (e.g. Mountain Center, CA)" data-s-location="${tileObj.id}" style="margin-top:6px;" />
      </div>
      <div class="settings-group">
        <h3>Model</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Model name (e.g. MiniMax-M2.7)" data-s-model="${tileObj.id}" style="flex:1;" />
        </div>
      </div>
      <div class="settings-group">
        <h3>API Keys</h3>
        <input type="password" placeholder="OpenAI API key" data-s-openai="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="MiniMax API key" data-s-minimax="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="Web search API key" data-s-search="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="Together AI key (images)" data-s-together="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="Mapbox API key (maps)" data-s-mapbox="${tileObj.id}" style="margin-bottom:6px;" />
      </div>
      <div class="settings-group">
        <h3>Text to Speech</h3>
        <input type="text" placeholder="MiniMax Voice ID (e.g. English_Graceful_Lady)" data-s-voiceid="${tileObj.id}" style="margin-bottom:6px;" />
        <span class="muted" style="font-size:10px;">Uses MiniMax T2A. Leave blank for default voice.</span>
      </div>
      <div class="settings-group">
        <h3>Image Generation</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <select data-s-imgprov="${tileObj.id}">
            <option value="">None</option>
            <option value="minimax">MiniMax</option>
            <option value="together">Together</option>
            <option value="openai">OpenAI</option>
          </select>
          <select data-s-imgmodel="${tileObj.id}" style="flex:1;"></select>
          <button class="form-btn" data-s-fetch-img-models="${tileObj.id}">Fetch</button>
        </div>
        <span class="muted" data-s-img-status="${tileObj.id}" style="font-size:10px;"></span>
      </div>
      <div class="settings-group">
        <h3>Workspace</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Workspace path" data-s-workspace="${tileObj.id}" />
          <button class="form-btn" data-s-setws="${tileObj.id}">Set</button>
          <button class="form-btn" data-s-savews="${tileObj.id}">Save</button>
        </div>
        <select data-s-saved-ws="${tileObj.id}" style="margin-bottom:6px;">
          <option value="">Saved workspaces</option>
        </select>
        <div class="form-row" style="margin-bottom:6px;">
          <button class="form-btn" data-s-usews="${tileObj.id}">Use Selected</button>
          <button class="form-btn danger" data-s-rmws="${tileObj.id}">Remove</button>
        </div>
      </div>
      <div class="settings-group">
        <h3>Appearance</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <select data-s-theme="${tileObj.id}">
            <option value="dark">Dark</option>
            <option value="light">Light</option>
            <option value="system">System</option>
          </select>
        </div>
      </div>
      <div class="settings-group">
        <h3>System</h3>
        <label class="checkbox-inline">
          <input type="checkbox" data-s-autostart="${tileObj.id}" /> Launch at login
        </label>
      </div>
      <div class="form-row" style="margin-top:12px;">
        <button class="form-btn" data-s-save="${tileObj.id}">Save Settings</button>
        <span class="muted" data-s-status="${tileObj.id}"></span>
      </div>
    </div>
  `;

  const nameInput = body.querySelector(`[data-s-name="${tileObj.id}"]`);
  const aboutInput = body.querySelector(`[data-s-about="${tileObj.id}"]`);
  const locationInput = body.querySelector(`[data-s-location="${tileObj.id}"]`);
  const modelSelect = body.querySelector(`[data-s-model="${tileObj.id}"]`);
  const openaiInput = body.querySelector(`[data-s-openai="${tileObj.id}"]`);
  const minimaxInput = body.querySelector(`[data-s-minimax="${tileObj.id}"]`);
  const searchInput = body.querySelector(`[data-s-search="${tileObj.id}"]`);
  const togetherInput = body.querySelector(`[data-s-together="${tileObj.id}"]`);
  const mapboxInput = body.querySelector(`[data-s-mapbox="${tileObj.id}"]`);
  const imgProvSelect = body.querySelector(`[data-s-imgprov="${tileObj.id}"]`);
  const imgModelSelect = body.querySelector(`[data-s-imgmodel="${tileObj.id}"]`);
  const wsInput = body.querySelector(`[data-s-workspace="${tileObj.id}"]`);
  const savedWsSelect = body.querySelector(`[data-s-saved-ws="${tileObj.id}"]`);
  const voiceidInput = body.querySelector(`[data-s-voiceid="${tileObj.id}"]`);
  const autostartInput = body.querySelector(`[data-s-autostart="${tileObj.id}"]`);
  const themeSelect = body.querySelector(`[data-s-theme="${tileObj.id}"]`);
  const statusEl = body.querySelector(`[data-s-status="${tileObj.id}"]`);
  const imgStatusEl = body.querySelector(`[data-s-img-status="${tileObj.id}"]`);

  function ensureOption(select, value) {
    if (!value) return;
    const exists = Array.from(select.options).some(o => o.value === value);
    if (!exists) {
      const opt = document.createElement("option");
      opt.value = value;
      opt.textContent = value;
      select.appendChild(opt);
    }
    select.value = value;
  }

  // Load settings
  async function loadS() {
    const s = await invoke("get_app_settings");
    ui.loadedSettings = s;
    nameInput.value = s["user/name"] || "";
    aboutInput.value = s["user/about"] || "";
    locationInput.value = s["user/location"] || "";

    const currentModel = s["llm/model"] || "MiniMax-M2.7";
    modelSelect.value = currentModel;

    openaiInput.value = "";
    minimaxInput.value = "";
    searchInput.value = "";
    togetherInput.value = "";
    mapboxInput.value = "";
    openaiInput.placeholder = s["llm/openai_api_key"] ? "OpenAI key (saved)" : "OpenAI API key";
    minimaxInput.placeholder = s["llm/minimax_api_key"] ? "MiniMax key (saved)" : "MiniMax API key";
    searchInput.placeholder = s["search/brave_api_key"] ? "Search key (saved)" : "Web search key";
    togetherInput.placeholder = s["imagegen/together_api_key"] ? "Together key (saved)" : "Together AI key";
    mapboxInput.placeholder = s["maps/mapbox_api_key"] ? "Mapbox key (saved)" : "Mapbox API key (maps)";
    voiceidInput.value = s["tts/voice_id"] || "";
    imgProvSelect.value = s["imagegen/provider"] || "";

    const currentImgModel = s["imagegen/model"] || "";
    ensureOption(imgModelSelect, currentImgModel);

    sidebarModel.textContent = currentModel;

    try { autostartInput.checked = await isAutostartEnabled(); } catch {}

    themeSelect.value = localStorage.getItem("muninn-theme") || "dark";

    // Workspace
    const pathInfo = await invoke("get_workspace_root");
    const workspaceRoot = field(pathInfo, "workspaceRoot", "workspace_root");
    wsInput.value = workspaceRoot;
    sidebarWorkspace.textContent = projectLabel(workspaceRoot);

    // Saved workspaces
    const items = await invoke("list_saved_workspaces");
    savedWsSelect.innerHTML = '<option value="">Saved workspaces</option>';
    for (const item of items) {
      const path = field(item, "path", "path");
      const opt = document.createElement("option");
      opt.value = path;
      opt.textContent = path;
      savedWsSelect.appendChild(opt);
    }
    if (workspaceRoot) savedWsSelect.value = workspaceRoot;
  }

  // Save
  body.querySelector(`[data-s-save="${tileObj.id}"]`).addEventListener("click", async () => {
    const updates = {
      "user/name": nameInput.value.trim(),
      "user/about": aboutInput.value.trim(),
      "user/location": locationInput.value.trim(),
      "llm/provider": "minimax",
      "llm/model": modelSelect.value.trim(),
      "imagegen/provider": imgProvSelect.value.trim(),
      "imagegen/model": imgModelSelect.value.trim()
    };
    if (openaiInput.value.trim()) updates["llm/openai_api_key"] = openaiInput.value.trim();
    if (minimaxInput.value.trim()) updates["llm/minimax_api_key"] = minimaxInput.value.trim();
    if (searchInput.value.trim()) updates["search/brave_api_key"] = searchInput.value.trim();
    if (togetherInput.value.trim()) updates["imagegen/together_api_key"] = togetherInput.value.trim();
    if (mapboxInput.value.trim()) {
      updates["maps/mapbox_api_key"] = mapboxInput.value.trim();
      ui.mapboxToken = mapboxInput.value.trim();
    }
    updates["tts/voice_id"] = voiceidInput.value.trim();

    await invoke("set_app_settings", { req: { settings: updates } });
    // Geocode location if changed and Mapbox key available
    const loc = locationInput.value.trim();
    const token = ui.mapboxToken || updates["maps/mapbox_api_key"];
    if (loc && token) {
      try {
        await invoke("geocode_user_location", { location: loc });
        statusEl.textContent = "Saved (location set ✓)";
      } catch (e) {
        console.warn("Location geocoding failed:", e);
        statusEl.textContent = "Saved (location failed: " + e + ")";
      }
    } else if (loc && !token) {
      statusEl.textContent = "Saved (add Mapbox key for location)";
    } else {
      statusEl.textContent = "Saved";
    }
    ui.loadedSettings = { ...ui.loadedSettings, ...updates };
    sidebarModel.textContent = updates["llm/model"];
    setTimeout(() => { statusEl.textContent = ""; }, 2000);
    await loadS();
  });

  // Known image models per provider (for providers without a fetch API)
  const KNOWN_IMG_MODELS = {
    minimax: [{ id: "image-01", label: "image-01" }, { id: "image-01-live", label: "image-01-live" }],
  };

  function populateImgModels(models, keepCurrent) {
    const currentVal = keepCurrent ? imgModelSelect.value : "";
    imgModelSelect.innerHTML = '<option value="">Default</option>';
    for (const m of models) {
      const opt = document.createElement("option");
      opt.value = m.id;
      opt.textContent = m.label || m.id;
      imgModelSelect.appendChild(opt);
    }
    if (models.some(m => m.id === currentVal)) {
      imgModelSelect.value = currentVal;
    }
  }

  // When provider changes, reset model list
  imgProvSelect.addEventListener("change", () => {
    const prov = imgProvSelect.value;
    const known = KNOWN_IMG_MODELS[prov];
    if (known) {
      populateImgModels(known, false);
    } else {
      imgModelSelect.innerHTML = '<option value="">Default</option>';
    }
  });

  // Fetch image models
  body.querySelector(`[data-s-fetch-img-models="${tileObj.id}"]`).addEventListener("click", async () => {
    const prov = imgProvSelect.value;
    if (!prov) { imgStatusEl.textContent = "Select a provider first"; return; }

    // Use known models for providers without a fetch API
    const known = KNOWN_IMG_MODELS[prov];
    if (known) {
      populateImgModels(known, true);
      imgStatusEl.textContent = `${known.length} models loaded`;
      setTimeout(() => { imgStatusEl.textContent = ""; }, 3000);
      return;
    }

    imgStatusEl.textContent = "Fetching...";
    try {
      const models = await invoke("fetch_models", { provider: prov });
      // Filter to image-related models by type or name
      const imgModels = models.filter(m => {
        if (m.type === "image") return true;
        const id = (m.id || "").toLowerCase();
        return id.includes("image") || id.includes("flux") || id.includes("dall") ||
               id.includes("stable") || id.includes("sdxl") || id.includes("qwen-image");
      });
      const useModels = imgModels.length > 0 ? imgModels : models;
      populateImgModels(useModels, true);
      imgStatusEl.textContent = `${useModels.length} models loaded`;
      setTimeout(() => { imgStatusEl.textContent = ""; }, 3000);
    } catch (e) {
      imgStatusEl.textContent = String(e);
    }
  });

  // Workspace controls
  body.querySelector(`[data-s-setws="${tileObj.id}"]`).addEventListener("click", async () => {
    try {
      const result = await invoke("set_workspace_root", { req: { workspaceRoot: wsInput.value.trim() } });
      wsInput.value = field(result, "workspaceRoot", "workspace_root");
      sidebarWorkspace.textContent = projectLabel(wsInput.value);
      statusEl.textContent = "Workspace set";
      setTimeout(() => { statusEl.textContent = ""; }, 2000);
    } catch (e) { statusEl.textContent = String(e); }
  });

  body.querySelector(`[data-s-savews="${tileObj.id}"]`).addEventListener("click", async () => {
    const path = wsInput.value.trim();
    if (!path) return;
    await invoke("save_workspace", { req: { path } });
    await loadS();
    statusEl.textContent = "Workspace saved";
    setTimeout(() => { statusEl.textContent = ""; }, 2000);
  });

  body.querySelector(`[data-s-usews="${tileObj.id}"]`).addEventListener("click", async () => {
    const selected = savedWsSelect.value;
    if (!selected) return;
    const result = await invoke("set_workspace_root", { req: { workspaceRoot: selected } });
    wsInput.value = field(result, "workspaceRoot", "workspace_root");
    sidebarWorkspace.textContent = projectLabel(wsInput.value);
  });

  body.querySelector(`[data-s-rmws="${tileObj.id}"]`).addEventListener("click", async () => {
    const selected = savedWsSelect.value;
    if (!selected) return;
    await invoke("remove_saved_workspace", { req: { path: selected } });
    await loadS();
  });

  // Theme
  themeSelect.addEventListener("change", () => {
    const val = themeSelect.value;
    localStorage.setItem("muninn-theme", val);
    applyTheme(val);
  });

  // Autostart
  autostartInput.addEventListener("change", async () => {
    try {
      if (autostartInput.checked) await enableAutostart();
      else await disableAutostart();
    } catch (e) {
      autostartInput.checked = !autostartInput.checked;
    }
  });

  loadS();
}


registerTileRenderer("settings", renderSettingsTile);
