import { registerTileRenderer } from "./registry.js";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { escapeHtml, field, formatFileSize, formatModifiedAt } from "./utils.js";
import { icon } from "./icons.js";

// ========== CONTENT TILE ==========
// SVG markup for each kind. Built once at module load so render stays cheap.
export const KIND_ICONS = {
  image: icon("image", 28),
  audio: icon("audio", 28),
  video: icon("video", 28),
  pdf:   icon("file-text", 28),
  text:  icon("file-text", 28),
  file:  icon("file", 28),
};

export function renderContentTile(tileObj, body) {
  body.innerHTML = `
    <div class="content-toolbar">
      <select data-content-filter="${tileObj.id}">
        <option value="all">All</option>
        <option value="image">Images</option>
        <option value="audio">Audio</option>
        <option value="video">Video</option>
        <option value="pdf">PDF</option>
        <option value="text">Text</option>
      </select>
      <span class="tasks-spacer"></span>
      <span class="muted" data-content-path="${tileObj.id}" style="font-size:10px;"></span>
      <button class="tile-btn" data-content-open="${tileObj.id}" title="Open folder">&#128193;</button>
      <button class="tile-btn" data-content-refresh="${tileObj.id}" title="Refresh">&#8635;</button>
    </div>
    <div class="content-list" data-content-list="${tileObj.id}"></div>
  `;

  const filterSelect = body.querySelector(`[data-content-filter="${tileObj.id}"]`);
  const listEl = body.querySelector(`[data-content-list="${tileObj.id}"]`);
  filterSelect.addEventListener("change", () => doRefresh());

  body.querySelector(`[data-content-open="${tileObj.id}"]`).addEventListener("click", async () => {
    try { await invoke("open_content_dir"); } catch {}
  });

  body.querySelector(`[data-content-refresh="${tileObj.id}"]`).addEventListener("click", () => doRefresh());

  // Drag-and-drop
  listEl.addEventListener("dragover", (e) => { e.preventDefault(); listEl.classList.add("drag-over"); });
  listEl.addEventListener("dragleave", () => listEl.classList.remove("drag-over"));
  listEl.addEventListener("drop", (e) => { e.preventDefault(); listEl.classList.remove("drag-over"); });

  async function doRefresh() {
    const pathLabel = body.querySelector(`[data-content-path="${tileObj.id}"]`);

    try {
      const paths = await invoke("get_content_path");
      const dir = field(paths, "contentDir", "content_dir");
      pathLabel.textContent = dir;
    } catch {}

    let items;
    try { items = await invoke("get_content_files"); } catch { items = []; }

    const filter = filterSelect.value;
    const filtered = filter === "all" ? items : items.filter(i => field(i, "kind", "kind") === filter);

    listEl.innerHTML = "";
    if (!filtered.length) {
      listEl.innerHTML = `<div class="feed-empty">${filter === "all" ? "Drop files here or add to content folder" : `No ${filter} files`}</div>`;
      return;
    }

    for (const item of filtered) {
      const name = field(item, "name", "name");
      const kind = field(item, "kind", "kind");
      const size = field(item, "size", "size");
      const path = field(item, "path", "path");
      const modifiedAt = field(item, "modifiedAt", "modified_at");

      const row = document.createElement("div");
      row.className = "content-row";

      // Thumbnail or icon
      let thumbHtml;
      if (kind === "image") {
        const src = convertFileSrc(path);
        thumbHtml = `<img class="content-thumb" src="${src}" loading="lazy" />`;
      } else {
        thumbHtml = `<div class="content-icon">${KIND_ICONS[kind] || KIND_ICONS.file}</div>`;
      }

      row.innerHTML = `
        ${thumbHtml}
        <div class="content-meta">
          <strong>${name} <span class="content-badge content-badge--${kind}">${kind}</span></strong>
          <span class="muted">${formatFileSize(size)} · ${formatModifiedAt(modifiedAt)}</span>
        </div>
        <button class="task-action-btn danger">Delete</button>
      `;

      // Preview on row click (but not on delete button)
      row.addEventListener("click", (e) => {
        if (e.target.closest("button")) return;
        showContentPreview(item);
      });

      row.querySelector("button").addEventListener("click", async (e) => {
        e.stopPropagation();
        await invoke("delete_content_file", { req: { kind, name } });
        doRefresh();
      });

      listEl.appendChild(row);
    }
  }

  doRefresh();
}

export function showContentPreview(item) {
  const name = field(item, "name", "name");
  const kind = field(item, "kind", "kind");
  const path = field(item, "path", "path");

  const overlay = document.createElement("div");
  overlay.className = "content-preview-overlay";

  function escHandler(e) {
    if (e.key === "Escape") close();
  }

  const close = () => {
    document.removeEventListener("keydown", escHandler);
    overlay.remove();
  };

  overlay.innerHTML = `
    <div class="content-preview-header">
      <span>${escapeHtml(name)}</span>
      <div style="display:flex;gap:6px;">
        <button data-preview-open>Open in App</button>
        <button data-preview-close>Close</button>
      </div>
    </div>
    <div class="content-preview-body"></div>
  `;

  const previewBody = overlay.querySelector(".content-preview-body");
  overlay.querySelector("[data-preview-close]").addEventListener("click", close);
  overlay.querySelector("[data-preview-open]").addEventListener("click", async () => {
    try { await invoke("open_content_file", { req: { path } }); } catch {}
  });
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) close();
  });

  if (kind === "image") {
    const img = document.createElement("img");
    img.src = convertFileSrc(path);
    previewBody.appendChild(img);
  } else if (kind === "audio") {
    const audio = document.createElement("audio");
    audio.controls = true;
    audio.src = convertFileSrc(path);
    previewBody.appendChild(audio);
  } else if (kind === "video") {
    const video = document.createElement("video");
    video.controls = true;
    video.src = convertFileSrc(path);
    previewBody.appendChild(video);
  } else if (kind === "text") {
    const pre = document.createElement("pre");
    pre.textContent = "Loading...";
    previewBody.appendChild(pre);
    invoke("read_content_text", { req: { path } }).then(resp => {
      pre.textContent = field(resp, "content", "content");
    }).catch(() => { pre.textContent = "Failed to load file."; });
  } else {
    previewBody.innerHTML = `<div style="color:var(--muted);text-align:center;padding:40px;">No preview available — use "Open in App" to view this file.</div>`;
  }

  document.addEventListener("keydown", escHandler);

  document.body.appendChild(overlay);
}


registerTileRenderer("content", renderContentTile);
