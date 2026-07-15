// Quick-ask overlay: a Spotlight-style window summoned by the global shortcut
// (registered Rust-side in lib.rs). Boots from index.html?quickask=1 with its
// own minimal DOM instead of the tile shell, and talks to the same `chat`
// command using a dedicated persistent session.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { renderMarkdown } from "./utils.js";

const SESSION_ID = "quickask";

export function isQuickAsk() {
  return new URLSearchParams(window.location.search).get("quickask") === "1";
}

export async function bootQuickAsk() {
  const appWindow = getCurrentWebviewWindow();

  document.body.innerHTML = `
    <div class="quickask">
      <div class="qa-inputrow" data-tauri-drag-region>
        <span class="qa-logo">M</span>
        <input class="qa-input" type="text" placeholder="Ask Muninn…" autocomplete="off" spellcheck="false" />
      </div>
      <div class="qa-answer" hidden></div>
      <div class="qa-footer">
        <span class="qa-status"></span>
        <span class="qa-hint">↩ ask · esc hide</span>
      </div>
    </div>
  `;

  const input = document.body.querySelector(".qa-input");
  const answer = document.body.querySelector(".qa-answer");
  const status = document.body.querySelector(".qa-status");

  let settings = {};
  try { settings = await invoke("get_app_settings"); } catch {}

  let streaming = false;
  let text = "";

  await listen("chat-token", (event) => {
    const { session_id, token } = event.payload;
    if (session_id !== SESSION_ID) return;
    text += token;
    answer.hidden = false;
    answer.innerHTML = renderMarkdown(text);
    answer.scrollTop = answer.scrollHeight;
  });

  await listen("tool-executed", (event) => {
    const { session_id, tool, status: toolStatus } = event.payload;
    if (session_id !== SESSION_ID) return;
    status.textContent = toolStatus === "running" ? `running ${tool}…` : "";
  });

  await listen("chat-done", (event) => {
    if (event.payload.session_id !== SESSION_ID) return;
    status.textContent = "";
  });

  await listen("quickask-shown", () => {
    input.focus();
    input.select();
  });

  input.addEventListener("keydown", async (e) => {
    if (e.key === "Escape") {
      appWindow.hide();
      return;
    }
    if (e.key !== "Enter" || streaming) return;
    const message = input.value.trim();
    if (!message) return;

    streaming = true;
    text = "";
    answer.hidden = false;
    answer.innerHTML = "";
    status.textContent = "thinking…";
    input.disabled = true;

    try {
      const req = {
        sessionId: SESSION_ID,
        message,
        provider: "minimax",
        model: settings["llm/model"] || "MiniMax-M3",
      };
      await invoke("chat", { req });
      if (text) answer.innerHTML = renderMarkdown(text);
    } catch (error) {
      answer.innerHTML = renderMarkdown(`**Error:** ${String(error)}`);
    } finally {
      streaming = false;
      status.textContent = "";
      input.disabled = false;
      input.value = "";
      input.focus();
    }
  });

  // Dismiss like Spotlight: clicking elsewhere hides the overlay. Skip while a
  // reply is streaming so a stray click doesn't bury an in-flight answer.
  window.addEventListener("blur", () => {
    if (!streaming) appWindow.hide();
  });

  input.focus();
}
