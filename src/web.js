import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { escapeHtml, normalizeWebUrl, renderMarkdown, looksLikeUrl } from "./utils.js";
import { popOutTile, saveLayout } from "./tilesys.js";
import { openReaderWindow } from "./reader.js";
import { openQueryInChat } from "./chat.js";

export function renderWebTile(tileObj, body) {
  body.classList.add("web-tile-body");
  body.innerHTML = `
    <div class="web-tile" data-web-root="${tileObj.id}">
      <div class="web-toolbar">
        <input type="text" class="web-url-input" data-web-q="${tileObj.id}" placeholder="Search the web or enter a URL…" />
        <button class="form-btn" data-web-go="${tileObj.id}">Search</button>
        <button class="form-btn" data-web-window="${tileObj.id}" title="Open current page in a separate window">Open in Window</button>
      </div>
      <div class="web-status" data-web-status="${tileObj.id}">
        Ask anything, or type a URL to browse.
      </div>

      <div class="web-results" data-web-results="${tileObj.id}">
        <div class="web-results-actions hidden" data-web-results-actions="${tileObj.id}">
          <button class="form-btn" data-web-chat="${tileObj.id}">💬 Open in chat</button>
        </div>
        <div class="web-answer" data-web-answer="${tileObj.id}"></div>
        <div class="web-sources" data-web-sources="${tileObj.id}"></div>
      </div>

      <div class="web-loading hidden" data-web-loading="${tileObj.id}">
        <div class="web-spinner"></div>
        <div>Researching…</div>
      </div>

      <div class="web-browse hidden" data-web-browse="${tileObj.id}">
        <div class="web-browse-bar">
          <button class="form-btn" data-web-back="${tileObj.id}">← Back to search</button>
        </div>
        <div class="web-frame-wrap">
          <iframe class="web-frame" data-web-frame="${tileObj.id}" title="Web content" referrerpolicy="no-referrer"></iframe>
          <div class="web-frame-overlay hidden" data-web-overlay="${tileObj.id}">
            <div class="web-overlay-title">This site may block in-app embedding.</div>
            <a href="#" class="web-open-link" data-web-open-link="${tileObj.id}">Open this page in a separate window</a>
          </div>
        </div>
      </div>
    </div>
  `;

  const input = body.querySelector(`[data-web-q="${tileObj.id}"]`);
  const goBtn = body.querySelector(`[data-web-go="${tileObj.id}"]`);
  const popBtn = body.querySelector(`[data-web-window="${tileObj.id}"]`);
  const status = body.querySelector(`[data-web-status="${tileObj.id}"]`);
  const frame = body.querySelector(`[data-web-frame="${tileObj.id}"]`);
  const overlay = body.querySelector(`[data-web-overlay="${tileObj.id}"]`);
  const overlayLink = body.querySelector(`[data-web-open-link="${tileObj.id}"]`);
  const resultsEl = body.querySelector(`[data-web-results="${tileObj.id}"]`);
  const resultsActions = body.querySelector(`[data-web-results-actions="${tileObj.id}"]`);
  const answerEl = body.querySelector(`[data-web-answer="${tileObj.id}"]`);
  const sourcesEl = body.querySelector(`[data-web-sources="${tileObj.id}"]`);
  const loadingEl = body.querySelector(`[data-web-loading="${tileObj.id}"]`);
  const browseEl = body.querySelector(`[data-web-browse="${tileObj.id}"]`);
  const backBtn = body.querySelector(`[data-web-back="${tileObj.id}"]`);

  let loadingTimer = null;

  function setStatus(message, isError = false) {
    status.textContent = message;
    status.classList.toggle("error", isError);
  }

  function clearLoadingTimer() {
    if (loadingTimer) {
      clearTimeout(loadingTimer);
      loadingTimer = null;
    }
  }
  tileObj.cleanup = clearLoadingTimer;

  // mode: 'results' | 'loading' | 'browse'
  function setMode(mode) {
    tileObj.webMode = mode;
    resultsEl.classList.toggle("hidden", mode !== "results");
    loadingEl.classList.toggle("hidden", mode !== "loading");
    browseEl.classList.toggle("hidden", mode !== "browse");
  }

  function renderResults(res, query) {
    tileObj.lastQuery = query;
    tileObj.lastResults = res;
    answerEl.innerHTML = res.answer ? renderMarkdown(res.answer) : "<em>No summary returned.</em>";
    sourcesEl.innerHTML = "";
    const sources = res.sources || [];
    for (const src of sources) {
      const url = src.url || "";
      if (!url) continue;
      const title = src.title || url;
      const snippet = src.snippet || "";
      const cardEl = document.createElement("div");
      cardEl.className = "web-source-card";
      cardEl.innerHTML = `
        <div class="web-source-title">${escapeHtml(title)}</div>
        <div class="web-source-url">${escapeHtml(url)}</div>
        ${snippet ? `<div class="web-source-snippet">${escapeHtml(snippet)}</div>` : ""}
      `;
      cardEl.addEventListener("click", () => openReaderWindow(url, title));
      sourcesEl.appendChild(cardEl);
    }
    resultsActions.classList.toggle("hidden", !query);
    setMode("results");
  }

  async function runSearch(query) {
    const q = query.trim();
    if (!q) return;
    setStatus(`Searching: ${q}`);
    setMode("loading");
    try {
      const res = await invoke("ai_web_search", { req: { query: q } });
      renderResults(res, q);
      setStatus(`Results for "${q}". Click a source to open it, or refine your search.`);
    } catch (err) {
      setMode("results");
      answerEl.innerHTML = `<div class="web-error">Search failed.<br><span>${escapeHtml(String(err))}</span></div>`;
      sourcesEl.innerHTML = "";
      resultsActions.classList.add("hidden");
      setStatus("Search failed.", true);
    }
  }

  function loadInFrame(rawUrl) {
    const normalized = normalizeWebUrl(rawUrl);
    if (!normalized) {
      setStatus("Invalid URL. Try example.com or https://example.com", true);
      return;
    }

    tileObj.webUrl = normalized;
    saveLayout();
    input.value = normalized;
    overlay.classList.add("hidden");
    overlayLink.href = normalized;
    setStatus(`Loading ${normalized} ...`);
    setMode("browse");
    frame.src = normalized;
    clearLoadingTimer();
    loadingTimer = setTimeout(() => {
      setStatus("If the page stays blank, it likely blocks iframe embedding. Use Open in Window.", false);
      overlay.classList.remove("hidden");
    }, 3500);
  }

  function popOutWithCurrentUrl(rawUrl) {
    const normalized = normalizeWebUrl(rawUrl);
    if (!normalized) {
      setStatus("Invalid URL. Try example.com or https://example.com", true);
      return;
    }
    tileObj.webUrl = normalized;
    saveLayout();
    popOutTile(tileObj.id);
  }

  function submit() {
    const v = input.value.trim();
    if (!v) return;
    if (looksLikeUrl(v)) {
      loadInFrame(v);
    } else {
      runSearch(v);
    }
  }

  goBtn.addEventListener("click", submit);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") submit();
  });

  backBtn.addEventListener("click", () => {
    tileObj.webUrl = "";
    saveLayout();
    setMode("results");
    setStatus(tileObj.lastQuery ? `Results for "${tileObj.lastQuery}".` : "Ask anything, or type a URL to browse.");
  });

  popBtn.addEventListener("click", () => {
    const source = (tileObj.webMode === "browse" ? tileObj.webUrl : "") || (looksLikeUrl(input.value) ? input.value.trim() : tileObj.webUrl);
    if (!source) {
      setStatus("Open a page first (type a URL), then use Open in Window.", true);
      return;
    }
    popOutWithCurrentUrl(source);
  });

  const chatBtn = body.querySelector(`[data-web-chat="${tileObj.id}"]`);
  chatBtn.addEventListener("click", () => {
    if (tileObj.lastQuery) openQueryInChat(tileObj.lastQuery);
  });

  overlayLink.addEventListener("click", (e) => {
    e.preventDefault();
    popOutWithCurrentUrl(tileObj.webUrl);
  });

  frame.addEventListener("load", () => {
    clearLoadingTimer();
    setStatus("Page loaded. Some sites may still restrict interaction inside iframes.");
  });
  frame.addEventListener("error", () => {
    clearLoadingTimer();
    setStatus("Failed to load in-app. Use Open in Window.", true);
    overlay.classList.remove("hidden");
  });

  // Restore prior state on re-render
  if (tileObj.webUrl) {
    input.value = tileObj.webUrl;
    loadInFrame(tileObj.webUrl);
  } else if (tileObj.lastResults) {
    renderResults(tileObj.lastResults, tileObj.lastQuery || "");
    setStatus(tileObj.lastQuery ? `Results for "${tileObj.lastQuery}".` : "");
  } else {
    setMode("results");
  }
}


registerTileRenderer("web", renderWebTile);
