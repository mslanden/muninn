import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { escapeHtml, field, formatModifiedAt } from "./utils.js";
import { emptyState } from "./uikit.js";
import { icon } from "./icons.js";

// The note that is always in the model's context (memory.rs PINNED_NOTE).
const PINNED = "about-me";

// ========== MEMORY TILE ==========
// Notes: the model-managed memory folder (one markdown note per topic), browsable
// and hand-editable. History: past conversations.
export function renderMemoryTile(tileObj, body) {
  body.innerHTML = `
    <div class="memory-tabs">
      <button class="memory-tab active" data-mtab="notes">Notes</button>
      <button class="memory-tab" data-mtab="history">History</button>
    </div>
    <div data-memory-view="notes" class="memory-notes">
      <div class="memory-notes-side">
        <div class="memory-notes-toolbar">
          <input type="text" placeholder="Filter notes…" data-mem-filter="${tileObj.id}" />
          <button class="tile-btn" data-mem-new="${tileObj.id}" title="New note" aria-label="New note">${icon("plus", 14)}</button>
          <button class="tile-btn" data-mem-folder="${tileObj.id}" title="Open memory folder in Finder" aria-label="Open memory folder">${icon("folder", 14)}</button>
        </div>
        <div class="memory-note-list" data-mem-list="${tileObj.id}"></div>
      </div>
      <div class="memory-editor" data-mem-editor="${tileObj.id}">
        <input type="text" class="memory-editor-name" placeholder="note-name" data-mem-name="${tileObj.id}" spellcheck="false" />
        <input type="text" class="memory-editor-desc" placeholder="One-line description — what this note holds" data-mem-desc="${tileObj.id}" />
        <textarea class="memory-editor-body" placeholder="Markdown…" data-mem-body="${tileObj.id}"></textarea>
        <div class="memory-editor-actions">
          <span class="muted text-xs" data-mem-status="${tileObj.id}"></span>
          <span class="tile-spacer"></span>
          <button class="form-btn" data-mem-delete="${tileObj.id}">Delete</button>
          <button class="form-btn" data-mem-save="${tileObj.id}">Save</button>
        </div>
      </div>
    </div>
    <div data-memory-view="history" class="hidden p-2">
      <div style="padding:6px 6px 0;">
        <div class="form-row mb-2">
          <input type="text" placeholder="Search messages..." data-hist-search="${tileObj.id}" />
          <button class="form-btn" data-hist-go="${tileObj.id}">Search</button>
        </div>
      </div>
      <div class="memory-list" data-history-list="${tileObj.id}"></div>
      <div class="memory-list hidden" data-history-detail="${tileObj.id}" style="border-top:1px solid var(--border);"></div>
    </div>
  `;

  const tabs = body.querySelectorAll(".memory-tab");
  const views = {
    notes: body.querySelector('[data-memory-view="notes"]'),
    history: body.querySelector('[data-memory-view="history"]'),
  };
  tabs.forEach(tab => {
    tab.addEventListener("click", () => {
      const active = tab.dataset.mtab;
      tabs.forEach(t => t.classList.toggle("active", t.dataset.mtab === active));
      Object.entries(views).forEach(([k, v]) => v.classList.toggle("hidden", k !== active));
      if (active === "history") refreshHistory();
    });
  });

  // ---- Notes ----
  const q = (attr) => body.querySelector(`[data-${attr}="${tileObj.id}"]`);
  const filterInput = q("mem-filter");
  const listEl = q("mem-list");
  const editorEl = q("mem-editor");
  const nameInput = q("mem-name");
  const descInput = q("mem-desc");
  const bodyInput = q("mem-body");
  const statusEl = q("mem-status");
  const deleteBtn = q("mem-delete");
  const saveBtn = q("mem-save");

  let notes = [];
  let current = null; // name of the note open in the editor (null = new/unsaved)
  let dirty = false;

  const setStatus = (text) => { statusEl.textContent = text; };
  const markDirty = () => { dirty = true; setStatus("Unsaved changes"); };
  [nameInput, descInput, bodyInput].forEach(el => el.addEventListener("input", markDirty));

  function openEditor(note) {
    current = note ? note.name : null;
    nameInput.value = note ? note.name : "";
    descInput.value = note ? (note.description || "") : "";
    bodyInput.value = note ? (note.body || "") : "";
    dirty = false;
    deleteBtn.hidden = !current;
    editorEl.classList.remove("is-empty");
    setStatus(note && note.updatedAt ? `Updated ${formatModifiedAt(String(note.updatedAt))}` : "");
    renderList();
  }

  function showEditorPlaceholder() {
    current = null;
    dirty = false;
    editorEl.classList.add("is-empty");
    renderList();
  }

  async function selectNote(name) {
    if (dirty && !(await ask("Discard unsaved changes to this note?", { title: "Unsaved changes", kind: "warning" }))) return;
    try {
      const note = await invoke("memory_read_note", { name });
      openEditor(note);
    } catch {
      // The pinned note may not exist yet — open it blank, ready to write.
      if (name === PINNED) {
        openEditor(null);
        nameInput.value = PINNED;
        descInput.value = "Facts about the user";
      }
    }
  }

  function renderList() {
    const f = filterInput.value.trim().toLowerCase();
    const matches = (n) => !f || n.name.includes(f) || (n.description || "").toLowerCase().includes(f);
    const pinned = notes.find(n => n.name === PINNED) || { name: PINNED, description: "Not written yet — facts about you", missing: true };
    const others = notes.filter(n => n.name !== PINNED && matches(n));

    listEl.innerHTML = "";
    if (matches(pinned)) listEl.appendChild(noteRow(pinned, true));
    for (const n of others) listEl.appendChild(noteRow(n, false));

    if (!notes.filter(n => n.name !== PINNED).length && !f) {
      listEl.appendChild(emptyState({ icon: "brain", iconSize: 20, text: "No memories yet. Muninn saves what's worth remembering as you chat." }));
    } else if (!others.length && f) {
      listEl.appendChild(emptyState({ icon: "search", iconSize: 20, text: "No notes match." }));
    }
  }

  function noteRow(n, isPinned) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = `memory-note-item${n.name === current ? " active" : ""}${isPinned ? " pinned" : ""}${n.missing ? " missing" : ""}`;
    row.innerHTML = `
      <span class="memory-note-name">${isPinned ? icon("pin", 11) : ""}${escapeHtml(n.name)}</span>
      <span class="memory-note-desc">${escapeHtml(n.description || "")}</span>
    `;
    row.addEventListener("click", () => selectNote(n.name));
    return row;
  }

  async function refreshNotes() {
    try { notes = await invoke("memory_list_notes"); } catch { notes = []; }
    renderList();
  }

  // Called when the model (or another window) changes memory. Leave an in-progress
  // edit alone; otherwise reload the open note so the editor never shows stale text.
  tileObj.refreshMemory = async () => {
    await refreshNotes();
    if (!current || dirty) return;
    if (!notes.some(n => n.name === current)) { showEditorPlaceholder(); return; }
    try { openEditor(await invoke("memory_read_note", { name: current })); } catch {}
  };

  filterInput.addEventListener("input", renderList);

  q("mem-new").addEventListener("click", async () => {
    if (dirty && !(await ask("Discard unsaved changes to this note?", { title: "Unsaved changes", kind: "warning" }))) return;
    openEditor(null);
    nameInput.focus();
  });

  q("mem-folder").addEventListener("click", () => { invoke("memory_open_folder").catch(() => {}); });

  saveBtn.addEventListener("click", async () => {
    const name = nameInput.value.trim();
    if (!name) { setStatus("Give the note a name first"); nameInput.focus(); return; }
    try {
      const saved = await invoke("memory_save_note", {
        name,
        description: descInput.value,
        body: bodyInput.value,
        previousName: current,
      });
      dirty = false;
      await refreshNotes();
      openEditor(await invoke("memory_read_note", { name: saved }));
      setStatus("Saved");
    } catch (e) {
      setStatus(String(e));
    }
  });

  deleteBtn.addEventListener("click", async () => {
    if (!current) return;
    const yes = await ask(`Delete the memory note "${current}"?`, { title: "Delete note", kind: "warning" });
    if (!yes) return;
    try {
      await invoke("memory_delete_note", { name: current });
      await refreshNotes();
      showEditorPlaceholder();
    } catch (e) {
      setStatus(String(e));
    }
  });

  // ---- History ----
  const histSearch = body.querySelector(`[data-hist-search="${tileObj.id}"]`);
  const histList = body.querySelector(`[data-history-list="${tileObj.id}"]`);
  const histDetail = body.querySelector(`[data-history-detail="${tileObj.id}"]`);

  body.querySelector(`[data-hist-go="${tileObj.id}"]`).addEventListener("click", () => refreshHistory());
  histSearch.addEventListener("keydown", (e) => { if (e.key === "Enter") refreshHistory(); });

  async function refreshHistory() {
    const query = histSearch.value.trim();
    histDetail.classList.add("hidden");
    histDetail.innerHTML = "";

    let sessions;
    try {
      if (query) {
        sessions = await invoke("search_messages", { req: { query, limit: 30 } });
      } else {
        sessions = await invoke("get_sessions", { req: { limit: 30 } });
      }
    } catch { sessions = []; }

    histList.innerHTML = "";
    if (!sessions.length) {
      histList.replaceChildren(emptyState({ icon: query ? "search" : "message-square", text: query ? "No matches" : "No conversations yet" }));
      return;
    }

    for (const session of sessions) {
      const sid = field(session, "id", "id");
      const preview = field(session, "preview", "preview") || "";
      const msgCount = field(session, "messageCount", "message_count") || 0;
      const lastAt = field(session, "lastMessageAt", "last_message_at") || field(session, "createdAt", "created_at") || "";

      const row = document.createElement("div");
      row.className = "entity-card";
      row.innerHTML = `
        <div class="entity-top">
          <span class="entity-name">${escapeHtml(preview.substring(0, 80)) || "Empty session"}</span>
          <span class="entity-delete" title="Delete session">&times;</span>
        </div>
        <div class="entity-summary">${escapeHtml(lastAt)} &middot; ${msgCount} msgs</div>
      `;

      row.querySelector(".entity-delete").addEventListener("click", async (e) => {
        e.stopPropagation();
        try { await invoke("delete_session", { sessionId: sid }); } catch {}
        refreshHistory();
      });

      row.addEventListener("click", async (e) => {
        if (e.target.classList.contains("entity-delete")) return;
        const messages = await invoke("get_messages", { sessionId: sid });
        histDetail.classList.remove("hidden");
        histDetail.innerHTML = "";
        for (const msg of messages) {
          const role = field(msg, "role", "role");
          const content = field(msg, "content", "content") || "";
          const el = document.createElement("div");
          el.className = `history-msg history-msg--${role}`;
          el.innerHTML = `<div class="history-role">${escapeHtml(role)}</div><div class="history-content">${escapeHtml(content.substring(0, 500))}${content.length > 500 ? "..." : ""}</div>`;
          histDetail.appendChild(el);
        }
      });

      histList.appendChild(row);
    }
  }

  showEditorPlaceholder();
  refreshNotes();
}


registerTileRenderer("memory", renderMemoryTile);
