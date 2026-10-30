// Connections window: everything Muninn can reach — model providers, apps on this
// Mac, keyed services, MCP servers and built-in tools — as cards with a status and
// one switch. A connection's switch flips all of its tools (commands/connections.rs),
// so this replaces the old flat Tools list and the API Keys / MCP settings pages.
// Keys are write-only here: the backend reports set/unset, never values.

import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { ui } from "./state.js";
import { escapeHtml } from "./utils.js";
import { icon } from "./icons.js";
import { updateModelChips } from "./chat.js";

const ICONS = {
  mail: "mail", calendar: "calendar", folder: "folder", screenshot: "monitor", search: "search",
  photo: "image", "map-pin": "map", video: "video", sparkles: "sparkles", plug: "plug", tool: "wrench",
};

const SECTIONS = [
  { kind: "provider", title: "Model providers", icon: "sparkles", hint: "Pick the model Muninn chats with. Keys stay on this Mac." },
  { kind: "mac", title: "On this Mac", icon: "monitor" },
  { kind: "service", title: "Services", icon: "globe" },
  { kind: "mcp", title: "MCP servers", icon: "plug" },
  { kind: "builtin", title: "Built in", icon: "sparkles" },
];

const STATUS = {
  in_use: ["In use", "accent"],
  ready: ["Ready", "ok"],
  needs_key: ["Set up", "muted"],
  off: ["Off", "muted"],
  disabled: ["Off", "muted"],
  error: ["Needs attention", "warn"],
};

let overlay = null;
let expandedId = null;
let showAllProviders = false;

const navBtn = () => document.getElementById("nav-connections");

export function isConnectionsOpen() {
  return !!overlay;
}

export function closeConnections() {
  if (!overlay) return;
  overlay.remove();
  overlay = null;
  document.removeEventListener("keydown", onEsc);
  navBtn()?.classList.remove("open");
}

function onEsc(e) {
  if (e.key === "Escape") closeConnections();
}

export function openConnections() {
  if (overlay) return;
  overlay = document.createElement("div");
  overlay.className = "settings-overlay";
  overlay.innerHTML = `
    <div class="settings-modal conn-modal" role="dialog" aria-modal="true" aria-label="Connections">
      <div class="settings-modal-head">
        <span class="settings-modal-title">Connections</span>
        <span class="conn-summary" data-conn-summary></span>
        <button class="settings-modal-close" aria-label="Close connections">${icon("x", 14)}</button>
      </div>
      <div class="conn-body" data-conn-body><div class="conn-loading">Loading connections…</div></div>
    </div>`;
  document.body.appendChild(overlay);
  overlay.addEventListener("mousedown", (e) => { if (e.target === overlay) closeConnections(); });
  overlay.querySelector(".settings-modal-close").addEventListener("click", closeConnections);
  document.addEventListener("keydown", onEsc);
  navBtn()?.classList.add("open");
  refreshConnections();
}

/** Re-render from the backend (also called on `connections-changed`). */
export async function refreshConnections() {
  if (!overlay) return;
  const body = overlay.querySelector("[data-conn-body]");
  let conns;
  try {
    conns = await invoke("get_connections");
  } catch (e) {
    body.innerHTML = `<div class="conn-loading">Couldn't load connections: ${escapeHtml(String(e))}</div>`;
    return;
  }
  const scroll = body.scrollTop;
  render(body, conns);
  body.scrollTop = scroll;
  const active = conns.filter(c => c.kind !== "provider" && c.enabled && c.status === "ready").length;
  const tools = conns.reduce((n, c) => n + c.tools.filter(t => t.enabled).length, 0);
  overlay.querySelector("[data-conn-summary]").textContent = `${active} connected · ${tools} tools on`;
}

function render(body, conns) {
  body.innerHTML = `<p class="conn-intro">What Muninn can reach. Switch a connection off and its tools disappear from chats.</p>`;
  for (const sec of SECTIONS) {
    let items = conns.filter(c => c.kind === sec.kind);
    if (sec.kind === "provider" && !showAllProviders) {
      // Show the providers you use or have keys for; the rest sit behind "Show all".
      const hidden = items.filter(c => c.status === "needs_key" && c.id !== expandedId);
      items = items.filter(c => !hidden.includes(c));
      sec.more = hidden.length;
    }
    if (!items.length && sec.kind !== "mcp") continue;
    const section = document.createElement("section");
    section.className = "conn-section";
    section.innerHTML = `
      <div class="conn-section-head">${icon(sec.icon, 13)}<span>${sec.title}</span><span class="tile-spacer"></span></div>
      ${sec.hint ? `<p class="conn-hint">${sec.hint}</p>` : ""}
      <div class="conn-grid"></div>`;
    const head = section.querySelector(".conn-section-head");
    const grid = section.querySelector(".conn-grid");
    for (const c of items) grid.appendChild(card(c));
    if (sec.kind === "provider" && (sec.more || showAllProviders)) {
      const more = button(showAllProviders ? "Show fewer" : `Show all providers (${sec.more} more)`, "conn-link");
      more.addEventListener("click", () => { showAllProviders = !showAllProviders; refreshConnections(); });
      head.appendChild(more);
    }
    if (sec.kind === "mcp") {
      const add = button("Add server", "form-btn conn-small-btn");
      add.addEventListener("click", () => toggleAddServer(section));
      head.appendChild(add);
      if (!items.length) {
        grid.insertAdjacentHTML("beforeend", `<p class="conn-hint">No MCP servers yet. Add one to give Muninn tools from GitHub, Notion, your CRM and more.</p>`);
      }
    }
    body.appendChild(section);
  }
}

