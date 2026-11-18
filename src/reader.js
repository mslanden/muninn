import { WebviewWindow } from "@tauri-apps/api/webviewWindow";

// Open a web page (e.g. a search result from the Web tile) in its own Muninn window.

export async function openReaderWindow(url, title) {
  if (!url) return;
  let hash = 0;
  for (let i = 0; i < url.length; i++) hash = ((hash << 5) - hash + url.charCodeAt(i)) | 0;
  const label = "muninn-reader-" + Math.abs(hash).toString(36);
  try {
    const existing = await WebviewWindow.getByLabel(label);
    if (existing) {
      try { await existing.setFocus(); } catch (_) {}
      return;
    }
  } catch (_) {}
  const w = new WebviewWindow(label, {
    url,
    width: 1100,
    height: 800,
    resizable: true,
    title: title ? `${title} — Muninn` : "Muninn Reader",
  });
  w.once("tauri://error", (e) => console.error("reader window error", e));
}
