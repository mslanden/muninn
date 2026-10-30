import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { field } from "./utils.js";
import { icon } from "./icons.js";
import { openTileForType } from "./tilesys.js";

// ========== LIBRARY TILE ==========
// Everything Muninn keeps for you, one click from the sidebar: memory notes, skills,
// and the content folder. Each card opens that tile; counts load in the background.
const ENTRIES = [
  { type: "memory", icon: "brain", title: "Memory", blurb: "Notes Muninn keeps about you and your work" },
  { type: "skills", icon: "sparkles", title: "Skills", blurb: "Reusable instructions — run one with /name" },
  { type: "content", icon: "image", title: "Content", blurb: "Images, videos and documents" },
];

export function renderLibraryTile(tileObj, body) {
  body.innerHTML = `
    <div class="library">
      <p class="library-intro">Everything Muninn keeps for you.</p>
      <div class="library-grid">
        ${ENTRIES.map(e => `
          <button type="button" class="library-card" data-lib="${e.type}">
            <span class="conn-icon">${icon(e.icon, 18)}</span>
            <span class="conn-text">
              <span class="conn-name">${e.title}</span>
              <span class="conn-detail" data-lib-count="${e.type}">${e.blurb}</span>
            </span>
          </button>`).join("")}
      </div>
    </div>`;
  body.querySelectorAll("[data-lib]").forEach(b =>
    b.addEventListener("click", () => openTileForType(b.dataset.lib)));
  fillCounts(body);
}

async function fillCounts(body) {
  const set = (type, text) => {
    const el = body.querySelector(`[data-lib-count="${type}"]`);
    if (el && text) el.textContent = text;
  };
  const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
  try {
    const notes = await invoke("memory_list_notes");
    const pinned = notes.some(n => field(n, "name") === "about-me");
    set("memory", notes.length ? `${plural(notes.length, "note")}${pinned ? " · about-me pinned" : ""}` : "No memories yet");
  } catch { /* keep blurb */ }
  try {
    const skills = await invoke("list_skills");
    set("skills", skills.length ? skills.slice(0, 3).map(s => `/${field(s, "name")}`).join(" · ") + (skills.length > 3 ? ` +${skills.length - 3}` : "") : "No skills yet");
  } catch { /* keep blurb */ }
  try {
    const files = await invoke("get_content_files");
    if (Array.isArray(files)) set("content", files.length ? plural(files.length, "file") : "Empty — drop files in to add them");
  } catch { /* keep blurb */ }
}

registerTileRenderer("library", renderLibraryTile);
