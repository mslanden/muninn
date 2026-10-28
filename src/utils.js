// ==================== UTILITIES ====================
export function escapeHtml(text) {
  if (text == null) return "";
  const div = document.createElement("div");
  div.textContent = String(text);
  return div.innerHTML;
}

export function field(object, camel, snake) {
  return object?.[camel] ?? object?.[snake];
}

export function projectLabel(path) {
  if (!path) return "~/";
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] || path;
}

export function normalizeWebUrl(input) {
  const raw = String(input || "").trim();
  if (!raw) return null;

  const withScheme = /^https?:\/\//i.test(raw) ? raw : `https://${raw}`;
  try {
    const u = new URL(withScheme);
    if (u.protocol !== "http:" && u.protocol !== "https:") return null;
    return u.toString();
  } catch {
    return null;
  }
}

// Block-level Markdown -> HTML. The input is escaped up front, so every tag in
// the output is produced here and the result is safe for innerHTML sinks.
// Covers what models actually emit: paragraphs, headings, lists, quotes, rules,
// GFM tables and fenced code (an unclosed fence mid-stream renders as code).
export function renderMarkdown(text) {
  if (!text) return "";
  const lines = String(text)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .split("\n");

  const out = [];
  let para = [];
  let quote = [];
  let list = null; // { tag, start, items }
  const flushPara = () => {
    if (para.length) out.push(`<p>${para.map(renderInline).join("<br>")}</p>`);
    para = [];
  };
  const flushQuote = () => {
    if (quote.length) out.push(`<blockquote>${quote.map(renderInline).join("<br>")}</blockquote>`);
    quote = [];
  };
  const flushList = () => {
    if (!list) return;
    const start = list.tag === "ol" && list.start !== 1 ? ` start="${list.start}"` : "";
    out.push(`<${list.tag}${start}>${list.items.map(i => `<li>${renderInline(i)}</li>`).join("")}</${list.tag}>`);
    list = null;
  };
  const flush = () => { flushPara(); flushQuote(); flushList(); };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    const fence = line.match(/^\s*```\s*([\w+#.-]*)\s*$/);
    if (fence) {
      flush();
      const code = [];
      for (i++; i < lines.length && !/^\s*```\s*$/.test(lines[i]); i++) code.push(lines[i]);
      out.push(codeBlock(fence[1], code.join("\n")));
      continue;
    }

    if (!line.trim()) { flush(); continue; }

    const heading = line.match(/^(#{1,6})\s+(.+?)\s*#*\s*$/);
    if (heading) {
      flush();
      const level = Math.min(heading[1].length, 3);
      out.push(`<h${level}>${renderInline(heading[2])}</h${level}>`);
      continue;
    }

    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) { flush(); out.push("<hr>"); continue; }

    const q = line.match(/^\s*&gt;\s?(.*)$/);
    if (q) { flushPara(); flushList(); quote.push(q[1]); continue; }
    flushQuote();

    if (/^\s*\|/.test(line) && i + 1 < lines.length && isTableDivider(lines[i + 1])) {
      flush();
      const head = splitRow(line);
      const rows = [];
      for (i += 2; i < lines.length && /^\s*\|/.test(lines[i]); i++) rows.push(splitRow(lines[i]));
      i--;
      out.push(`<div class="md-table"><table><thead><tr>${head.map(c => `<th>${renderInline(c)}</th>`).join("")}</tr></thead>`
        + `<tbody>${rows.map(r => `<tr>${head.map((_, j) => `<td>${renderInline(r[j] || "")}</td>`).join("")}</tr>`).join("")}</tbody></table></div>`);
      continue;
    }

    const ul = line.match(/^\s*[-*+]\s+(.+)$/);
    const ol = line.match(/^\s*(\d+)[.)]\s+(.+)$/);
    if (ul || ol) {
      flushPara();
      const tag = ul ? "ul" : "ol";
      if (list && list.tag !== tag) flushList();
      if (!list) list = { tag, start: ol ? parseInt(ol[1], 10) : 1, items: [] };
      list.items.push(ul ? ul[1] : ol[2]);
      continue;
    }

    // Indented continuation of the previous list item.
    if (list && /^\s+\S/.test(line)) {
      list.items[list.items.length - 1] += " " + line.trim();
      continue;
    }

    flushList();
    para.push(line);
  }
  flush();
  return out.join("");
}

