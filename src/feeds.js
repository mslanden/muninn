import { registerTileRenderer } from "./registry.js";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { escapeHtml, feedTimeAgo } from "./utils.js";
import { openReaderWindow, openArticleReader } from "./reader.js";
import { openFeedItemInChat, playTts } from "./chat.js";
import { renderScheduleWidget } from "./schedule.js";

// ========== FEEDS TILE ==========
export function renderFeedsTile(tileObj, body) {
  body.innerHTML = `
    <div class="feeds-layout">
      <div class="feeds-sidebar">
        <div class="feeds-sidebar-header">
          <span class="feeds-sidebar-title">Feeds</span>
          <button class="feeds-add-btn tile-btn" data-feeds-new="${tileObj.id}" title="New feed">+</button>
        </div>
        <div class="feeds-feed-list" data-feeds-list="${tileObj.id}"></div>
      </div>
      <div class="feeds-main">
        <div class="feeds-form" data-feeds-form="${tileObj.id}">
          <div class="feeds-form-title" data-feeds-form-title="${tileObj.id}">New Feed</div>
          <input type="text" class="feeds-input" placeholder="Feed name (e.g. Tech News)" data-feeds-name="${tileObj.id}" />
          <input type="text" class="feeds-input" placeholder="Topics (comma-separated, e.g. AI, Rust, startup)" data-feeds-topics="${tileObj.id}" />
          <div class="feeds-schedule-container" data-feeds-schedule="${tileObj.id}"></div>
          <div class="feeds-form-actions">
            <button class="feeds-btn primary" data-feeds-save="${tileObj.id}">Create Feed</button>
            <button class="feeds-btn" data-feeds-cancel="${tileObj.id}">Cancel</button>
          </div>
          <div class="feeds-form-error" data-feeds-error="${tileObj.id}"></div>
        </div>
        <div class="feeds-items-panel" data-feeds-items="${tileObj.id}">
          <div class="feeds-empty-state">
            <div class="feeds-empty-icon">◈</div>
            <div class="feeds-empty-text">Select a feed or create one with +</div>
          </div>
        </div>
      </div>
    </div>
  `;

  let selectedFeedId = null;
  let editingFeedId = null;
  let allFeeds = [];
  const form = body.querySelector(`[data-feeds-form="${tileObj.id}"]`);
  const formTitle = body.querySelector(`[data-feeds-form-title="${tileObj.id}"]`);
  const feedList = body.querySelector(`[data-feeds-list="${tileObj.id}"]`);
  const itemsPanel = body.querySelector(`[data-feeds-items="${tileObj.id}"]`);
  const nameInput = body.querySelector(`[data-feeds-name="${tileObj.id}"]`);
  const topicsInput = body.querySelector(`[data-feeds-topics="${tileObj.id}"]`);
  const saveBtn = body.querySelector(`[data-feeds-save="${tileObj.id}"]`);
  const errorEl = body.querySelector(`[data-feeds-error="${tileObj.id}"]`);
  const scheduleContainer = body.querySelector(`[data-feeds-schedule="${tileObj.id}"]`);
  const scheduleWidget = renderScheduleWidget(scheduleContainer, null);

  function showError(msg) {
    errorEl.textContent = msg;
    errorEl.style.display = msg ? "block" : "none";
  }

  function resetForm() {
    editingFeedId = null;
    nameInput.value = "";
    topicsInput.value = "";
    scheduleWidget.clear();
    formTitle.textContent = "New Feed";
    saveBtn.textContent = "Create Feed";
    showError("");
  }

  body.querySelector(`[data-feeds-new="${tileObj.id}"]`).addEventListener("click", () => {
    resetForm();
    form.classList.add("visible");
    nameInput.focus();
  });

  body.querySelector(`[data-feeds-cancel="${tileObj.id}"]`).addEventListener("click", () => {
    form.classList.remove("visible");
    resetForm();
  });

  body.querySelector(`[data-feeds-save="${tileObj.id}"]`).addEventListener("click", async () => {
    const name = nameInput.value.trim();
    const topicsRaw = topicsInput.value.trim();
    if (!name) { showError("Feed name is required."); return; }
    if (!topicsRaw) { showError("Enter at least one topic."); return; }
    const topics = topicsRaw.split(",").map(t => t.trim()).filter(t => t.length > 0);
    const schedData = scheduleWidget.getData();
    const scheduleConfig = schedData ? JSON.stringify(schedData) : "";
    showError("");
    try {
      if (editingFeedId) {
        await invoke("update_ai_feed", { req: { id: editingFeedId, name, topics, scheduleConfig } });
      } else {
        await invoke("create_ai_feed", { req: { name, topics, scheduleConfig } });
      }
      form.classList.remove("visible");
      resetForm();
      await loadFeedList();
    } catch (err) {
      showError(String(err));
    }
  });

  async function loadFeedList() {
    try { allFeeds = await invoke("get_ai_feeds"); } catch { allFeeds = []; }
    feedList.innerHTML = "";
    if (!allFeeds.length) {
      feedList.innerHTML = `<div class="feeds-no-feeds">No feeds yet</div>`;
      return;
    }
    for (const feed of allFeeds) {
      const id = feed.id;
      const name = feed.name;
      const topics = (feed.topics || []).join(", ");
      const el = document.createElement("div");
      el.className = "feeds-feed-item" + (id === selectedFeedId ? " selected" : "");
      el.dataset.feedId = id;
      el.innerHTML = `
        <div class="feeds-feed-item-top">
          <span class="feeds-feed-name">${escapeHtml(name)}</span>
          <button class="feeds-feed-edit tile-btn" data-edit-id="${id}" title="Edit feed">&#9998;</button>
          <button class="feeds-feed-del tile-btn" data-del-id="${id}" title="Delete feed">&times;</button>
        </div>
        <div class="feeds-feed-topics">${escapeHtml(topics)}</div>
      `;
      el.querySelector(`[data-edit-id="${id}"]`).addEventListener("click", (e) => {
        e.stopPropagation();
        editingFeedId = id;
        nameInput.value = feed.name;
        topicsInput.value = (feed.topics || []).join(", ");
        formTitle.textContent = "Edit Feed";
        saveBtn.textContent = "Update Feed";
        if (feed.scheduleConfig) {
          try { scheduleWidget.setData(JSON.parse(feed.scheduleConfig)); } catch { scheduleWidget.clear(); }
        } else {
          scheduleWidget.clear();
        }
        showError("");
        form.classList.add("visible");
        nameInput.focus();
      });
      el.querySelector(`[data-del-id="${id}"]`).addEventListener("click", async (e) => {
        e.stopPropagation();
        await invoke("delete_ai_feed", { req: { id } });
        if (selectedFeedId === id) {
          selectedFeedId = null;
          showItemsEmpty();
        }
        await loadFeedList();
      });
      el.addEventListener("click", () => selectFeed(id, name));
      feedList.appendChild(el);
    }
  }

  function showItemsEmpty() {
    itemsPanel.innerHTML = `
      <div class="feeds-empty-state">
        <div class="feeds-empty-icon">◈</div>
        <div class="feeds-empty-text">Select a feed or create one with +</div>
      </div>
    `;
  }

  async function selectFeed(id, name) {
    selectedFeedId = id;
    // Highlight selected
    feedList.querySelectorAll(".feeds-feed-item").forEach(el => {
      el.classList.toggle("selected", Number(el.dataset.feedId) === id);
    });
    await loadFeedItems(id, name);
  }

  async function loadFeedItems(feedId, feedName) {
    itemsPanel.innerHTML = `<div class="feeds-loading">Loading…</div>`;
    try {
      const items = await invoke("get_ai_feed_items", { req: { id: feedId } });
      renderFeedItems(feedId, feedName, items);
    } catch (err) {
      itemsPanel.innerHTML = `<div class="feeds-error-msg">${escapeHtml(err)}</div>`;
    }
  }

  function renderFeedItems(feedId, feedName, items) {
    itemsPanel.innerHTML = "";

    const toolbar = document.createElement("div");
    toolbar.className = "feeds-items-toolbar";
    toolbar.innerHTML = `
      <span class="feeds-items-title">${escapeHtml(feedName)}</span>
      <span class="feeds-spacer"></span>
      <button class="feeds-btn feeds-refresh-btn" data-refresh-id="${feedId}">&#8635; Refresh</button>
    `;
    toolbar.querySelector(`[data-refresh-id="${feedId}"]`).addEventListener("click", async (e) => {
      const btn = e.currentTarget;
      btn.textContent = "Refreshing…";
      btn.disabled = true;
      try {
        const result = await invoke("refresh_ai_feed", { req: { id: feedId } });
        const added = result.itemsAdded ?? result.items_added ?? 0;
        await loadFeedItems(feedId, feedName);
        // Re-get the toolbar button after re-render and show count briefly
        const newBtn = itemsPanel.querySelector(`[data-refresh-id="${feedId}"]`);
        if (newBtn) {
          newBtn.textContent = `+ ${added} added`;
          setTimeout(() => { if (newBtn) newBtn.textContent = "↻ Refresh"; }, 2500);
        }
      } catch (err) {
        btn.textContent = "↻ Refresh";
        btn.disabled = false;
        const errEl = document.createElement("div");
        errEl.className = "feeds-error-msg";
        errEl.textContent = String(err);
        itemsPanel.insertBefore(errEl, itemsPanel.firstChild);
        setTimeout(() => errEl.remove(), 6000);
      }
    });
    itemsPanel.appendChild(toolbar);

    if (!items.length) {
      const empty = document.createElement("div");
      empty.className = "feeds-empty-state";
      empty.innerHTML = `
        <div class="feeds-empty-icon">◈</div>
        <div class="feeds-empty-text">No items yet — click Refresh to fetch content</div>
      `;
      itemsPanel.appendChild(empty);
      return;
    }

    const list = document.createElement("div");
    list.className = "feeds-cards";
    for (const item of items) {
      const id = item.id;
      const isRead = item.isRead ?? item.is_read ?? false;
      const title = item.title || "";
      const summary = item.summary || "";
      const url = item.url || "";
      const sourceName = item.sourceName ?? item.source_name ?? "";
      const createdAt = item.createdAt ?? item.created_at ?? "";

      const card = document.createElement("div");
      const rawImageUrl = item.imageUrl ?? item.image_url ?? "";
      // Local paths use asset: protocol, remote URLs pass through
      const imageUrl = rawImageUrl.startsWith("/") ? convertFileSrc(rawImageUrl) : rawImageUrl;
      card.className = "feeds-card" + (isRead ? " read" : "") + (imageUrl ? " has-image" : "");
      const ttsText = `${title}. ${summary}`;
      card.innerHTML = `
        ${imageUrl ? `<div class="feeds-card-img-wrap"><img class="feeds-card-img" src="${escapeHtml(imageUrl)}" alt="" loading="lazy" /></div>` : `<div class="feeds-card-img-wrap feeds-card-img-pending" data-item-id="${id}"></div>`}
        <div class="feeds-card-body">
          <div class="feeds-card-header">
            <span class="feeds-card-source">${escapeHtml(sourceName)}</span>
            <span class="feeds-card-time">${feedTimeAgo(createdAt)}</span>
          </div>
          <div class="feeds-card-title">${escapeHtml(title)}</div>
          ${summary ? `<div class="feeds-card-summary">${escapeHtml(summary)}</div>` : ""}
          <div class="feeds-card-actions">
            <button class="feeds-card-action feeds-overview-btn">✦ Overview</button>
            <button class="feeds-card-action feeds-chat-btn">💬 Open in chat</button>
            <button class="feeds-card-action feeds-card-tts" title="Read aloud">&#9654; Listen</button>
            ${url ? `<a class="feeds-card-link" href="${escapeHtml(url)}" data-reader-url="${escapeHtml(url)}">Read original ↗</a>` : ""}
          </div>
        </div>
      `;
      const ttsBtn = card.querySelector(".feeds-card-tts");
      ttsBtn.addEventListener("click", (e) => { e.stopPropagation(); playTts(ttsBtn, ttsText); });
      const overviewBtn = card.querySelector(".feeds-overview-btn");
      overviewBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        openArticleReader(item);
        if (!isRead) {
          invoke("mark_ai_feed_item_read", { req: { id } }).catch(() => {});
          card.classList.add("read");
        }
      });
      const chatBtn = card.querySelector(".feeds-chat-btn");
      chatBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        openFeedItemInChat(item);
        if (!isRead) {
          invoke("mark_ai_feed_item_read", { req: { id } }).catch(() => {});
          card.classList.add("read");
        }
      });
      const readerLink = card.querySelector("[data-reader-url]");
      if (readerLink) {
        readerLink.addEventListener("click", (e) => {
          e.preventDefault();
          e.stopPropagation();
          openReaderWindow(readerLink.dataset.readerUrl, title);
          if (!isRead) {
            invoke("mark_ai_feed_item_read", { req: { id } }).catch(() => {});
            card.classList.add("read");
          }
        });
      }
      // Handle broken OG images: queue for AI generation
      const imgEl = card.querySelector(".feeds-card-img");
      if (imgEl) {
        imgEl.addEventListener("error", () => {
          const wrap = imgEl.parentElement;
          imgEl.remove();
          wrap.classList.add("feeds-card-img-pending");
          wrap.dataset.itemId = id;
        });
      }
      if (!isRead) {
        card.addEventListener("click", async () => {
          await invoke("mark_ai_feed_item_read", { req: { id } });
          card.classList.add("read");
        });
      }
      list.appendChild(card);
    }
    itemsPanel.appendChild(list);

    // Collect item IDs that need images, then generate in the background.
    // Generation continues even if user navigates away — images are saved to DB
    // and will show up next time the feed is opened.
    const pendingIds = [];
    for (const wrap of list.querySelectorAll(".feeds-card-img-pending")) {
      pendingIds.push(parseInt(wrap.dataset.itemId));
    }
    if (pendingIds.length > 0) {
      (async () => {
        for (const itemId of pendingIds) {
          try {
            const localPath = await invoke("generate_feed_item_image", { req: { id: itemId } });
            // If the DOM element is still there, update it live
            const wrap = list.querySelector(`.feeds-card-img-pending[data-item-id="${itemId}"]`);
            if (localPath && wrap && wrap.isConnected) {
              wrap.classList.remove("feeds-card-img-pending");
              const img = document.createElement("img");
              img.className = "feeds-card-img";
              img.loading = "lazy";
              img.alt = "";
              img.onerror = () => { wrap.style.display = "none"; };
              img.src = localPath.startsWith("/") ? convertFileSrc(localPath) : localPath;
              wrap.innerHTML = "";
              wrap.appendChild(img);
            }
          } catch (err) {
            console.error("Feed image gen failed for item", itemId, err);
          }
        }
      })();
    }
  }

  loadFeedList();
}


registerTileRenderer("feeds", renderFeedsTile);
