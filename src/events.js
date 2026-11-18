import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { tiles, chatStreams, ui } from "./state.js";
import { escapeHtml, renderMarkdown } from "./utils.js";
import { getTilesByType, getChatTileBySessionId } from "./tilesys.js";
import { playTts, maybeAutoSpeak, updateModelChips, addToolCard, renderTodoPanel, showUserQuestion, renderDeferredCard, showThinking, hideThinking, updateContextBar, onVoiceRecorded, onVoiceError, setGoalBannerStatus, hideGoalBanner, addTurn, addPdfCard } from "./chat.js";
import { renderTasksTile } from "./tasks.js";
import { renderContentTile } from "./content.js";
import { isDarkTheme, applyTheme } from "./theme.js";
import { renderSettingsTile, refreshSettingsModalIfOpen } from "./settings.js";
import { onSkillsChanged } from "./skills.js";
import { refreshConnections } from "./connections.js";

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
    const { session_id, input_tokens, output_tokens, cached_tokens } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const stream = chatStreams[tile.id];
    if (!stream) return;
    const total = input_tokens + output_tokens;
    stream.contextTokens = total;
    // Share of this request the provider served from its prompt cache (0 = none reported).
    const cachedPct = input_tokens > 0 ? Math.round(((cached_tokens || 0) / input_tokens) * 100) : 0;
    updateContextBar(tile.id, total, cachedPct);
  });

  // The older part of a long chat was summarized so the model keeps the thread without the
  // full history. Messages stay in the chat; only what is sent to the model changed.
  await listen("context-compacted", (event) => {
    const { session_id, messages, ok } = event.payload;
    if (!ok || !messages) return;
    tiles
      .filter(t => t.type === "chat" && chatStreams[t.id]?.sessionId === session_id)
      .forEach(t => {
        const chatLog = t.el.querySelector(`[data-chat-stream="${t.id}"]`);
        if (chatLog) {
          addTurn(
            chatLog,
            "muninn",
            `🗜️ Summarized ${Number(messages)} earlier messages to free up context. Nothing was deleted — the model now works from a summary of them.`
          );
        }
      });
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

  // Map results (search_places / search_nearby / get_directions): a card with an Apple
  // Maps image rendered by the muninn-maps helper, the details, and an Open in Maps button.
  function addMapCard(session_id, buildBody, snapshotReq, mapsUrl) {
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;
    const stream = chatStreams[tile.id];
    if (stream && stream.bubble) {
      stream.bubble.innerHTML = renderMarkdown(stream.text);
      stream.bubble = null;
      stream.text = "";
    }
    const el = document.createElement("div");
    el.className = "turn";
    el.innerHTML = `
      <div class="turn-body">
        ${buildBody()}
        <div class="map-image-wrap"><div class="map-image-loading">Loading map…</div></div>
        <button class="form-btn map-open-btn" data-map-open>Open in Apple Maps</button>
      </div>
    `;
    const wrap = el.querySelector(".map-image-wrap");
    if (snapshotReq) {
      invoke("map_snapshot", { req: { ...snapshotReq, dark: isDarkTheme(), width: 640, height: 320 } })
        .then((src) => {
          const img = document.createElement("img");
          img.className = "map-image";
          img.alt = "Map";
          img.src = src;
          wrap.replaceChildren(img);
        })
        .catch((e) => {
          console.warn("map_snapshot", e);
          wrap.remove();
        });
    } else {
      wrap.remove();
    }
    const openBtn = el.querySelector("[data-map-open]");
    if (mapsUrl) {
      openBtn.addEventListener("click", () => invoke("open_in_maps", { url: mapsUrl }).catch((e) => console.error("open_in_maps", e)));
    } else {
      openBtn.remove();
    }
    chatLog.appendChild(el);
    chatLog.parentElement.scrollTop = chatLog.parentElement.scrollHeight;
  }

  const fmtDist = (m) => (m < 1000 ? `${Math.round(m)}m` : `${(m / 1000).toFixed(1)}km`);

  await listen("map-places", (event) => {
    const { session_id, query, results, maps_url } = event.payload;
    const places = results || [];
    addMapCard(
      session_id,
      () => `
        <div class="map-title">${escapeHtml(query)}</div>
        <div class="map-results">
          ${places.map((r, i) => {
            const extra = [r.category, r.distance_m != null ? fmtDist(r.distance_m) : "", r.phone].filter(Boolean).join(" · ");
            return `<div class="map-result">${i + 1}. <strong>${escapeHtml(r.name)}</strong>${r.address && !String(r.name).includes(r.address) ? `<span class="muted"> — ${escapeHtml(r.address)}</span>` : ""}${extra ? `<div class="muted text-xs">${escapeHtml(extra)}</div>` : ""}</div>`;
          }).join("")}
        </div>`,
      places.length ? { markers: places.map((r, i) => ({ lat: r.lat, lon: r.lon, label: String(i + 1) })) } : null,
      maps_url,
    );
  });

  await listen("map-route", (event) => {
    const { session_id, from, to, from_point, to_point, mode, distance_km, duration_min, polyline, steps, note, maps_url } = event.payload;
    const distText = distance_km < 1 ? `${Math.round(distance_km * 1000)}m` : `${distance_km.toFixed(1)}km`;
    const durText = duration_min < 60 ? `${Math.round(duration_min)} min` : `${Math.floor(duration_min / 60)}h ${Math.round(duration_min % 60)}m`;
    const markers = [
      from_point && { ...from_point, label: "A", color: "green" },
      to_point && { ...to_point, label: "B", color: "red" },
    ].filter(Boolean);
    addMapCard(
      session_id,
      () => `
        <div class="map-title">${escapeHtml(from)} \u2192 ${escapeHtml(to)}</div>
        <div class="map-summary">${escapeHtml(mode || "driving")} \u00b7 ${distText} \u00b7 ${durText}</div>
        ${note ? `<div class="muted text-xs">${escapeHtml(note)}</div>` : ""}
        ${steps && steps.length > 0 ? `<details class="map-steps"><summary>Directions (${steps.length} steps)</summary>${steps.map(s => `<div class="map-step">${escapeHtml(s.instruction)} (${fmtDist(s.distance)})</div>`).join("")}</details>` : ""}`,
      markers.length ? { markers, route: polyline || [] } : null,
      maps_url,
    );
  });

  await listen("memory-changed", () => {
    // The model (or the Memory tile) saved/edited/deleted a note — refresh open tiles
    // in place so an in-progress edit isn't thrown away.
    for (const t of getTilesByType("memory")) {
      if (typeof t.refreshMemory === "function") t.refreshMemory();
    }
  });

  await listen("connections-changed", () => {
    // A connection or tool was switched on/off — keep the Connections window in sync.
    refreshConnections();
  });

  await listen("skills-changed", () => {
    // A skill was created/edited/deleted (Muninn's skill tool or the Skills tile) —
    // refresh the cache behind slash commands + task pickers, the sidebar, and tiles.
    onSkillsChanged();
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

  // create_pdf finished: show the document inline in the chat that asked for it.
  await listen("pdf-created", (event) => {
    const { session_id, path, pages, bytes } = event.payload;
    const tile = getChatTileBySessionId(session_id);
    if (!tile) return;
    const chatLog = tile.el.querySelector(`[data-chat-stream="${tile.id}"]`);
    if (!chatLog) return;
    const stream = chatStreams[tile.id];
    // Finalize any open text bubble so later tokens start below the PDF.
    if (stream && stream.bubble) {
      stream.bubble.innerHTML = renderMarkdown(stream.text);
      stream.bubble = null;
      stream.text = "";
    }
    addPdfCard(chatLog, { path, pages, kb: Math.round((bytes || 0) / 1024) });
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