function renderInline(s) {
  const spans = [];
  s = s.replace(/`([^`]+)`/g, (_, c) => { spans.push(c); return `\x00IC${spans.length - 1}\x00`; });
  s = s
    .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*\w])\*(?=\S)([^*]+?)\*(?!\w)/g, "$1<em>$2</em>")
    .replace(/~~(.+?)~~/g, "<del>$1</del>");
  return s.replace(/\x00IC(\d+)\x00/g, (_, i) => `<code>${spans[i]}</code>`);
}

function codeBlock(lang, code) {
  return `<div class="code-block"><div class="code-head"><span class="code-lang">${lang}</span>`
    + `<button class="code-copy" type="button">Copy</button></div><pre><code>${code}</code></pre></div>`;
}

function isTableDivider(line) {
  return /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(line);
}

function splitRow(line) {
  return line.trim().replace(/^\|/, "").replace(/\|$/, "").split("|").map(c => c.trim());
}

export function feedTimeAgo(dateStr) {
  const now = new Date();
  const then = new Date(dateStr.replace(" ", "T") + "Z");
  const sec = Math.floor((now - then) / 1000);
  if (sec < 60) return "just now";
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const d = Math.floor(hr / 24);
  if (d < 7) return `${d}d ago`;
  return then.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function formatNextRun(isoStr) {
  try {
    const d = new Date(isoStr);
    if (Number.isNaN(d.getTime())) return isoStr;
    const now = new Date();
    const diff = d - now;
    if (diff < 0) return "overdue";
    if (diff < 60000) return "< 1 min";
    if (diff < 3600000) return `${Math.round(diff / 60000)}m`;
    if (diff < 86400000) return `${Math.round(diff / 3600000)}h`;
    return `${Math.round(diff / 86400000)}d`;
  } catch { return isoStr; }
}

export function formatFileSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)} MB`;
  return `${(bytes / 1073741824).toFixed(1)} GB`;
}

export function formatModifiedAt(unixStr) {
  if (!unixStr) return "";
  const d = new Date(Number(unixStr) * 1000);
  const now = new Date();
  const diff = now - d;
  if (diff < 60000) return "just now";
  if (diff < 3600000) return `${Math.round(diff / 60000)}m ago`;
  if (diff < 86400000) return `${Math.round(diff / 3600000)}h ago`;
  if (diff < 604800000) return `${Math.round(diff / 86400000)}d ago`;
  return d.toLocaleDateString();
}

export function parseAllowedTools(input) {
  return input.split(",").map(p => p.trim()).filter(p => p.length > 0);
}

export function threadLabel(item, index = 0) {
  const preview = field(item, "preview", "preview");
  const messageCount = field(item, "messageCount", "message_count") ?? 0;
  if (!preview || !preview.trim()) {
    return messageCount > 0 ? `Chat ${index + 1}` : "New chat";
  }
  const cleaned = preview.replace(/\s+/g, " ").replace(/^Tool:\s*/i, "").trim();
  if (!cleaned) return messageCount > 0 ? `Chat ${index + 1}` : "New chat";
  if (cleaned.length <= 34) return cleaned;
  return `${cleaned.slice(0, 34).trimEnd()}...`;
}
// Treat input as a URL (browse) only when it has no spaces, contains a dot, and parses.
export function looksLikeUrl(s) {
  const v = String(s || "").trim();
  return !/\s/.test(v) && /\./.test(v) && !!normalizeWebUrl(v);
}
