import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { ui } from "./state.js";
import { escapeHtml, field, formatModifiedAt, parseAllowedTools } from "./utils.js";
import { emptyState } from "./uikit.js";
import { icon } from "./icons.js";
import { refreshSkillsList } from "./sidebar.js";
import { getTilesByType } from "./tilesys.js";

// ========== SKILLS TILE ==========
// Browse and edit the SKILL.md folders (skills.rs). A skill is a name, a
// description (how the model decides to use it), optional allowed-tools, and
// markdown instructions; run one directly with /name. Shares the Memory tile's
// list + editor layout.
export function renderSkillsTile(tileObj, body) {
  body.innerHTML = `
    <div class="memory-notes">
      <div class="memory-notes-side">
        <div class="memory-notes-toolbar">
          <input type="text" placeholder="Filter skills…" data-sk-filter="${tileObj.id}" />
          <button class="tile-btn" data-sk-new="${tileObj.id}" title="New skill" aria-label="New skill">${icon("plus", 14)}</button>
          <button class="tile-btn" data-sk-folder="${tileObj.id}" title="Open skills folder in Finder" aria-label="Open skills folder">${icon("folder", 14)}</button>
        </div>
        <div class="memory-note-list" data-sk-list="${tileObj.id}"></div>
      </div>
      <div class="memory-editor" data-sk-editor="${tileObj.id}" data-placeholder="Select a skill, or create one with +">
        <input type="text" class="memory-editor-name" placeholder="skill-name (run it with /skill-name)" data-sk-name="${tileObj.id}" spellcheck="false" />
        <input type="text" class="memory-editor-desc" placeholder="What it does and when to use it — the model picks skills by this" data-sk-desc="${tileObj.id}" />
        <input type="text" class="memory-editor-desc" placeholder="Allowed tools (optional, comma-separated — blank = all)" data-sk-tools="${tileObj.id}" spellcheck="false" />
        <textarea class="memory-editor-body" placeholder="Instructions (markdown)…" data-sk-body="${tileObj.id}"></textarea>
        <div class="muted text-xs" data-sk-files="${tileObj.id}"></div>
        <div class="memory-editor-actions">
          <span class="muted text-xs" data-sk-status="${tileObj.id}"></span>
          <span class="tile-spacer"></span>
          <button class="form-btn" data-sk-delete="${tileObj.id}">Delete</button>
          <button class="form-btn" data-sk-save="${tileObj.id}">Save</button>
        </div>
      </div>
    </div>
  `;

  const q = (attr) => body.querySelector(`[data-${attr}="${tileObj.id}"]`);
  const filterInput = q("sk-filter");
  const listEl = q("sk-list");
  const editorEl = q("sk-editor");
  const nameInput = q("sk-name");
  const descInput = q("sk-desc");
  const toolsInput = q("sk-tools");
  const bodyInput = q("sk-body");
  const filesEl = q("sk-files");
  const statusEl = q("sk-status");
  const deleteBtn = q("sk-delete");

  let skills = [];
  let current = null; // name of the skill open in the editor (null = new/unsaved)
  let dirty = false;

  const setStatus = (text) => { statusEl.textContent = text; };
  [nameInput, descInput, toolsInput, bodyInput].forEach(el =>
    el.addEventListener("input", () => { dirty = true; setStatus("Unsaved changes"); }));

  const confirmDiscard = async () =>
    !dirty || ask("Discard unsaved changes to this skill?", { title: "Unsaved changes", kind: "warning" });

  function openEditor(skill) {
    current = skill ? skill.name : null;
    nameInput.value = skill ? skill.name : "";
    descInput.value = skill ? (skill.description || "") : "";
    toolsInput.value = skill ? (field(skill, "allowedTools", "allowed_tools") || []).join(", ") : "";
    bodyInput.value = skill ? (skill.body || "") : "";
    const files = (skill && skill.files) || [];
    filesEl.textContent = files.length ? `Reference files: ${files.join(", ")}` : "";
    dirty = false;
    deleteBtn.hidden = !current;
    editorEl.classList.remove("is-empty");
    setStatus(skill && skill.updatedAt ? `Updated ${formatModifiedAt(String(skill.updatedAt))}` : "");
    renderList();
  }

  function showPlaceholder() {
    current = null;
    dirty = false;
    editorEl.classList.add("is-empty");
    renderList();
  }

  function renderList() {
    const f = filterInput.value.trim().toLowerCase();
    const shown = skills.filter(s => !f || s.name.includes(f) || (s.description || "").toLowerCase().includes(f));
    listEl.innerHTML = "";
    for (const s of shown) {
      const row = document.createElement("button");
      row.type = "button";
      row.className = `memory-note-item${s.name === current ? " active" : ""}`;
      row.innerHTML = `
        <span class="memory-note-name">/${escapeHtml(s.name)}</span>
        <span class="memory-note-desc">${escapeHtml(s.description || "")}</span>
      `;
      row.addEventListener("click", async () => {
        if (!(await confirmDiscard())) return;
        try { openEditor(await invoke("read_skill", { name: s.name })); } catch (e) { setStatus(String(e)); }
      });
      listEl.appendChild(row);
    }
    if (!skills.length) {
      listEl.appendChild(emptyState({ icon: "sparkles", iconSize: 20, text: "No skills yet. Create one with +, ask Muninn to make one, or drop SKILL.md folders into the skills folder." }));
    } else if (!shown.length) {
      listEl.appendChild(emptyState({ icon: "search", iconSize: 20, text: "No skills match." }));
    }
  }

  async function refreshList() {
    try { skills = await invoke("list_skills"); } catch { skills = []; }
    ui.cachedSkills = skills;
    renderList();
  }

  // Called on `skills-changed` (Muninn's skill tool or another window). Leave an
  // in-progress edit alone; otherwise keep the open skill in sync.
  tileObj.refreshSkills = async () => {
    await refreshList();
    if (!current || dirty) return;
    if (!skills.some(s => s.name === current)) { showPlaceholder(); return; }
    try { openEditor(await invoke("read_skill", { name: current })); } catch {}
  };

  filterInput.addEventListener("input", renderList);

  q("sk-new").addEventListener("click", async () => {
    if (!(await confirmDiscard())) return;
    openEditor(null);
    nameInput.focus();
  });

  q("sk-folder").addEventListener("click", () => { invoke("open_skills_folder").catch(() => {}); });

  q("sk-save").addEventListener("click", async () => {
    const name = nameInput.value.trim();
    if (!name) { setStatus("Give the skill a name first"); nameInput.focus(); return; }
    if (!descInput.value.trim()) { setStatus("Add a description — it's how Muninn knows when to use it"); descInput.focus(); return; }
    try {
      const saved = await invoke("save_skill", {
        name,
        description: descInput.value,
        body: bodyInput.value,
        allowedTools: parseAllowedTools(toolsInput.value),
        previousName: current,
      });
      dirty = false;
      await refreshList();
      openEditor(await invoke("read_skill", { name: saved }));
      setStatus("Saved");
    } catch (e) {
      setStatus(String(e));
    }
  });

  deleteBtn.addEventListener("click", async () => {
    if (!current) return;
    if (!(await ask(`Delete the skill "${current}" and its folder?`, { title: "Delete skill", kind: "warning" }))) return;
    try {
      await invoke("delete_skill", { name: current });
      await refreshList();
      showPlaceholder();
    } catch (e) {
      setStatus(String(e));
    }
  });

  showPlaceholder();
  refreshList();
}

/** `skills-changed`: refresh the shared cache, the sidebar list, and open tiles. */
export async function onSkillsChanged() {
  try { ui.cachedSkills = await invoke("list_skills"); } catch { /* keep old cache */ }
  try { refreshSkillsList(); } catch { /* sidebar absent in pop-out */ }
  for (const t of getTilesByType("skills")) {
    if (typeof t.refreshSkills === "function") t.refreshSkills();
  }
}

registerTileRenderer("skills", renderSkillsTile);
