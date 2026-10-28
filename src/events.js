import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { tiles, chatStreams, tileMaps, ui } from "./state.js";
import { escapeHtml, renderMarkdown } from "./utils.js";
import { getTilesByType, getChatTileBySessionId } from "./tilesys.js";
import { playTts, maybeAutoSpeak, updateModelChips, addToolCard, renderTodoPanel, showUserQuestion, renderDeferredCard, showThinking, hideThinking, updateContextBar, onVoiceRecorded, onVoiceError, setGoalBannerStatus, hideGoalBanner, addTurn } from "./chat.js";
import { renderTasksTile } from "./tasks.js";
import { renderMemoryTile } from "./memory.js";
import { renderContentTile } from "./content.js";
import { isDarkTheme, applyTheme } from "./theme.js";
import { renderSettingsTile, refreshSettingsModalIfOpen } from "./settings.js";
import { renderAgentsTile } from "./agents.js";

// ==================== TAURI EVENT LISTENERS ====================
export async function setupEventListeners() {
  // Chat token - route to the correct chat tile by session_id
  await listen("chat-token", (event) => {
    const { session_id, token } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;

    hideThinking(tile.id);

    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    if (!stream.bubble) {
      const turn = document.createElement("div");
      turn.className = "turn";
      const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      turn.innerHTML = `
        <div class="turn-header">
          <span class="turn-label muninn">Muninn</span>
          <span class="turn-time">${time}</span>
        </div>
        <div class="turn-body"></div>
      `;
      chatLog.appendChild(turn);
      stream.bubble = turn.querySelector(".turn-body");
    }

    stream.text += token;
    stream.bubble.innerHTML = renderMarkdown(stream.text);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
  });

  // Tool execution events — show tool cards in the chat stream
  await listen("tool-executed", (event) => {
    const { session_id, tool, args, status } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;

    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    // Finalize any open text bubble before showing a tool card
    if (stream.bubble && status === "running") {
      stream.bubble.innerHTML = renderMarkdown(stream.text);
      stream.bubble = null;
      stream.text = "";
    }

    if (status === "running") {
      const card = addToolCard(chatLog, tool, args || "{}", "running");
      stream.toolElement = card;
    } else if (stream.toolElement) {
      const badge = stream.toolElement.querySelector(".status-badge");
      if (badge) badge.textContent = status;
      const dot = stream.toolElement.querySelector(".dot");
      if (dot) dot.classList.remove("running");
      stream.toolElement = null;
    }
  });

  await listen("chat-done", (event) => {
    const { session_id } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;

    if (stream.bubble) {
      const finalText = stream.text;
      stream.bubble.innerHTML = renderMarkdown(finalText);
      // Add TTS button to the turn header
      const turnEl = stream.bubble.closest(".turn");
      if (turnEl) {
        const header = turnEl.querySelector(".turn-header");
        if (header && !header.querySelector(".tts-btn")) {
          const btn = document.createElement("button");
          btn.className = "tts-btn";
          btn.title = "Read aloud";
          btn.innerHTML = "&#9654;";
          btn.addEventListener("click", () => playTts(btn, finalText));
          header.appendChild(btn);
        }
      }
      stream.bubble = null;
      stream.text = "";
      maybeAutoSpeak(tile, finalText);
    }
    hideThinking(tile.id);
  });

  // Voice input: native recorder emits the WAV here; route it to the recording tile's input.
  await listen("voice-recorded", (event) => {
    onVoiceRecorded(event.payload);
  });
  await listen("voice-error", (event) => {
    onVoiceError(event.payload);
  });

  await listen("chat-usage", (event) => {
    const { session_id, input_tokens, output_tokens } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;
    const total = input_tokens + output_tokens;
    stream.contextTokens = total;
    updateContextBar(tile.id, total);
  });

  await listen("tool-start", (event) => {
    const { sessionId, toolName, arguments: args } = event.payload;
    const tile = getChatTileBySessionId(sessionId);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    if (stream.toolElement) {
      stream.toolElement.remove();
    }
    stream.toolElement = addToolCard(chatLog, toolName, args, "running");
  });

  await listen("tool-complete", (event) => {
    const { sessionId, toolName, output, status } = event.payload;
    const tile = getChatTileBySessionId(sessionId);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;

    if (stream.toolElement) {
      stream.toolElement.remove();
      stream.toolElement = null;
    }

    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (chatLog) {
      addToolCard(chatLog, toolName, "", status || "done");
    }
  });

  await listen("agent-iteration", (event) => {
    const { sessionId, agentName, iteration, maxIterations } = event.payload;
    const tile = getChatTileBySessionId(sessionId);
    if (!tile) return;
    showThinking(tile.id, `${agentName} (${iteration}/${maxIterations})`);
  });

  // Goal auto-continue loop: update the banner's status/counter on every bound tile.
  await listen("goal-iteration", (event) => {
    const { session_id, iteration, reason, done } = event.payload;
    const status = done
      ? String(reason || "")
      : `iteration ${iteration}: ${String(reason || "")}`;
    tiles
      .filter(t => t.type === "chat" && chatStreams[t.id]?.sessionId === session_id)
      .forEach(t => setGoalBannerStatus(t, status));
  });

  // Goal met: hide the banner and append a system-style line to each bound tile's log.
  await listen("goal-met", (event) => {
    const { session_id, reason } = event.payload;
    tiles
      .filter(t => t.type === "chat" && chatStreams[t.id]?.sessionId === session_id)
      .forEach(t => {
        hideGoalBanner(t);
        const chatLog = t.el.querySelector(`[data-chat-stream="${t.id}"]`);
        if (chatLog) addTurn(chatLog, "muninn", `🎯 Goal met: ${String(reason || "")}`);
      });
  });

  await listen("map-places", (event) => {
    const { session_id, query, results } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    const mapId = "map-" + Date.now();
    const el = document.createElement("div");
    el.className = "turn";
    el.innerHTML = `
      <div class="turn-body">
        <div class="map-title">Places: ${escapeHtml(query)}</div>
        <div id="${mapId}" class="map-container"></div>
        <div class="map-results">
          ${results.map((r, i) => `<div class="map-result">${i + 1}. <strong>${escapeHtml(r.name)}</strong></div>`).join("")}
        </div>
      </div>
    `;
    chatLog.appendChild(el);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;

    if (!ui.mapboxToken) return;
    setTimeout(() => {
      const isDark = isDarkTheme();
      const style = isDark ? "mapbox://styles/mapbox/dark-v11" : "mapbox://styles/mapbox/streets-v12";
      const map = new mapboxgl.Map({
        container: mapId,
        style,
        center: [results[0]?.lon || -117, results[0]?.lat || 34],
        zoom: 13,
        accessToken: ui.mapboxToken,
        attributionControl: false,
      });
      const bounds = new mapboxgl.LngLatBounds();
      results.forEach(r => {
        const lon = parseFloat(r.lon), lat = parseFloat(r.lat);
        if (lat === 0 && lon === 0) return;
        new mapboxgl.Marker()
          .setLngLat([lon, lat])
          .setPopup(new mapboxgl.Popup({ offset: 25 }).setText(r.name))
          .addTo(map);
        bounds.extend([lon, lat]);
      });
      if (!bounds.isEmpty() && results.length > 1) {
        map.fitBounds(bounds, { padding: 30 });
      }
      if (tile) { (tileMaps[tile.id] = tileMaps[tile.id] || []).push(map); }
    }, 100);
  });

  await listen("map-route", (event) => {
    const { session_id, from, to, distance_km, duration_min, geometry, steps } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;

    const mapId = "map-" + Date.now();
    const distText = distance_km < 1 ? `${Math.round(distance_km * 1000)}m` : `${distance_km.toFixed(1)}km`;
    const durText = duration_min < 60 ? `${Math.round(duration_min)} min` : `${Math.floor(duration_min / 60)}h ${Math.round(duration_min % 60)}m`;

    const el = document.createElement("div");
    el.className = "turn";
    el.innerHTML = `
      <div class="turn-body">
        <div class="map-title">${escapeHtml(from)} \u2192 ${escapeHtml(to)}</div>
        <div class="map-summary">${distText} \u00b7 ${durText}</div>
        <div id="${mapId}" class="map-container"></div>
        ${steps && steps.length > 0 ? `<details class="map-steps"><summary>Directions (${steps.length} steps)</summary>${steps.map(s => `<div class="map-step">${escapeHtml(s.instruction)} (${s.distance}m)</div>`).join("")}</details>` : ""}
      </div>
    `;
    chatLog.appendChild(el);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;

    if (!ui.mapboxToken || !geometry || geometry.length === 0) return;
    setTimeout(() => {
      const isDark = isDarkTheme();
      const style = isDark ? "mapbox://styles/mapbox/dark-v11" : "mapbox://styles/mapbox/streets-v12";
      // geometry is already [lon, lat] from Mapbox Directions API
      const map = new mapboxgl.Map({
        container: mapId,
        style,
        center: geometry[0],
        zoom: 13,
        accessToken: ui.mapboxToken,
        attributionControl: false,
      });
      map.on("load", () => {
        map.addSource("route", {
          type: "geojson",
          data: { type: "Feature", geometry: { type: "LineString", coordinates: geometry } },
        });
        map.addLayer({
          id: "route",
          type: "line",
          source: "route",
          layout: { "line-join": "round", "line-cap": "round" },
          paint: { "line-color": "#4A90D9", "line-width": 4 },
        });
        // Fit bounds to route
        const bounds = new mapboxgl.LngLatBounds();
        geometry.forEach(c => bounds.extend(c));
        map.fitBounds(bounds, { padding: 30 });
      });
      // Start and end markers
      new mapboxgl.Marker({ color: "#22c55e" })
        .setLngLat(geometry[0])
        .setPopup(new mapboxgl.Popup({ offset: 25 }).setText(from))
        .addTo(map);
      new mapboxgl.Marker({ color: "#ef4444" })
        .setLngLat(geometry[geometry.length - 1])
        .setPopup(new mapboxgl.Popup({ offset: 25 }).setText(to))
        .addTo(map);
      if (tile) { (tileMaps[tile.id] = tileMaps[tile.id] || []).push(map); }
    }, 100);
  });

  await listen("memory-updated", () => {
    // Refresh any open memory tiles
    const memTiles = getTilesByType("memory");
    for (const t of memTiles) {
      const body = t.el.querySelector(".tile-body");
      if (body) renderMemoryTile(t, body);
    }
  });

  await listen("agents-changed", async () => {
    // The agent created/edited/deleted a custom agent via tools — refresh the cache
    // (slash commands, task agent pickers) and any open Agents tiles.
    try { ui.cachedAgents = await invoke("get_agents"); } catch { /* keep old cache */ }
    const agentTiles = getTilesByType("agents");
    for (const t of agentTiles) {
      const body = t.el.querySelector(".tile-body");
      if (body) renderAgentsTile(t, body);
    }
  });

  await listen("settings-updated", async () => {
    // The agent changed a setting via set_setting — resync cached settings and any open
    // Settings tiles so the UI doesn't show stale values.
    try { ui.loadedSettings = await invoke("get_app_settings"); } catch { /* keep old cache */ }
    updateModelChips();
    refreshSettingsModalIfOpen();
    const settingsTiles = getTilesByType("settings");
    for (const t of settingsTiles) {
      const body = t.el.querySelector(".tile-body");
      if (body) renderSettingsTile(t, body);
    }
  });

  await listen("theme-changed", (event) => {
    // The agent's set_theme tool — same effect as picking the theme in Settings.
    const theme = event.payload?.theme;
    if (!theme) return;
    localStorage.setItem("muninn-theme", theme);
    applyTheme(theme);
  });

  await listen("tasks-changed", () => {
    const taskTiles = getTilesByType("tasks");
    for (const t of taskTiles) {
      const body = t.el.querySelector(".tile-body");
      if (body) renderTasksTile(t, body);
    }
  });

  await listen("session-titled", async () => {
    // A background LLM call named a session — refresh the sidebar's chat list.
    try {
      const { refreshSessionList } = await import("./sidebar.js");
      refreshSessionList();
    } catch { /* sidebar not present (pop-out window) */ }
  });

  await listen("todos-changed", (event) => {
    const payload = event.payload || {};
    const sid = payload.session_id;
    const tile = getChatTileBySessionId(sid);
    if (!tile) return;
    renderTodoPanel(tile.id, payload.items || []);
  });

  await listen("user-question", (event) => {
    const payload = event.payload || {};
    const sid = payload.session_id;
    const tile = getChatTileBySessionId(sid);
    if (!tile) return;
    showUserQuestion(tile.id, sid, payload.question || "", payload.choices || []);
  });

  await listen("user-question-timeout", (event) => {
    const sid = event.payload?.session_id;
    if (!sid) return;
    const tile = getChatTileBySessionId(sid);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    const existing = chatLog?.querySelector(".user-question");
    if (existing) existing.remove();
  });

  await listen("command-deferred", (event) => {
    const payload = event.payload || {};
    const sid = payload.session_id;
    const tile = getChatTileBySessionId(sid);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;
    renderDeferredCard(chatLog, {
      id: payload.id,
      command: payload.command || "",
      verifier_note: payload.verifier_note || "",
    });
  });

  await listen("content-changed", () => {
    const contentTiles = getTilesByType("content");
    for (const t of contentTiles) {
      const body = t.el.querySelector(".tile-body");
      if (body) renderContentTile(t, body);
    }
  });

  // Drag-and-drop file import (Tauri native drag-drop provides file paths).
  // Tauri intercepts OS file drops at the window level, so the webview's HTML5 drop
  // event never receives them — we resolve the drop position to the element under the
  // cursor here instead.
  const IMG_EXT = /\.(png|jpe?g|gif|webp)$/i;

  // Map a Tauri physical drop position to the chat tile under the cursor, if any.
  function chatTileAt(position) {
    if (!position) return null;
    const dpr = window.devicePixelRatio || 1;
    const el = document.elementFromPoint(position.x / dpr, position.y / dpr);
    const tileEl = el && el.closest(".tile");
    const tileId = tileEl && tileEl.dataset.tileId;
    if (!tileId) return null;
    return tiles.find(t => String(t.id) === String(tileId) && t.type === "chat") || null;
  }

  // Highlight the chat tile a dragged file is hovering over.
  let dragHoverTile = null;
  function setDragHover(tile) {
    if (dragHoverTile === tile) return;
    if (dragHoverTile && dragHoverTile.el) dragHoverTile.el.classList.remove("drop-target");
    if (tile && tile.el) tile.el.classList.add("drop-target");
    dragHoverTile = tile;
  }
  await listen("tauri://drag-over", (event) => setDragHover(chatTileAt(event.payload?.position)));
  await listen("tauri://drag-leave", () => setDragHover(null));

  await listen("tauri://drag-drop", async (event) => {
    setDragHover(null);
    const paths = event.payload?.paths;
    if (!paths || !paths.length) return;

    // An image dropped on a chat tile becomes an attachment for that chat, not a content import.
    const imagePaths = paths.filter(p => IMG_EXT.test(p));
    const tile = imagePaths.length ? chatTileAt(event.payload?.position) : null;
    if (tile && typeof tile.addDroppedImage === "function") {
      for (const path of imagePaths) {
        try {
          const dataUrl = await invoke("read_image_base64", { path });
          tile.addDroppedImage(dataUrl);
        } catch {}
      }
      return;
    }

    // Otherwise fall back to importing the dropped files into the content library.
    try {
      await invoke("import_content_files", { req: { paths } });
    } catch {}
  });

  await listen("image-generated", async (event) => {
    const { session_id, path, prompt } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;
    const stream = chatStreams[tile.id];

    // Finalize any open text bubble before inserting the image
    if (stream && stream.bubble) {
      stream.bubble.innerHTML = renderMarkdown(stream.text);
      stream.bubble = null;
      stream.text = "";
    }

    // Load image as base64 data URL (reliable across all Tauri configs)
    let src;
    try {
      src = await invoke("read_image_base64", { path });
    } catch {
      src = convertFileSrc(path); // fallback
    }

    const el = document.createElement("div");
    el.className = "turn image-turn";
    el.innerHTML = `
      <div class="turn-header">
        <span class="turn-label muninn">Muninn</span>
        <span class="turn-time">${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</span>
      </div>
      <div class="turn-body">
        <img src="${src}" alt="${prompt.replace(/"/g, '&quot;')}" style="max-width:100%; border-radius:8px; margin:8px 0;" />
        <p class="image-caption">${escapeHtml(prompt)}</p>
      </div>
    `;
    chatLog.appendChild(el);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
  });

  await listen("screen-captured", (event) => {
    const { session_id, media_type, data, note } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;
    const stream = chatStreams[tile.id];

    // Finalize any open text bubble before inserting the screenshot
    if (stream && stream.bubble) {
      stream.bubble.innerHTML = renderMarkdown(stream.text);
      stream.bubble = null;
      stream.text = "";
    }

    const mt = escapeHtml(media_type || "image/png");
    const src = `data:${mt};base64,${data || ""}`;
    const caption = note ? escapeHtml(note) : "screenshot";
    const el = document.createElement("div");
    el.className = "turn image-turn";
    el.innerHTML = `
      <div class="turn-header">
        <span class="turn-label muninn">Muninn</span>
        <span class="turn-time">${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</span>
      </div>
      <div class="turn-body">
        <img src="${src}" alt="screenshot" style="max-width:100%; border-radius:8px; margin:8px 0;" />
        <p class="image-caption">📸 ${caption}</p>
      </div>
    `;
    chatLog.appendChild(el);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
  });
}
