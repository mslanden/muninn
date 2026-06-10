import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { escapeHtml, field } from "./utils.js";

// ========== MEMORY TILE ==========
export function renderMemoryTile(tileObj, body) {
  body.innerHTML = `
    <div class="memory-tabs">
      <button class="memory-tab active" data-mtab="profile">Profile</button>
      <button class="memory-tab" data-mtab="entities">Entities</button>
      <button class="memory-tab" data-mtab="timeline">Timeline</button>
      <button class="memory-tab" data-mtab="history">History</button>
    </div>
    <div data-memory-view="profile" class="profile-section">
      <div class="profile-field">
        <div class="profile-label">NAME</div>
        <input type="text" data-mem-name="${tileObj.id}" />
      </div>
      <div class="profile-field">
        <div class="profile-label">ABOUT</div>
        <textarea rows="2" data-mem-about="${tileObj.id}"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">IDENTITY</div>
        <textarea rows="2" data-mem-identity="${tileObj.id}" placeholder="Role, job, background"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">PERSONALITY</div>
        <textarea rows="2" data-mem-personality="${tileObj.id}" placeholder="Communication style, temperament"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">GOALS</div>
        <textarea rows="2" data-mem-goals="${tileObj.id}" placeholder="What you're working toward"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">PREFERENCES</div>
        <textarea rows="2" data-mem-preferences="${tileObj.id}" placeholder="Likes, dislikes, tool preferences"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">CURRENT FOCUS</div>
        <textarea rows="2" data-mem-currentstate="${tileObj.id}" placeholder="What you're currently doing"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">PATTERNS</div>
        <textarea rows="2" data-mem-patterns="${tileObj.id}" placeholder="Recurring behaviors or habits"></textarea>
      </div>
      <div class="profile-field">
        <div class="profile-label">NOTES</div>
        <textarea rows="3" data-mem-profile="${tileObj.id}" placeholder="Additional notes"></textarea>
      </div>
      <button class="form-btn" data-mem-save="${tileObj.id}">Save Profile</button>
    </div>
    <div data-memory-view="entities" class="hidden" style="padding:6px;">
      <div style="padding:6px 6px 0;">
        <div class="form-row" style="margin-bottom:8px;">
          <input type="text" placeholder="Kind" data-ent-kind="${tileObj.id}" style="width:100px" />
          <input type="text" placeholder="Name" data-ent-name="${tileObj.id}" />
        </div>
        <input type="text" placeholder="Aliases (comma-separated)" data-ent-aliases="${tileObj.id}" style="margin-bottom:6px;" />
        <textarea rows="2" placeholder="Summary" data-ent-summary="${tileObj.id}" style="margin-bottom:6px;"></textarea>
        <div class="form-row" style="margin-bottom:8px;">
          <button class="form-btn" data-ent-save="${tileObj.id}">Save Entity</button>
          <button class="form-btn" data-ent-reset="${tileObj.id}">Reset</button>
        </div>
      </div>
      <div class="memory-section-label">RELATIONSHIPS</div>
      <div class="memory-list" data-relationships-list="${tileObj.id}"></div>
      <div class="memory-section-label">ENTITIES</div>
      <div class="memory-list" data-entities-list="${tileObj.id}"></div>
    </div>
    <div data-memory-view="timeline" class="hidden" style="padding:6px;">
      <div style="padding:6px 6px 0;">
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Description" data-ep-desc="${tileObj.id}" />
          <input type="text" placeholder="Timestamp" data-ep-time="${tileObj.id}" style="width:140px" />
          <button class="form-btn" data-ep-add="${tileObj.id}">Add</button>
        </div>
      </div>
      <div class="memory-list" data-episodes-list="${tileObj.id}"></div>
    </div>
    <div data-memory-view="history" class="hidden" style="padding:6px;">
      <div style="padding:6px 6px 0;">
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Search messages..." data-hist-search="${tileObj.id}" />
          <button class="form-btn" data-hist-go="${tileObj.id}">Search</button>
        </div>
      </div>
      <div class="memory-list" data-history-list="${tileObj.id}"></div>
      <div class="memory-list hidden" data-history-detail="${tileObj.id}" style="border-top:1px solid var(--border);"></div>
    </div>
  `;

  let activeTab = "profile";
  let editingEntityId = null;

  const tabs = body.querySelectorAll(".memory-tab");
  const views = {
    profile: body.querySelector('[data-memory-view="profile"]'),
    entities: body.querySelector('[data-memory-view="entities"]'),
    timeline: body.querySelector('[data-memory-view="timeline"]'),
    history: body.querySelector('[data-memory-view="history"]')
  };

  tabs.forEach(tab => {
    tab.addEventListener("click", () => {
      activeTab = tab.dataset.mtab;
      tabs.forEach(t => t.classList.toggle("active", t.dataset.mtab === activeTab));
      Object.entries(views).forEach(([k, v]) => v.classList.toggle("hidden", k !== activeTab));
    });
  });

  // Profile
  const memName = body.querySelector(`[data-mem-name="${tileObj.id}"]`);
  const memAbout = body.querySelector(`[data-mem-about="${tileObj.id}"]`);
  const memProfile = body.querySelector(`[data-mem-profile="${tileObj.id}"]`);
  const memIdentity = body.querySelector(`[data-mem-identity="${tileObj.id}"]`);
  const memPersonality = body.querySelector(`[data-mem-personality="${tileObj.id}"]`);
  const memGoals = body.querySelector(`[data-mem-goals="${tileObj.id}"]`);
  const memPreferences = body.querySelector(`[data-mem-preferences="${tileObj.id}"]`);
  const memCurrentState = body.querySelector(`[data-mem-currentstate="${tileObj.id}"]`);
  const memPatterns = body.querySelector(`[data-mem-patterns="${tileObj.id}"]`);

  body.querySelector(`[data-mem-save="${tileObj.id}"]`).addEventListener("click", async () => {
    await invoke("set_memory_profile", {
      req: {
        userName: memName.value.trim(),
        userAbout: memAbout.value.trim(),
        profileText: memProfile.value.trim(),
        identity: memIdentity.value.trim(),
        personality: memPersonality.value.trim(),
        goals: memGoals.value.trim(),
        preferences: memPreferences.value.trim(),
        currentState: memCurrentState.value.trim(),
        patterns: memPatterns.value.trim(),
      }
    });
    refreshProfile();
  });

  async function refreshProfile() {
    let profile;
    try { profile = await invoke("get_memory_profile"); } catch { return; }
    memName.value = field(profile, "userName", "user_name") || "";
    memAbout.value = field(profile, "userAbout", "user_about") || "";
    memProfile.value = field(profile, "profileText", "profile_text") || "";
    memIdentity.value = field(profile, "identity", "identity") || "";
    memPersonality.value = field(profile, "personality", "personality") || "";
    memGoals.value = field(profile, "goals", "goals") || "";
    memPreferences.value = field(profile, "preferences", "preferences") || "";
    memCurrentState.value = field(profile, "currentState", "current_state") || "";
    memPatterns.value = field(profile, "patterns", "patterns") || "";
  }

  // Entities
  const entKind = body.querySelector(`[data-ent-kind="${tileObj.id}"]`);
  const entName = body.querySelector(`[data-ent-name="${tileObj.id}"]`);
  const entSummary = body.querySelector(`[data-ent-summary="${tileObj.id}"]`);
  const entAliases = body.querySelector(`[data-ent-aliases="${tileObj.id}"]`);

  body.querySelector(`[data-ent-save="${tileObj.id}"]`).addEventListener("click", async () => {
    const kind = entKind.value.trim();
    const name = entName.value.trim();
    if (!kind || !name) return;
    const aliases = entAliases.value.trim() ? entAliases.value.split(",").map(a => a.trim()).filter(Boolean) : [];
    await invoke("upsert_entity", { req: { id: editingEntityId, kind, name, summary: entSummary.value.trim(), aliases } });
    editingEntityId = null;
    entKind.value = "";
    entName.value = "";
    entSummary.value = "";
    entAliases.value = "";
    refreshEntities();
    refreshRelationships();
  });

  body.querySelector(`[data-ent-reset="${tileObj.id}"]`).addEventListener("click", () => {
    editingEntityId = null;
    entKind.value = "";
    entName.value = "";
    entSummary.value = "";
    entAliases.value = "";
  });

  async function refreshEntities() {
    const list = body.querySelector(`[data-entities-list="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_entities"); } catch { items = []; }
    list.innerHTML = "";

    if (!items.length) {
      list.innerHTML = `<div class="feed-empty">No entities yet</div>`;
      return;
    }

    for (const item of items) {
      const id = field(item, "id", "id");
      const kind = field(item, "kind", "kind");
      const name = field(item, "name", "name");
      const summary = field(item, "summary", "summary");
      const aliases = field(item, "aliases", "aliases") || [];

      const kindMap = { person: "person", place: "place", project: "project", organization: "organization", concept: "concept", tool: "tool", thing: "concept" };
      const kindClass = kindMap[kind] || "concept";

      const card = document.createElement("div");
      card.className = "entity-card";
      card.innerHTML = `
        <div class="entity-top">
          <span class="entity-kind ${kindClass}">${escapeHtml(kind)}</span>
          <span class="entity-name">${escapeHtml(name)}</span>
          <span class="entity-delete" title="Delete entity">&times;</span>
        </div>
        ${aliases.length ? `<div class="entity-aliases">aka ${escapeHtml(aliases.join(", "))}</div>` : ""}
        <div class="entity-summary">${escapeHtml(summary) || "No summary"}</div>
      `;

      card.querySelector(".entity-delete").addEventListener("click", async (e) => {
        e.stopPropagation();
        await invoke("delete_entity", { req: { id } });
        refreshEntities();
        refreshRelationships();
      });

      card.addEventListener("click", (e) => {
        if (e.target.classList.contains("entity-delete")) return;
        editingEntityId = id;
        entKind.value = kind;
        entName.value = name;
        entSummary.value = summary || "";
        entAliases.value = aliases.join(", ");
      });

      list.appendChild(card);
    }
  }

  // Relationships
  async function refreshRelationships() {
    const list = body.querySelector(`[data-relationships-list="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_relationships"); } catch { items = []; }
    list.innerHTML = "";

    if (!items.length) {
      list.innerHTML = `<div class="feed-empty">No relationships yet</div>`;
      return;
    }

    for (const item of items) {
      const id = field(item, "id", "id");
      const fromName = field(item, "fromName", "from_name");
      const toName = field(item, "toName", "to_name");
      const label = field(item, "label", "label");

      const el = document.createElement("div");
      el.className = "relationship-card";
      el.innerHTML = `
        <span class="rel-from">${escapeHtml(fromName)}</span>
        <span class="rel-arrow">&rarr;</span>
        <span class="rel-label">${escapeHtml(label)}</span>
        <span class="rel-arrow">&rarr;</span>
        <span class="rel-to">${escapeHtml(toName)}</span>
        <span class="entity-delete" title="Delete">&times;</span>
      `;
      el.querySelector(".entity-delete").addEventListener("click", async () => {
        await invoke("delete_relationship", { req: { id } });
        refreshRelationships();
      });
      list.appendChild(el);
    }
  }

  // Episodes
  const epDesc = body.querySelector(`[data-ep-desc="${tileObj.id}"]`);
  const epTime = body.querySelector(`[data-ep-time="${tileObj.id}"]`);

  body.querySelector(`[data-ep-add="${tileObj.id}"]`).addEventListener("click", async () => {
    const description = epDesc.value.trim();
    if (!description) return;
    await invoke("add_episode", { req: { description, timestamp: epTime.value.trim() } });
    epDesc.value = "";
    epTime.value = "";
    refreshEpisodes();
  });

  async function refreshEpisodes() {
    const list = body.querySelector(`[data-episodes-list="${tileObj.id}"]`);
    let items;
    try { items = await invoke("get_episodes"); } catch { items = []; }
    list.innerHTML = "";

    if (!items.length) {
      list.innerHTML = `<div class="feed-empty">No timeline events yet</div>`;
      return;
    }

    for (const item of items) {
      const id = field(item, "id", "id");
      const description = field(item, "description", "description");
      const timestamp = field(item, "timestamp", "timestamp");
      const createdAt = field(item, "createdAt", "created_at");
      const entityNames = field(item, "entityNames", "entity_names") || [];

      const el = document.createElement("div");
      el.className = "entity-card";
      el.innerHTML = `
        <div class="entity-top">
          <span class="entity-name">${escapeHtml(description)}</span>
          <span class="entity-delete" title="Delete">&times;</span>
        </div>
        <div class="entity-summary">${escapeHtml(timestamp || createdAt)}</div>
        ${entityNames.length ? `<div class="episode-entities">${entityNames.map(n => `<span class="episode-entity-tag">${escapeHtml(n)}</span>`).join("")}</div>` : ""}
      `;
      el.querySelector(".entity-delete").addEventListener("click", async (e) => {
        e.stopPropagation();
        await invoke("delete_episode", { req: { id } });
        refreshEpisodes();
      });
      list.appendChild(el);
    }
  }

  // History
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
      histList.innerHTML = `<div class="feed-empty">${query ? "No matches" : "No conversations yet"}</div>`;
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
        <div class="entity-summary">${lastAt} &middot; ${msgCount} msgs</div>
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

  refreshProfile();
  refreshEntities();
  refreshRelationships();
  refreshEpisodes();
  refreshHistory();
}


registerTileRenderer("memory", renderMemoryTile);
