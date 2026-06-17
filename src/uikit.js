// ==================== SHARED UI KIT ====================
// Cross-cutting helpers used by the tile modules: consistent error logging,
// invoke() error handling, persistent error banners with retry, empty states,
// button loading states, keyed incremental list rendering, and Esc-to-close for
// overlays. Centralising these removes the form/list/error patterns that were
// previously copy-pasted across feeds/tasks/agents/memory/logs.

import { escapeHtml } from "./utils.js";

/**
 * Log an error with a stable context label. Previously many call sites used
 * `catch {}` and swallowed failures silently, leaving the UI stuck with no
 * diagnostics. Use this everywhere a failure isn't surfaced to the user.
 */
export function logError(context, err) {
  console.error(`[${context}]`, err);
}

/**
 * Invoke a Tauri command, logging (and optionally rethrowing) on failure.
 * Returns the result, or `fallback` when the call fails and `rethrow` is false.
 */
export async function safeInvoke(
  invoke,
  cmd,
  args,
  { context, fallback = null, rethrow = false } = {},
) {
  try {
    return await invoke(cmd, args);
  } catch (err) {
    logError(context || cmd, err);
    if (rethrow) throw err;
    return fallback;
  }
}

/**
 * Render a persistent error banner into `container` (prepended). Unlike the old
 * 2.5s auto-dismissing flashes, this stays until dismissed and can offer a Retry
 * action. Returns the banner element (call `.remove()` to clear it manually).
 */
export function showErrorBanner(
  container,
  message,
  { onRetry = null, onDismiss = null, type = "error" } = {},
) {
  if (!container) return null;
  // Replace any existing banner so they don't stack.
  container.querySelector(":scope > .uikit-banner")?.remove();

  const banner = document.createElement("div");
  banner.className = `uikit-banner uikit-banner-${type}`;
  banner.setAttribute("role", "alert");
  banner.innerHTML = `
    <span class="uikit-banner-msg">${escapeHtml(message)}</span>
    <span class="uikit-banner-actions">
      ${onRetry ? `<button type="button" class="uikit-banner-retry">Retry</button>` : ""}
      <button type="button" class="uikit-banner-dismiss" aria-label="Dismiss">&times;</button>
    </span>
  `;
  if (onRetry) {
    banner.querySelector(".uikit-banner-retry").addEventListener("click", () => {
      banner.remove();
      onRetry();
    });
  }
  banner.querySelector(".uikit-banner-dismiss").addEventListener("click", () => {
    banner.remove();
    if (onDismiss) onDismiss();
  });
  container.prepend(banner);
  return banner;
}

/**
 * Build a consistent empty-state element with an optional call-to-action button.
 */
export function emptyState({
  icon = "◈",
  text = "Nothing here yet",
  actionLabel = null,
  onAction = null,
} = {}) {
  const el = document.createElement("div");
  el.className = "uikit-empty";
  el.innerHTML = `
    <div class="uikit-empty-icon" aria-hidden="true">${escapeHtml(icon)}</div>
    <div class="uikit-empty-text">${escapeHtml(text)}</div>
    ${actionLabel ? `<button type="button" class="uikit-empty-action">${escapeHtml(actionLabel)}</button>` : ""}
  `;
  if (actionLabel && onAction) {
    el.querySelector(".uikit-empty-action").addEventListener("click", onAction);
  }
  return el;
}

/**
 * Put a button into a loading state (disabled + spinner + label), returning a
 * function that restores its original content. Guards against double-submits.
 */
export function setButtonLoading(btn, loadingLabel = "Working…") {
  if (!btn) return () => {};
  const original = btn.innerHTML;
  const wasDisabled = btn.disabled;
  btn.disabled = true;
  btn.setAttribute("aria-busy", "true");
  btn.innerHTML = `<span class="uikit-spinner" aria-hidden="true"></span>${escapeHtml(loadingLabel)}`;
  return () => {
    btn.innerHTML = original;
    btn.disabled = wasDisabled;
    btn.removeAttribute("aria-busy");
  };
}

/**
 * Incrementally reconcile a keyed list into `container`, preserving existing DOM
 * nodes (and therefore scroll position and focus) instead of wiping and
 * rebuilding. `createFn(item)` builds a fresh node for a new key; optional
 * `updateFn(node, item)` refreshes an existing node in place. Nodes whose keys
 * disappear are removed. Order follows `items`.
 */
export function reconcileKeyedList(container, items, keyFn, createFn, updateFn = null) {
  if (!container) return;
  const existing = new Map();
  for (const child of Array.from(container.children)) {
    if (child.dataset && child.dataset.uikitKey != null) {
      existing.set(child.dataset.uikitKey, child);
    }
  }

  const seen = new Set();
  let prev = null;
  for (const item of items) {
    const key = String(keyFn(item));
    seen.add(key);
    let node = existing.get(key);
    if (node) {
      if (updateFn) updateFn(node, item);
    } else {
      node = createFn(item);
      node.dataset.uikitKey = key;
    }
    // Place node directly after prev to enforce order without needless moves.
    const target = prev ? prev.nextSibling : container.firstChild;
    if (node !== target) container.insertBefore(node, target);
    prev = node;
  }

  // Drop nodes whose keys are gone.
  for (const [key, node] of existing) {
    if (!seen.has(key)) node.remove();
  }
}

/**
 * Wire Escape-to-close on an overlay element. Listens on document so it works
 * regardless of focus, and returns a cleanup function that removes the listener.
 */
export function attachEscClose(onClose) {
  const handler = (e) => {
    if (e.key === "Escape") onClose(e);
  };
  document.addEventListener("keydown", handler);
  return () => document.removeEventListener("keydown", handler);
}
