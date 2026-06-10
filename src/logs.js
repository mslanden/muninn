import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { escapeHtml, field, feedTimeAgo } from "./utils.js";

// ========== LOGS TILE ==========
export function renderLogsTile(tileObj, body) {
  body.innerHTML = `
    <div class="logs-toolbar">
      <span class="logs-badge" data-logs-badge="${tileObj.id}">0 unread</span>
      <div class="logs-actions">
        <button class="tile-btn" data-logs-refresh="${tileObj.id}" title="Refresh">&#8635;</button>
        <button class="tile-btn" data-logs-read="${tileObj.id}" title="Mark all read">&#10003;</button>
        <button class="tile-btn" data-logs-clear="${tileObj.id}" title="Clear">&#8856;</button>
      </div>
    </div>
    <div class="logs-list" data-logs-list="${tileObj.id}"></div>
  `;

  body.querySelector(`[data-logs-refresh="${tileObj.id}"]`).addEventListener("click", () => refreshLogsTile(tileObj.id));
  body.querySelector(`[data-logs-read="${tileObj.id}"]`).addEventListener("click", async () => {
    await invoke("mark_all_feed_read");
    await refreshLogsTile(tileObj.id);
  });
  body.querySelector(`[data-logs-clear="${tileObj.id}"]`).addEventListener("click", async () => {
    await invoke("clear_feed_items");
    await refreshLogsTile(tileObj.id);
  });

  refreshLogsTile(tileObj.id);
}

export async function refreshLogsTile(tileId) {
  const list = document.querySelector(`[data-logs-list="${tileId}"]`);
  const badge = document.querySelector(`[data-logs-badge="${tileId}"]`);
  if (!list) return;

  let items;
  try { items = await invoke("get_feed_items", { req: { limit: 200, unreadOnly: false } }); } catch { items = []; }
  const unread = items.filter(item => !field(item, "isRead", "is_read")).length;

  if (badge) {
    badge.textContent = `${unread} unread`;
    badge.classList.toggle("has-unread", unread > 0);
  }

  list.innerHTML = "";
  if (!items.length) {
    list.innerHTML = `<div class="feed-empty">No activity yet</div>`;
    return;
  }

  for (const item of items) {
    const id = field(item, "id", "id");
    const isRead = !!field(item, "isRead", "is_read");
    const kind = field(item, "kind", "kind") || "response";
    const title = field(item, "title", "title");
    const message = field(item, "message", "message");
    const createdAt = field(item, "createdAt", "created_at");

    const dotColor = kind === "tool" ? "blue" : kind === "message" ? "green" : "amber";
    const el = document.createElement("div");
    el.className = "logs-item";
    el.innerHTML = `
      <span class="logs-dot ${isRead ? "gray" : dotColor}"></span>
      <div class="logs-info">
        <strong>${escapeHtml(title)}</strong>
        <span>${escapeHtml(message)}</span>
        <span class="logs-time">${feedTimeAgo(createdAt)}</span>
      </div>
      <span class="logs-item-delete" title="Remove">&times;</span>
    `;

    if (!isRead) {
      el.addEventListener("click", async (e) => {
        if (e.target.classList.contains("logs-item-delete")) return;
        await invoke("mark_feed_item_read", { req: { id } });
        await refreshLogsTile(tileId);
      });
    }

    el.querySelector(".logs-item-delete").addEventListener("click", async (e) => {
      e.stopPropagation();
      try { await invoke("remove_feed_item", { req: { id } }); } catch {}
      await refreshLogsTile(tileId);
    });

    list.appendChild(el);
  }
}


registerTileRenderer("logs", renderLogsTile);