function button(label, cls) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = cls;
  b.textContent = label;
  return b;
}

function card(c) {
  const el = document.createElement("div");
  const expanded = expandedId === c.id;
  el.className = `conn-card${expanded ? " expanded" : ""}`;
  const [label, tone] = STATUS[c.status] || ["", "muted"];
  const hasSwitch = c.kind !== "provider" && c.status !== "needs_key";
  el.innerHTML = `
    <div class="conn-card-row" role="button" tabindex="0" aria-expanded="${expanded}">
      <span class="conn-icon">${icon(ICONS[c.icon] || "sparkles", 16)}</span>
      <span class="conn-text">
        <span class="conn-name">${escapeHtml(c.name)}</span>
        <span class="conn-detail">${escapeHtml(c.detail || c.description)}</span>
      </span>
      ${hasSwitch
        ? `<button type="button" class="conn-switch${c.enabled ? " on" : ""}" role="switch" aria-checked="${c.enabled}" aria-label="${escapeHtml(c.name)}"></button>`
        : `<span class="conn-pill ${tone}">${label}</span>`}
    </div>`;
  const row = el.querySelector(".conn-card-row");
  const toggleExpand = () => { expandedId = expanded ? null : c.id; refreshConnections(); };
  row.addEventListener("click", (e) => { if (!e.target.closest(".conn-switch")) toggleExpand(); });
  row.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); toggleExpand(); } });
  el.querySelector(".conn-switch")?.addEventListener("click", async (e) => {
    e.stopPropagation();
    try {
      await invoke("set_connection_enabled", { id: c.id, enabled: !c.enabled });
    } catch (err) {
      console.error("set_connection_enabled failed:", err);
    }
    refreshConnections();
  });
  if (expanded) el.appendChild(details(c));
  return el;
}

function details(c) {
  const d = document.createElement("div");
  d.className = "conn-details";
  d.innerHTML = `<p class="conn-desc">${escapeHtml(c.description)}</p><div class="conn-status-line" data-conn-msg></div>`;
  const msg = (text, tone = "") => {
    const el = d.querySelector("[data-conn-msg]");
    el.textContent = text;
    el.className = `conn-status-line ${tone}`;
  };

  if (c.keySetting) {
    const isUrl = c.keySetting.endsWith("_base_url");
    const row = document.createElement("div");
    row.className = "form-row conn-key-row";
    row.innerHTML = `
      <input type="${isUrl ? "text" : "password"}" spellcheck="false" autocomplete="off"
        placeholder="${isUrl ? "http://localhost:11434/v1" : c.keySet ? "Key saved — paste a new one to replace it" : "Paste API key"}" />
      <button type="button" class="form-btn">Save</button>`;
    const input = row.querySelector("input");
    row.querySelector("button").addEventListener("click", async () => {
      const value = input.value.trim();
      if (!value) { msg(isUrl ? "Enter the server's base URL" : "Paste a key first", "warn"); input.focus(); return; }
      try {
        await invoke("set_app_settings", { req: { settings: { [c.keySetting]: value } } });
        await reloadSettings();
        msg("Saved", "ok");
        setTimeout(refreshConnections, 600);
      } catch (e) { msg(String(e), "warn"); }
    });
    d.appendChild(row);
  }

  if (c.kind === "provider") {
    const provider = c.id.slice("provider:".length);
    const s = ui.loadedSettings || {};
    const current = c.status === "in_use" ? (s["llm/model"] || "") : (s[`llm/model/${provider}`] || "");
    const row = document.createElement("div");
    row.className = "form-row conn-key-row";
    row.innerHTML = `
      <input type="text" spellcheck="false" placeholder="Model id" value="${escapeHtml(current)}" />
      <button type="button" class="form-btn">Test</button>
      ${c.status === "in_use" ? "" : `<button type="button" class="form-btn conn-primary">Use this</button>`}`;
    const model = row.querySelector("input");
    const [testBtn, useBtn] = row.querySelectorAll("button");
    testBtn.addEventListener("click", async () => {
      if (!model.value.trim()) { msg("Enter a model id to test", "warn"); return; }
      msg("Testing…");
      try { msg(await invoke("test_llm_connection", { provider, model: model.value.trim() }), "ok"); }
      catch (e) { msg(String(e), "warn"); }
    });
    useBtn?.addEventListener("click", async () => {
      const m = model.value.trim();
      if (!m) { msg("Enter a model id first", "warn"); model.focus(); return; }
      if (!c.keySet) { msg("Save a key for this provider first", "warn"); return; }
      try {
        await invoke("set_app_settings", { req: { settings: { "llm/provider": provider, "llm/model": m, [`llm/model/${provider}`]: m } } });
        await reloadSettings();
        refreshConnections();
      } catch (e) { msg(String(e), "warn"); }
    });
    d.appendChild(row);
  }

  if (c.id === "mail" || c.id === "calendar") {
    const check = button("Check access", "form-btn conn-small-btn");
    check.addEventListener("click", async () => {
      msg(`Asking ${c.name}… (macOS may ask you to allow Muninn)`);
      try {
        const r = await invoke("check_app_access", { req: { app: c.id } });
        msg(r.status === "ok" ? `Connected · ${r.detail}` : r.detail, r.status === "ok" ? "ok" : "warn");
      } catch (e) { msg(String(e), "warn"); }
    });
    d.appendChild(check);
  }

  if (c.kind === "mcp") {
    const name = c.id.slice("mcp:".length);
    const remove = button("Remove server", "form-btn conn-small-btn");
    remove.addEventListener("click", async () => {
      if (!(await ask(`Remove the MCP server "${name}"?`, { title: "Remove server", kind: "warning" }))) return;
      try {
        const servers = await readServers();
        await writeServers(servers.filter(s => s.name !== name));
        expandedId = null;
        refreshConnections();
      } catch (e) { msg(String(e), "warn"); }
    });
    d.appendChild(remove);
  }

  if (c.tools.length) {
    const list = document.createElement("div");
    list.className = "conn-tools";
    for (const t of c.tools) {
      const row = document.createElement("label");
      row.className = "conn-tool";
      row.title = t.description || "";
      row.innerHTML = `<span class="conn-tool-name mono">${escapeHtml(t.id)}</span><span class="conn-tool-desc">${escapeHtml(t.description || "")}</span>
        <input type="checkbox" ${t.enabled ? "checked" : ""} aria-label="${escapeHtml(t.name)}" />`;
      row.querySelector("input").addEventListener("change", async (e) => {
        try { await invoke("set_tool_enabled", { req: { toolId: t.id, enabled: e.target.checked } }); }
        catch (err) { console.error("set_tool_enabled failed:", err); }
        refreshConnections();
      });
      list.appendChild(row);
    }
    d.appendChild(list);
  }
  return d;
}

