import { invoke } from "@tauri-apps/api/core";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { escapeHtml, renderMarkdown } from "./utils.js";
import { openFeedItemInChat, playTts } from "./chat.js";
import { icon } from "./icons.js";

export async function openReaderWindow(url, title) {
  if (!url) return;
  let hash = 0;
  for (let i = 0; i < url.length; i++) hash = ((hash << 5) - hash + url.charCodeAt(i)) | 0;
  const label = "muninn-reader-" + Math.abs(hash).toString(36);
  try {
    const existing = await WebviewWindow.getByLabel(label);
    if (existing) {
      try { await existing.setFocus(); } catch (_) {}
      return;
    }
  } catch (_) {}
  const w = new WebviewWindow(label, {
    url,
    width: 1100,
    height: 800,
    resizable: true,
    title: title ? `${title} — Muninn` : "Muninn Reader",
  });
  w.once("tauri://error", (e) => console.error("reader window error", e));
}

// Reader-style overlay that shows a generated AI overview/article for a feed item.
// Generates the article on first open (lazy) and caches it on the item object + DB.
export function openArticleReader(item) {
  const id = item.id;
  const title = item.title || "";
  const url = item.url || "";
  const sourceName = item.sourceName ?? item.source_name ?? "";
  const cached = item.article ?? "";

  // Remove any existing reader overlay first.
  document.querySelector(".article-reader-overlay")?.remove();

  const overlay = document.createElement("div");
  overlay.className = "article-reader-overlay";
  overlay.innerHTML = `
    <div class="article-reader-panel">
      <div class="article-reader-header">
        <div class="article-reader-meta">
          <div class="article-reader-source">${escapeHtml(sourceName)}</div>
          <div class="article-reader-title">${escapeHtml(title)}</div>
        </div>
        <div class="article-reader-tools">
          <button class="article-reader-btn article-reader-tts" data-action="tts" title="Read aloud" aria-label="Read aloud" disabled>${icon("play", 12)}</button>
          <button class="article-reader-btn" data-action="chat" aria-label="Open in chat">${icon("message-square", 12)} Open in chat</button>
          ${url ? `<button class="article-reader-btn" data-action="original" aria-label="Open original">${icon("arrow-up-right", 12)} Original</button>` : ""}
          <button class="article-reader-btn article-reader-close" data-action="close" title="Close" aria-label="Close">${icon("x", 12)}</button>
        </div>
      </div>
      <div class="article-reader-body" data-article-body></div>
    </div>
  `;
  document.body.appendChild(overlay);

  const bodyEl = overlay.querySelector("[data-article-body]");
  const ttsBtn = overlay.querySelector('[data-action="tts"]');
  const close = () => overlay.remove();

  overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });
  overlay.querySelector('[data-action="close"]').addEventListener("click", close);
  overlay.querySelector('[data-action="original"]')?.addEventListener("click", () => openReaderWindow(url, title));
  const escHandler = (e) => { if (e.key === "Escape") { close(); document.removeEventListener("keydown", escHandler); } };
  document.addEventListener("keydown", escHandler);

  const enableTts = (text) => {
    if (!ttsBtn || !text || !text.trim()) return;
    ttsBtn.disabled = false;
    ttsBtn.onclick = () => playTts(ttsBtn, text);
  };

  const renderArticle = (md) => {
    bodyEl.classList.remove("article-reader-loading");
    bodyEl.innerHTML = `<div class="article-reader-content">${renderMarkdown(md)}</div>`;
    enableTts(md);
  };

  overlay.querySelector('[data-action="chat"]').addEventListener("click", () => {
    openFeedItemInChat(item);
    close();
  });

  if (cached && cached.trim()) {
    renderArticle(cached);
    return;
  }

  bodyEl.classList.add("article-reader-loading");
  bodyEl.innerHTML = `<div class="article-reader-spinner"></div><div class="article-reader-loading-text">Generating overview…</div>`;

  invoke("generate_feed_article", { req: { id } })
    .then((article) => {
      item.article = article; // cache on the in-memory item for instant re-open
      if (overlay.isConnected) renderArticle(article);
    })
    .catch((err) => {
      if (!overlay.isConnected) return;
      bodyEl.classList.remove("article-reader-loading");
      bodyEl.innerHTML = `<div class="article-reader-error">Couldn't generate an overview.<br><span>${escapeHtml(String(err))}</span></div>`;
    });
}

// Open a fresh chat tile seeded with the feed item's context and auto-send a prompt
// asking the assistant to dive deeper into it.
