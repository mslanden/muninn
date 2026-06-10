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

export function renderMarkdown(text) {
  if (!text) return "";
  let html = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  // Extract code blocks before other processing
  const codeBlocks = [];
  html = html.replace(/```(\w*)\n([\s\S]*?)```/g, (_, lang, code) => {
    const idx = codeBlocks.length;
    codeBlocks.push(`<pre><code>${code}</code></pre>`);
    return `\x00CB${idx}\x00`;
  });
  html = html.replace(/`([^`]+)`/g, '<code>$1</code>');
  html = html
    .replace(/^### (.+)$/gm, '<h3>$1</h3>')
    .replace(/^## (.+)$/gm, '<h2>$1</h2>')
    .replace(/^# (.+)$/gm, '<h1>$1</h1>');
  // Wrap consecutive <li> runs into <ul> (non-greedy, per group)
  html = html
    .replace(/^\* (.+)$/gm, '<li>$1</li>');
  html = html.replace(/((?:<li>.*<\/li>\n?)+)/g, '<ul>$1</ul>');
  html = html
    .replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>')
    .replace(/\*(.+?)\*/g, '<em>$1</em>');
  html = html.replace(/\n/g, '<br>');
  // Restore code blocks (after <br> replacement so code stays clean)
  html = html.replace(/\x00CB(\d+)\x00/g, (_, idx) => codeBlocks[parseInt(idx)]);
  return html;
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