async function reloadSettings() {
  try { ui.loadedSettings = await invoke("get_app_settings"); } catch { /* keep cache */ }
  try { updateModelChips(); } catch { /* no chat tiles */ }
}

async function readServers() {
  const s = await invoke("get_app_settings");
  const raw = (s["mcp/servers"] || "").trim();
  if (!raw) return [];
  const parsed = JSON.parse(raw);
  return Array.isArray(parsed) ? parsed : [];
}

async function writeServers(servers) {
  await invoke("set_app_settings", { req: { settings: { "mcp/servers": JSON.stringify(servers, null, 2) } } });
  try { await invoke("reload_mcp_servers"); } catch { /* status shows the error */ }
  await reloadSettings();
}

function toggleAddServer(section) {
  const existing = section.querySelector(".conn-add-server");
  if (existing) { existing.remove(); return; }
  const form = document.createElement("div");
  form.className = "conn-add-server";
  form.innerHTML = `
    <input type="text" placeholder="Name (e.g. github)" spellcheck="false" data-f="name" />
    <input type="text" placeholder="Command (e.g. npx)" spellcheck="false" data-f="command" />
    <input type="text" placeholder="Arguments (e.g. -y @modelcontextprotocol/server-github)" spellcheck="false" data-f="args" />
    <textarea rows="2" placeholder="Environment, one per line (e.g. GITHUB_TOKEN=…)" spellcheck="false" data-f="env"></textarea>
    <div class="form-row"><span class="conn-status-line" data-f="msg"></span><span class="tile-spacer"></span><button type="button" class="form-btn">Add server</button></div>`;
  const f = (k) => form.querySelector(`[data-f="${k}"]`);
  form.querySelector("button").addEventListener("click", async () => {
    const name = f("name").value.trim();
    const command = f("command").value.trim();
    if (!name || !command) { f("msg").textContent = "Name and command are required"; f("msg").className = "conn-status-line warn"; return; }
    const env = {};
    for (const line of f("env").value.split("\n")) {
      const i = line.indexOf("=");
      if (i > 0) env[line.slice(0, i).trim()] = line.slice(i + 1).trim();
    }
    const args = f("args").value.trim() ? f("args").value.trim().split(/\s+/) : [];
    try {
      const servers = await readServers();
      if (servers.some(s => s.name === name)) { f("msg").textContent = "A server with that name already exists"; f("msg").className = "conn-status-line warn"; return; }
      servers.push({ name, command, args, env });
      f("msg").textContent = "Starting server…";
      await writeServers(servers);
      expandedId = `mcp:${name}`;
      refreshConnections();
    } catch (e) {
      f("msg").textContent = String(e);
      f("msg").className = "conn-status-line warn";
    }
  });
  section.querySelector(".conn-grid").before(form);
  f("name").focus();
}
