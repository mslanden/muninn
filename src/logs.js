import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { escapeHtml, field, feedTimeAgo } from "./utils.js";
import { logError, reconcileKeyedList, emptyState } from "./uikit.js";

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
  try {
    items = await invoke("get_feed_items", { req: { limit: 200, unreadOnly: false } });
  } catch (err) {
    logError("logs/get_feed_items", err);
    items = [];
  }
  const unread = items.filter(item => !field(item, "isRead", "is_read")).length;

  if (badge) {
    badge.textContent = `${unread} unread`;
    badge.classList.toggle("has-unread", unread > 0);
  }

  if (!items.length) {
    list.replaceChildren(emptyState({ icon: "✓", text: "No activity yet" }));
    return;
  }

  const dotClass = (item) => {
    const kind = field(item, "kind", "kind") || "response";
    return kind === "tool" ? "blue" : kind === "message" ? "green" : "amber";
  };

  // Incremental keyed render: existing rows are kept (preserving scroll position),
  // only changed read-state / time is patched, and removed items are dropped.
  reconcileKeyedList(
    list,
    items,
    (item) => field(item, "id", "id"),
    (item) => {
      const id = field(item, "id", "id");
      const el = document.createElement("div");
      el.className = "logs-item";
      el.innerHTML = `
        <span class="logs-dot"></span>
        <div class="logs-info">
          <strong>${escapeHtml(field(item, "title", "title"))}</strong>
          <span>${escapeHtml(field(item, "message", "message"))}</span>
          <span class="logs-time"></span>
        </div>
        <button type="button" class="logs-item-delete" title="Remove" aria-label="Remove activity item">&times;</button>
      `;
      el.addEventListener("click", async (e) => {
        if (e.target.closest(".logs-item-delete")) return;
        if (el.dataset.read === "true") return;
        await invoke("mark_feed_item_read", { req: { id } });
        await refreshLogsTile(tileId);
      });
      el.querySelector(".logs-item-delete").addEventListener("click", async (e) => {
        e.stopPropagation();
        try {
          await invoke("remove_feed_item", { req: { id } });
        } catch (err) {
          logError("logs/remove_feed_item", err);
        }
        await refreshLogsTile(tileId);
      });
      updateRow(el, item);
      return el;
    },
    updateRow,
  );

  function updateRow(el, item) {
    const isRead = !!field(item, "isRead", "is_read");
    el.dataset.read = String(isRead);
    el.querySelector(".logs-dot").className = `logs-dot ${isRead ? "gray" : dotClass(item)}`;
    el.querySelector(".logs-time").textContent = feedTimeAgo(field(item, "createdAt", "created_at"));
  }
}


registerTileRenderer("logs", renderLogsTile);
