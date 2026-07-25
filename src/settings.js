import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { enable as enableAutostart, disable as disableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
import { workspace, sidebarWorkspace, sidebarModel, ui } from "./state.js";
import { applyTheme, THEMES } from "./theme.js";
import { field, projectLabel, escapeHtml } from "./utils.js";
import { icon } from "./icons.js";
import qrcode from "qrcode-generator";
import { renderLogsTile } from "./logs.js";
import { renderToolsTile } from "./toolspanel.js";

// ========== SETTINGS TILE ==========
export function renderSettingsTile(tileObj, body) {
  body.innerHTML = `
    <div class="settings-layout">
      <div class="settings-tabs" data-s-tabs="${tileObj.id}">
        <button type="button" class="settings-tab active" data-s-tab="general">General</button>
        <button type="button" class="settings-tab" data-s-tab="activity">Activity</button>
        <button type="button" class="settings-tab" data-s-tab="tools">Tools</button>
      </div>
      <div class="settings-pages">
        <div class="settings-page active" data-s-page="general">
    <div class="settings-section">
      <div class="settings-group">
        <h3>Profile</h3>
        <input type="text" placeholder="Your name" data-s-name="${tileObj.id}" style="margin-bottom:6px;" />
        <textarea rows="2" placeholder="About you" data-s-about="${tileObj.id}"></textarea>
        <input type="text" placeholder="Your location (e.g. Mountain Center, CA)" data-s-location="${tileObj.id}" style="margin-top:6px;" />
      </div>
      <div class="settings-group">
        <h3>Model</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <select data-s-provider="${tileObj.id}" style="flex:1; min-width:0;">
            <option value="minimax">MiniMax (Anthropic-compatible)</option>
            <option value="openai">OpenAI</option>
            <option value="anthropic">Anthropic</option>
            <option value="groq">Groq</option>
            <option value="deepseek">DeepSeek</option>
            <option value="xai">xAI (Grok)</option>
            <option value="google">Google (Gemini)</option>
            <option value="mistral">Mistral</option>
            <option value="together">Together</option>
            <option value="fireworks">Fireworks</option>
            <option value="openrouter">OpenRouter</option>
            <option value="cohere">Cohere</option>
            <option value="meta">Meta (Muse Spark)</option>
            <option value="custom">Custom (OpenAI-compatible)</option>
          </select>
          <input type="text" placeholder="Model name (e.g. MiniMax-M3)" list="s-model-list-${tileObj.id}" data-s-model="${tileObj.id}" style="flex:1.3; min-width:120px;" />
          <button class="form-btn" data-s-fetch-models="${tileObj.id}">Fetch models</button>
        </div>
        <datalist id="s-model-list-${tileObj.id}" data-s-model-list="${tileObj.id}"></datalist>
        <input type="text" placeholder="http://localhost:11434/v1" data-s-custombase="${tileObj.id}" style="margin-bottom:4px; display:none;" />
        <input type="password" placeholder="API key for the selected provider" data-s-provkey="${tileObj.id}" style="margin-bottom:4px;" />
        <span class="muted" data-s-prov-hint="${tileObj.id}" style="font-size:10px;"></span>
        <div class="form-row" style="margin-top:6px; align-items:center;">
          <button class="form-btn" data-s-test-llm="${tileObj.id}">Test</button>
          <span class="muted" data-s-model-status="${tileObj.id}" style="font-size:10px;"></span>
        </div>
        <div class="form-row" style="margin-top:8px;">
          <input type="number" min="1" step="1024" placeholder="Max response tokens (e.g. 40960)" data-s-maxtokens="${tileObj.id}" style="flex:1;" />
        </div>
        <span class="muted" style="font-size:10px;">Output-token ceiling per response. Too low truncates long file writes mid-call. Blank = 40960.</span>
        <div class="form-row" style="margin-top:8px;">
          <select data-s-fbprovider="${tileObj.id}" style="flex:1; min-width:0;">
            <option value="">No fallback provider</option>
            <option value="minimax">MiniMax (Anthropic-compatible)</option>
            <option value="openai">OpenAI</option>
            <option value="anthropic">Anthropic</option>
            <option value="groq">Groq</option>
            <option value="deepseek">DeepSeek</option>
            <option value="xai">xAI (Grok)</option>
            <option value="google">Google (Gemini)</option>
            <option value="mistral">Mistral</option>
            <option value="together">Together</option>
            <option value="fireworks">Fireworks</option>
            <option value="openrouter">OpenRouter</option>
            <option value="cohere">Cohere</option>
            <option value="meta">Meta (Muse Spark)</option>
            <option value="custom">Custom (OpenAI-compatible)</option>
          </select>
          <input type="text" placeholder="Fallback model" data-s-fbmodel="${tileObj.id}" style="flex:1.3; min-width:120px;" />
        </div>
        <span class="muted" style="font-size:10px;">Retried automatically when the primary model's first request fails. Blank model = the fallback provider's remembered model.</span>
        <label class="checkbox-inline" style="margin-top:6px;">
          <input type="checkbox" data-s-keyfallback="${tileObj.id}" /> Allow the OpenAI key as a stand-in for providers without their own key
        </label>
      </div>
      <div class="settings-group">
        <h3>API Keys</h3>
        <span class="muted" style="font-size:10px;">LLM provider keys are set in the Model section above.</span>
        <input type="password" placeholder="Groq API key (voice transcription)" data-s-groq="${tileObj.id}" style="margin-bottom:6px; margin-top:6px;" />
        <input type="password" placeholder="Web search API key" data-s-search="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="Together AI key (images)" data-s-together="${tileObj.id}" style="margin-bottom:6px;" />
        <input type="password" placeholder="Mapbox API key (maps)" data-s-mapbox="${tileObj.id}" style="margin-bottom:6px;" />
      </div>
      <div class="settings-group">
        <h3>Voice</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <select data-s-ttsprov="${tileObj.id}" style="flex:1; min-width:0;">
            <option value="minimax">MiniMax</option>
            <option value="fishspeech">FishSpeech</option>
          </select>
          <input type="text" placeholder="speech-2.8-hd" data-s-ttsmodel="${tileObj.id}" style="flex:2; min-width:0;" />
        </div>
        <div class="form-row" style="margin-bottom:6px; align-items:center;">
          <input type="text" placeholder="Voice ID (e.g. English_Graceful_Lady)" data-s-voiceid="${tileObj.id}" style="flex:1;" />
          <button class="form-btn" data-s-tts-preview="${tileObj.id}" title="Preview voice">▶</button>
        </div>
        <span class="muted" data-s-tts-status="${tileObj.id}" style="font-size:10px;"></span>
        <div class="form-row" style="margin-top:6px;">
          <select data-s-sttprov="${tileObj.id}" style="flex:1;">
            <option value="auto">Automatic (Groq → OpenAI)</option>
            <option value="groq">Groq</option>
            <option value="openai">OpenAI</option>
          </select>
        </div>
        <span class="muted" style="font-size:10px;">Speech-to-text transcription provider for voice input.</span>
      </div>
      <div class="settings-group">
        <h3>Image Generation</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <select data-s-imgprov="${tileObj.id}">
            <option value="">None</option>
            <option value="minimax">MiniMax</option>
            <option value="together">Together</option>
            <option value="openai">OpenAI</option>
          </select>
          <select data-s-imgmodel="${tileObj.id}" style="flex:1;"></select>
          <button class="form-btn" data-s-fetch-img-models="${tileObj.id}">Fetch</button>
        </div>
        <span class="muted" data-s-img-status="${tileObj.id}" style="font-size:10px;"></span>
      </div>
      <div class="settings-group">
        <h3>Video</h3>
        <label class="checkbox-inline">
          <input type="checkbox" data-s-localtranscribe="${tileObj.id}" /> On-device transcription (Apple Speech)
        </label>
        <span class="muted" style="font-size:10px;">Studio video analysis transcribes on your Mac — no API key, audio never leaves the device. Falls back to cloud whisper (Groq/OpenAI) if it fails. First use downloads a speech model.</span>
        <label class="checkbox-inline" style="margin-top:6px;">
          <input type="checkbox" data-s-hwencode="${tileObj.id}" /> Hardware video encoding (VideoToolbox)
        </label>
        <span class="muted" style="font-size:10px;">Uses your Mac's hardware encoder for faster renders when available. Falls back to software encoding automatically if it's unsupported or fails.</span>
      </div>
      <div class="settings-group">
        <h3>Default directory</h3>
        <p class="settings-hint">The folder a chat operates in when no project folder is picked. Use the ${icon("folder", 12)} button in a chat to scope it to a specific folder.</p>
        <div class="form-row" style="margin-bottom:6px;">
          <input type="text" placeholder="Default directory path" data-s-workspace="${tileObj.id}" />
          <button class="form-btn" data-s-setws="${tileObj.id}">Set</button>
        </div>
      </div>
      <div class="settings-group">
        <h3>Appearance</h3>
        <div class="form-row" style="margin-bottom:6px;">
          <select data-s-theme="${tileObj.id}">
            ${THEMES.map((t) => `<option value="${t.id}">${t.label}</option>`).join("")}
          </select>
        </div>
      </div>
      <div class="settings-group">
        <h3>System</h3>
        <label class="checkbox-inline">
          <input type="checkbox" data-s-autostart="${tileObj.id}" /> Launch at login
        </label>
        <label class="checkbox-inline">
          <input type="checkbox" data-s-notify="${tileObj.id}" /> Desktop notifications
        </label>
        <div class="form-row" style="margin-top:6px;">
          <input type="text" placeholder="Quick-ask shortcut (e.g. cmd+shift+space)" data-s-qashortcut="${tileObj.id}" style="flex:1;" />
        </div>
        <span class="muted" style="font-size:10px;">Shortcut changes take effect after restarting Muninn.</span>
      </div>
      <div class="settings-group">
        <h3>Safety</h3>
        <label class="checkbox-inline">
          <input type="checkbox" data-s-autoverify="${tileObj.id}" /> Auto-verify dangerous commands (LLM reviews before asking you)
        </label>
        <div class="form-row" style="margin-top:6px;">
          <input type="number" min="15" max="3600" step="1" placeholder="120" data-s-approvaltimeout="${tileObj.id}" style="flex:1;" />
        </div>
        <span class="muted" style="font-size:10px;">Approval timeout (seconds) before a verified command is deferred for later. 15–3600.</span>
      </div>
      <div class="settings-group">
        <h3>Remote access (phone)</h3>
        <p class="settings-hint">Run a local server so you can chat with Muninn from your phone on the same Wi-Fi.</p>
        <div class="form-row" style="margin-bottom:6px; align-items:center;">
          <button class="form-btn" data-s-mobile-toggle="${tileObj.id}">Enable</button>
          <span class="muted" data-s-mobile-state="${tileObj.id}" style="font-size:10px;"></span>
        </div>
        <div class="form-row" style="margin-bottom:4px; align-items:center;">
          <input type="number" min="1" max="65535" step="1" placeholder="8737" data-s-mobile-port="${tileObj.id}" style="flex:1;" />
          <span class="muted" style="font-size:10px; white-space:nowrap;">applies on next start</span>
        </div>
        <span class="muted" data-s-mobile-error="${tileObj.id}" style="font-size:10px; color:var(--red);"></span>
        <div class="mobile-pairing" data-s-mobile-pairing="${tileObj.id}" style="display:none; margin-top:10px;">
          <div class="mobile-qr" data-s-mobile-qr="${tileObj.id}"></div>
          <div class="mobile-urls" data-s-mobile-urls="${tileObj.id}"></div>
          <div class="form-row" style="margin-top:8px;">
            <button class="form-btn danger" data-s-mobile-regen="${tileObj.id}">Regenerate token</button>
          </div>
          <p class="settings-hint" style="margin-top:8px;">Works on your Wi-Fi. For access from anywhere, install Tailscale on both devices — the URL then works over it too.</p>
        </div>
        <p class="settings-hint" style="margin-top:8px; color:var(--red);">Anyone with this URL can chat and run commands as Muninn. Keep it private.</p>
      </div>
      <div class="settings-group">
        <h3>MCP Servers</h3>
        <textarea rows="4" spellcheck="false" placeholder='[{"name": "github", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "..."}}]' data-s-mcp="${tileObj.id}"></textarea>
        <div class="form-row" style="margin-top:6px;">
          <button class="form-btn" data-s-mcp-reload="${tileObj.id}">Reload Servers</button>
        </div>
        <div class="muted" data-s-mcp-status="${tileObj.id}" style="font-size:10px; white-space:pre-line;"></div>
      </div>
      <div class="form-row" style="margin-top:12px;">
        <button class="form-btn" data-s-save="${tileObj.id}">Save Settings</button>
        <span class="muted" data-s-status="${tileObj.id}"></span>
      </div>
    </div>
        </div>
        <div class="settings-page" data-s-page="activity">
          <div class="settings-section">
            <div class="settings-group">
              <h3>Activity log</h3>
              <p class="settings-hint">Tool runs, messages, and background events from this session and others.</p>
              <div class="settings-embed" data-s-activity-host="${tileObj.id}"></div>
            </div>
          </div>
        </div>
        <div class="settings-page" data-s-page="tools">
          <div class="settings-section">
            <div class="settings-group">
              <h3>Tool access</h3>
              <p class="settings-hint">Enable or disable tools the assistant can call during chat and agent runs.</p>
              <div class="settings-embed" data-s-tools-host="${tileObj.id}"></div>
            </div>
          </div>
        </div>
      </div>
    </div>
  `;

  // Tab switching — Activity / Tools hosts are filled lazily on first visit.
  const tabsEl = body.querySelector(`[data-s-tabs="${tileObj.id}"]`);
  let activityMounted = false;
  let toolsMounted = false;
  tabsEl.querySelectorAll(".settings-tab").forEach((tab) => {
    tab.addEventListener("click", () => {
      const id = tab.dataset.sTab;
      tabsEl.querySelectorAll(".settings-tab").forEach((t) => t.classList.toggle("active", t === tab));
      body.querySelectorAll(".settings-page").forEach((p) => {
        p.classList.toggle("active", p.dataset.sPage === id);
      });
      if (id === "activity" && !activityMounted) {
        const host = body.querySelector(`[data-s-activity-host="${tileObj.id}"]`);
        renderLogsTile({ id: `settings-logs-${tileObj.id}`, el: tileObj.el }, host);
        activityMounted = true;
      }
      if (id === "tools" && !toolsMounted) {
        const host = body.querySelector(`[data-s-tools-host="${tileObj.id}"]`);
        renderToolsTile({ id: `settings-tools-${tileObj.id}`, el: tileObj.el }, host);
        toolsMounted = true;
      }
    });
  });

  const nameInput = body.querySelector(`[data-s-name="${tileObj.id}"]`);
  const aboutInput = body.querySelector(`[data-s-about="${tileObj.id}"]`);
  const locationInput = body.querySelector(`[data-s-location="${tileObj.id}"]`);
  const modelSelect = body.querySelector(`[data-s-model="${tileObj.id}"]`);
  const providerSelect = body.querySelector(`[data-s-provider="${tileObj.id}"]`);
  const provKeyInput = body.querySelector(`[data-s-provkey="${tileObj.id}"]`);
  const provHint = body.querySelector(`[data-s-prov-hint="${tileObj.id}"]`);
  const customBaseInput = body.querySelector(`[data-s-custombase="${tileObj.id}"]`);
  const modelDatalist = body.querySelector(`[data-s-model-list="${tileObj.id}"]`);
  const modelStatusEl = body.querySelector(`[data-s-model-status="${tileObj.id}"]`);
  const maxTokensInput = body.querySelector(`[data-s-maxtokens="${tileObj.id}"]`);
  const groqInput = body.querySelector(`[data-s-groq="${tileObj.id}"]`);
  const searchInput = body.querySelector(`[data-s-search="${tileObj.id}"]`);
  const togetherInput = body.querySelector(`[data-s-together="${tileObj.id}"]`);
  const mapboxInput = body.querySelector(`[data-s-mapbox="${tileObj.id}"]`);
  const imgProvSelect = body.querySelector(`[data-s-imgprov="${tileObj.id}"]`);
  const imgModelSelect = body.querySelector(`[data-s-imgmodel="${tileObj.id}"]`);
  const wsInput = body.querySelector(`[data-s-workspace="${tileObj.id}"]`);
  const voiceidInput = body.querySelector(`[data-s-voiceid="${tileObj.id}"]`);
  const ttsProvSelect = body.querySelector(`[data-s-ttsprov="${tileObj.id}"]`);
  const ttsModelInput = body.querySelector(`[data-s-ttsmodel="${tileObj.id}"]`);
  const ttsStatusEl = body.querySelector(`[data-s-tts-status="${tileObj.id}"]`);
  const sttProvSelect = body.querySelector(`[data-s-sttprov="${tileObj.id}"]`);
  const autostartInput = body.querySelector(`[data-s-autostart="${tileObj.id}"]`);
  const notifyInput = body.querySelector(`[data-s-notify="${tileObj.id}"]`);
  const qaShortcutInput = body.querySelector(`[data-s-qashortcut="${tileObj.id}"]`);
  const fbProviderSelect = body.querySelector(`[data-s-fbprovider="${tileObj.id}"]`);
  const fbModelInput = body.querySelector(`[data-s-fbmodel="${tileObj.id}"]`);
  const keyFallbackInput = body.querySelector(`[data-s-keyfallback="${tileObj.id}"]`);
  const autoVerifyInput = body.querySelector(`[data-s-autoverify="${tileObj.id}"]`);
  const localTranscribeInput = body.querySelector(`[data-s-localtranscribe="${tileObj.id}"]`);
  const hwEncodeInput = body.querySelector(`[data-s-hwencode="${tileObj.id}"]`);
  const approvalTimeoutInput = body.querySelector(`[data-s-approvaltimeout="${tileObj.id}"]`);
  const mobileToggleBtn = body.querySelector(`[data-s-mobile-toggle="${tileObj.id}"]`);
  const mobileStateEl = body.querySelector(`[data-s-mobile-state="${tileObj.id}"]`);
  const mobilePortInput = body.querySelector(`[data-s-mobile-port="${tileObj.id}"]`);
  const mobileErrorEl = body.querySelector(`[data-s-mobile-error="${tileObj.id}"]`);
  const mobilePairingEl = body.querySelector(`[data-s-mobile-pairing="${tileObj.id}"]`);
  const mobileQrEl = body.querySelector(`[data-s-mobile-qr="${tileObj.id}"]`);
  const mobileUrlsEl = body.querySelector(`[data-s-mobile-urls="${tileObj.id}"]`);
  const mobileRegenBtn = body.querySelector(`[data-s-mobile-regen="${tileObj.id}"]`);
  const mcpInput = body.querySelector(`[data-s-mcp="${tileObj.id}"]`);
  const mcpStatusEl = body.querySelector(`[data-s-mcp-status="${tileObj.id}"]`);
  const themeSelect = body.querySelector(`[data-s-theme="${tileObj.id}"]`);
  const statusEl = body.querySelector(`[data-s-status="${tileObj.id}"]`);
  const imgStatusEl = body.querySelector(`[data-s-img-status="${tileObj.id}"]`);

  function renderMcpStatus(servers) {
    if (!servers.length) {
      mcpStatusEl.textContent = "No servers configured";
      return;
    }
    mcpStatusEl.textContent = servers
      .map(s => s.connected
        ? `✓ ${s.name}: ${s.toolCount} tool${s.toolCount === 1 ? "" : "s"}`
        : `✗ ${s.name}: ${s.error || "not connected"}`)
      .join("\n");
  }

  async function refreshMcpStatus(reload = false) {
    if (!(mcpInput.value || "").trim()) { mcpStatusEl.textContent = ""; return; }
    mcpStatusEl.textContent = reload ? "Reloading servers…" : "Checking servers…";
    try {
      renderMcpStatus(await invoke(reload ? "reload_mcp_servers" : "get_mcp_status"));
    } catch (e) {
      mcpStatusEl.textContent = String(e);
    }
  }

  // ---------- Remote access (phone) ----------
  // Build the pairing URL ourselves from status fields rather than trusting arbitrary strings.
  function mobileUrl(ip, port, token) {
    return `http://${ip}:${port}/?token=${encodeURIComponent(token)}`;
  }

  function renderMobileStatus(status) {
    const running = !!(status && status.running);
    mobileToggleBtn.textContent = running ? "Disable" : "Enable";
    mobileToggleBtn.classList.toggle("danger", running);
    const port = status && status.port ? status.port : 8737;
    if (running) {
      mobileStateEl.textContent = `Running on port ${port}`;
    } else {
      mobileStateEl.textContent = "Off";
    }
    // Don't clobber a port the user is editing; only set placeholder-backing value when empty.
    if (!mobilePortInput.value && status && status.port) mobilePortInput.value = status.port;

    if (!running) {
      mobilePairingEl.style.display = "none";
      mobileQrEl.innerHTML = "";
      mobileUrlsEl.innerHTML = "";
      return;
    }

    const token = (status && status.token) || "";
    const ips = Array.isArray(status && status.lan_ips) ? status.lan_ips : [];
    // Build our own URL list from status fields.
    const urls = ips.map(ip => mobileUrl(ip, port, token));

    mobilePairingEl.style.display = "block";

    // QR of the first LAN URL. The SVG is generated from our own data.
    mobileQrEl.innerHTML = "";
    if (urls.length) {
      const qr = qrcode(0, "M");
      qr.addData(urls[0]);
      qr.make();
      mobileQrEl.innerHTML = qr.createSvgTag({ cellSize: 4, margin: 2 });
    }

    // URL rows: selectable text + copy button each.
    mobileUrlsEl.innerHTML = urls.map((url, i) => `
      <div class="mobile-url-row">
        <span class="mobile-url-text" data-s-mobile-url="${tileObj.id}-${i}">${escapeHtml(url)}</span>
        <button class="form-btn" data-s-mobile-copy="${tileObj.id}-${i}">Copy</button>
      </div>
    `).join("");
    urls.forEach((url, i) => {
      const btn = mobileUrlsEl.querySelector(`[data-s-mobile-copy="${tileObj.id}-${i}"]`);
      if (btn) btn.addEventListener("click", async () => {
        try {
          await navigator.clipboard.writeText(url);
          btn.textContent = "Copied";
          setTimeout(() => { btn.textContent = "Copy"; }, 1500);
        } catch {}
      });
    });
  }

  async function refreshMobileStatus() {
    try {
      renderMobileStatus(await invoke("get_mobile_server_status"));
    } catch (e) {
      mobileErrorEl.textContent = String(e);
    }
  }

  mobileToggleBtn.addEventListener("click", async () => {
    mobileErrorEl.textContent = "";
    let status;
    try { status = await invoke("get_mobile_server_status"); } catch {}
    const running = !!(status && status.running);
    mobileToggleBtn.disabled = true;
    try {
      if (running) {
        await invoke("stop_mobile_server");
      } else {
        // Persist the port first if the user changed it.
        const portRaw = mobilePortInput.value.trim();
        if (portRaw && /^\d+$/.test(portRaw)) {
          await invoke("set_app_settings", { req: { settings: { "mobile/server_port": portRaw } } });
        }
        const addr = await invoke("start_mobile_server");
        mobileStateEl.textContent = `Started on ${addr}`;
      }
    } catch (e) {
      mobileErrorEl.textContent = String(e);
    } finally {
      mobileToggleBtn.disabled = false;
      await refreshMobileStatus();
    }
  });

  mobileRegenBtn.addEventListener("click", async () => {
    // window.confirm() is a silent no-op in the Tauri webview — use the dialog plugin.
    let yes = false;
    try {
      const { ask } = await import("@tauri-apps/plugin-dialog");
      yes = await ask("This will disconnect the phone until you re-scan.", {
        title: "Regenerate pairing token",
        kind: "warning",
      });
    } catch (err) { console.error("ask dialog failed:", err); }
    if (!yes) return;
    mobileErrorEl.textContent = "";
    try {
      await invoke("regenerate_mobile_token");
    } catch (e) {
      mobileErrorEl.textContent = String(e);
    }
    await refreshMobileStatus();
  });

  function ensureOption(select, value) {
    if (!value) return;
    const exists = Array.from(select.options).some(o => o.value === value);
    if (!exists) {
      const opt = document.createElement("option");
      opt.value = value;
      opt.textContent = value;
      select.appendChild(opt);
    }
    select.value = value;
  }

  // The app_config key that holds the API key for a given provider.
  function providerKeyName(provider) {
    if (provider === "minimax") return "llm/minimax_api_key";
    if (provider === "openai") return "llm/openai_api_key";
    if (provider === "anthropic") return "llm/anthropic_api_key";
    return `llm/${provider}_api_key`;
  }

  // Suggested model placeholder per provider (shown when the model field is empty).
  const MODEL_PLACEHOLDERS = {
    minimax: "MiniMax-M3",
    meta: "muse-spark-1.1",
    openai: "gpt-4.1-mini",
    anthropic: "claude-sonnet-4-5",
    groq: "llama-3.3-70b-versatile",
    google: "gemini-2.5-pro",
    xai: "grok-3",
    custom: "e.g. llama3.1",
  };
  function modelPlaceholder(prov) {
    return MODEL_PLACEHOLDERS[prov] ? `Model name (e.g. ${MODEL_PLACEHOLDERS[prov]})` : "Model name";
  }

  // Reflect, for the selected provider, whether a key is already saved.
  function updateProviderHint() {
    const prov = providerSelect.value;
    const keyName = providerKeyName(prov);
    const saved = ui.loadedSettings && ui.loadedSettings[keyName];
    provKeyInput.placeholder = saved
      ? `${prov} key (saved — leave blank to keep)`
      : `API key for ${prov}`;
    customBaseInput.style.display = prov === "custom" ? "" : "none";
    if (prov === "minimax") {
      provHint.textContent = "Your current setup. Anthropic-compatible MiniMax endpoint.";
    } else if (prov === "meta") {
      provHint.textContent = "muse-spark-1.1 — $1.25/M in, $4.25/M out";
    } else if (prov === "custom") {
      provHint.textContent = "Local or self-hosted OpenAI-compatible server; API key optional.";
    } else {
      provHint.textContent = saved ? "" : `No ${prov} key saved yet — enter one to use this provider.`;
    }
  }

  providerSelect.addEventListener("change", () => {
    const prov = providerSelect.value;
    provKeyInput.value = "";
    // Per-provider model memory: restore a remembered model, else clear and hint.
    const remembered = ui.loadedSettings && ui.loadedSettings["llm/model/" + prov];
    modelSelect.value = remembered || "";
    modelSelect.placeholder = modelPlaceholder(prov);
    modelDatalist.innerHTML = "";
    modelStatusEl.textContent = "";
    updateProviderHint();
  });

  // Load settings
  async function loadS() {
    const s = await invoke("get_app_settings");
    ui.loadedSettings = s;
    nameInput.value = s["user/name"] || "";
    aboutInput.value = s["user/about"] || "";
    locationInput.value = s["user/location"] || "";

    providerSelect.value = s["llm/provider"] || "minimax";
    // Prefer the per-provider remembered model; fall back to the global llm/model only
    // when no per-provider entry exists yet (pre-upgrade settings), and to the MiniMax
    // default only on a fresh install. An empty remembered model stays empty — the
    // placeholder guides the user rather than resurrecting another provider's model.
    const remembered = s["llm/model/" + providerSelect.value];
    const currentModel = remembered !== undefined ? remembered : (s["llm/model"] ?? "MiniMax-M3");
    modelSelect.value = currentModel;
    modelSelect.placeholder = modelPlaceholder(providerSelect.value);
    provKeyInput.value = "";
    customBaseInput.value = s["llm/custom_base_url"] || "";
    modelDatalist.innerHTML = "";
    modelStatusEl.textContent = "";
    maxTokensInput.value = s["llm/max_tokens"] || "";
    fbProviderSelect.value = s["llm/fallback_provider"] || "";
    fbModelInput.value = s["llm/fallback_model"] || "";
    keyFallbackInput.checked = s["llm/key_fallback"] !== "0"; // default ON when unset
    updateProviderHint();

    groqInput.value = "";
    searchInput.value = "";
    togetherInput.value = "";
    mapboxInput.value = "";
    groqInput.placeholder = s["llm/groq_api_key"] ? "Groq key (saved)" : "Groq API key (voice transcription)";
    searchInput.placeholder = s["search/brave_api_key"] ? "Search key (saved)" : "Web search key";
    togetherInput.placeholder = s["imagegen/together_api_key"] ? "Together key (saved)" : "Together AI key";
    mapboxInput.placeholder = s["maps/mapbox_api_key"] ? "Mapbox key (saved)" : "Mapbox API key (maps)";
    voiceidInput.value = s["tts/voice_id"] || "";
    ttsProvSelect.value = s["tts/provider"] || "minimax";
    ttsModelInput.value = s["tts/model"] || "";
    sttProvSelect.value = s["stt/provider"] || "auto";
    imgProvSelect.value = s["imagegen/provider"] || "";

    const currentImgModel = s["imagegen/model"] || "";
    ensureOption(imgModelSelect, currentImgModel);

    sidebarModel.textContent = currentModel || "default model";
    sidebarModel.title = currentModel || "provider default";

    try { autostartInput.checked = await isAutostartEnabled(); } catch {}

    notifyInput.checked = s["notifications/enabled"] !== "false";
    qaShortcutInput.value = s["quickask/shortcut"] || "cmd+shift+space";
    autoVerifyInput.checked = s["safety/auto_verify"] === "1";
    localTranscribeInput.checked = s["video/local_transcription"] === "1";
    hwEncodeInput.checked = s["video/hw_encode"] !== "0"; // default ON when unset
    approvalTimeoutInput.value = s["safety/approval_timeout_secs"] || "";
    mobilePortInput.value = s["mobile/server_port"] || "";
    refreshMobileStatus();
    mcpInput.value = s["mcp/servers"] || "";
    refreshMcpStatus();

    themeSelect.value = localStorage.getItem("muninn-theme") || "dark";

    // Default directory
    const pathInfo = await invoke("get_workspace_root");
    const workspaceRoot = field(pathInfo, "workspaceRoot", "workspace_root");
    wsInput.value = workspaceRoot;
    sidebarWorkspace.textContent = projectLabel(workspaceRoot);
    sidebarWorkspace.title = workspaceRoot || "~/";
  }

  // Save
  body.querySelector(`[data-s-save="${tileObj.id}"]`).addEventListener("click", async () => {
    const updates = {
      "user/name": nameInput.value.trim(),
      "user/about": aboutInput.value.trim(),
      "user/location": locationInput.value.trim(),
      "llm/provider": providerSelect.value,
      "llm/model": modelSelect.value.trim(),
      "llm/custom_base_url": customBaseInput.value.trim(),
      "llm/fallback_provider": fbProviderSelect.value,
      "llm/fallback_model": fbModelInput.value.trim(),
      "llm/key_fallback": keyFallbackInput.checked ? "1" : "0",
      "tts/provider": ttsProvSelect.value,
      "tts/model": ttsModelInput.value.trim(),
      "stt/provider": sttProvSelect.value,
      "imagegen/provider": imgProvSelect.value.trim(),
      "imagegen/model": imgModelSelect.value.trim()
    };
    // Per-provider model memory: remember the active model under the selected provider.
    updates["llm/model/" + providerSelect.value] = modelSelect.value.trim();
    // Max response tokens: store a positive integer, or "" to fall back to the default.
    const maxTokensRaw = maxTokensInput.value.trim();
    if (maxTokensRaw === "" || /^\d+$/.test(maxTokensRaw) && parseInt(maxTokensRaw, 10) > 0) {
      updates["llm/max_tokens"] = maxTokensRaw;
    } else {
      statusEl.textContent = "Max response tokens must be a positive whole number — not saved";
      setTimeout(() => { statusEl.textContent = ""; }, 3000);
      return;
    }
    // Generic provider key → the selected provider's key setting (only when entered).
    if (provKeyInput.value.trim()) updates[providerKeyName(providerSelect.value)] = provKeyInput.value.trim();
    if (groqInput.value.trim()) updates["llm/groq_api_key"] = groqInput.value.trim();
    if (searchInput.value.trim()) updates["search/brave_api_key"] = searchInput.value.trim();
    if (togetherInput.value.trim()) updates["imagegen/together_api_key"] = togetherInput.value.trim();
    if (mapboxInput.value.trim()) {
      updates["maps/mapbox_api_key"] = mapboxInput.value.trim();
      ui.mapboxToken = mapboxInput.value.trim();
    }
    updates["tts/voice_id"] = voiceidInput.value.trim();
    updates["notifications/enabled"] = notifyInput.checked ? "true" : "false";
    updates["quickask/shortcut"] = qaShortcutInput.value.trim();
    updates["safety/auto_verify"] = autoVerifyInput.checked ? "1" : "0";
    updates["video/local_transcription"] = localTranscribeInput.checked ? "1" : "0";
    updates["video/hw_encode"] = hwEncodeInput.checked ? "1" : "0";
    // Approval timeout: blank falls back to the Rust default (120); otherwise clamp to [15, 3600].
    const approvalRaw = approvalTimeoutInput.value.trim();
    if (approvalRaw === "") {
      updates["safety/approval_timeout_secs"] = "";
    } else if (/^\d+$/.test(approvalRaw)) {
      const clamped = Math.min(3600, Math.max(15, parseInt(approvalRaw, 10)));
      updates["safety/approval_timeout_secs"] = String(clamped);
    } else {
      statusEl.textContent = "Approval timeout must be a whole number of seconds — not saved";
      setTimeout(() => { statusEl.textContent = ""; }, 3000);
      return;
    }

    const mcpJson = mcpInput.value.trim();
    if (mcpJson) {
      try {
        JSON.parse(mcpJson);
        updates["mcp/servers"] = mcpJson;
      } catch {
        statusEl.textContent = "MCP servers JSON is invalid — not saved";
        setTimeout(() => { statusEl.textContent = ""; }, 3000);
        return;
      }
    } else {
      updates["mcp/servers"] = "";
    }

    await invoke("set_app_settings", { req: { settings: updates } });
    // Restart MCP connections so config edits (changed args/env) take effect.
    if (mcpJson !== (ui.loadedSettings["mcp/servers"] || "")) refreshMcpStatus(true);
    // Geocode location if changed and Mapbox key available
    const loc = locationInput.value.trim();
    const token = ui.mapboxToken || updates["maps/mapbox_api_key"];
    if (loc && token) {
      try {
        await invoke("geocode_user_location", { location: loc });
        statusEl.textContent = "Saved (location set ✓)";
      } catch (e) {
        console.warn("Location geocoding failed:", e);
        statusEl.textContent = "Saved (location failed: " + e + ")";
      }
    } else if (loc && !token) {
      statusEl.textContent = "Saved (add Mapbox key for location)";
    } else {
      statusEl.textContent = "Saved";
    }
    ui.loadedSettings = { ...ui.loadedSettings, ...updates };
    sidebarModel.textContent = updates["llm/model"] || "default model";
    sidebarModel.title = updates["llm/model"] || "provider default";
    setTimeout(() => { statusEl.textContent = ""; }, 2000);
    await loadS();
  });

  // ---------- Text-model fetch / test ----------
  // MiniMax has no public models API — offer a curated list instead.
  const MINIMAX_TEXT_MODELS = ["MiniMax-M3", "MiniMax-M2.7", "MiniMax-Text-01"];

  function fillModelDatalist(models) {
    modelDatalist.innerHTML = "";
    for (const m of models) {
      const opt = document.createElement("option");
      opt.value = m.id; // .value assignment is safe — not an innerHTML sink
      if (m.label && m.label !== m.id) opt.label = m.label;
      modelDatalist.appendChild(opt);
    }
    // Empty input + fresh list: pre-fill the first model so Fetch → Test just works.
    if (!modelSelect.value.trim() && models.length) modelSelect.value = models[0].id;
  }

  body.querySelector(`[data-s-fetch-models="${tileObj.id}"]`).addEventListener("click", async () => {
    const prov = providerSelect.value;
    if (prov === "minimax") {
      fillModelDatalist(MINIMAX_TEXT_MODELS.map(id => ({ id })));
      modelStatusEl.textContent = `${MINIMAX_TEXT_MODELS.length} models loaded`;
      return;
    }
    modelStatusEl.textContent = "Fetching…";
    try {
      const models = await invoke("fetch_models", { provider: prov });
      // Drop image/embedding models when the provider reports a type; otherwise keep all.
      const hasTypes = models.some(m => m.type);
      const chat = hasTypes ? models.filter(m => m.type !== "image" && m.type !== "embedding") : models;
      fillModelDatalist(chat.map(m => ({ id: m.id, label: m.label })));
      modelStatusEl.textContent = `${chat.length} models loaded`;
    } catch (e) {
      modelStatusEl.textContent = String(e);
    }
  });

  const testLlmBtn = body.querySelector(`[data-s-test-llm="${tileObj.id}"]`);
  testLlmBtn.addEventListener("click", async () => {
    const prov = providerSelect.value;
    const model = modelSelect.value.trim();
    if (!model) { modelStatusEl.textContent = "Enter a model name first"; return; }
    // Unsaved keys aren't visible to the backend — prompt to Save so the test is meaningful.
    if (provKeyInput.value.trim()) {
      modelStatusEl.textContent = "Save first — the entered key isn't used until saved.";
      return;
    }
    testLlmBtn.disabled = true;
    modelStatusEl.textContent = "Testing…";
    try {
      modelStatusEl.textContent = await invoke("test_llm_connection", { provider: prov, model });
    } catch (e) {
      modelStatusEl.textContent = String(e);
    } finally {
      testLlmBtn.disabled = false;
    }
  });

  // ---------- Voice preview ----------
  const ttsPreviewBtn = body.querySelector(`[data-s-tts-preview="${tileObj.id}"]`);
  ttsPreviewBtn.addEventListener("click", async () => {
    ttsPreviewBtn.disabled = true;
    ttsStatusEl.textContent = "Loading voice…";
    try {
      const resp = await invoke("tts_speak", { req: { text: "Hi, this is your Muninn voice.", provider: ttsProvSelect.value } });
      if (!resp.audioBase64) { ttsStatusEl.textContent = "No audio returned"; return; }
      const blob = new Blob(
        [Uint8Array.from(atob(resp.audioBase64), c => c.charCodeAt(0))],
        { type: resp.contentType || "audio/mp3" }
      );
      const url = URL.createObjectURL(blob);
      const audio = new Audio(url);
      audio.onended = () => URL.revokeObjectURL(url);
      audio.play();
      ttsStatusEl.textContent = "";
    } catch (e) {
      ttsStatusEl.textContent = String(e);
    } finally {
      ttsPreviewBtn.disabled = false;
    }
  });

  // Known image models per provider (for providers without a fetch API)
  const KNOWN_IMG_MODELS = {
    minimax: [{ id: "image-01", label: "image-01" }, { id: "image-01-live", label: "image-01-live" }],
  };

  function populateImgModels(models, keepCurrent) {
    const currentVal = keepCurrent ? imgModelSelect.value : "";
    imgModelSelect.innerHTML = '<option value="">Default</option>';
    for (const m of models) {
      const opt = document.createElement("option");
      opt.value = m.id;
      opt.textContent = m.label || m.id;
      imgModelSelect.appendChild(opt);
    }
    if (models.some(m => m.id === currentVal)) {
      imgModelSelect.value = currentVal;
    }
  }

  // When provider changes, reset model list
  imgProvSelect.addEventListener("change", () => {
    const prov = imgProvSelect.value;
    const known = KNOWN_IMG_MODELS[prov];
    if (known) {
      populateImgModels(known, false);
    } else {
      imgModelSelect.innerHTML = '<option value="">Default</option>';
    }
  });

  // Fetch image models
  body.querySelector(`[data-s-fetch-img-models="${tileObj.id}"]`).addEventListener("click", async () => {
    const prov = imgProvSelect.value;
    if (!prov) { imgStatusEl.textContent = "Select a provider first"; return; }

    // Use known models for providers without a fetch API
    const known = KNOWN_IMG_MODELS[prov];
    if (known) {
      populateImgModels(known, true);
      imgStatusEl.textContent = `${known.length} models loaded`;
      setTimeout(() => { imgStatusEl.textContent = ""; }, 3000);
      return;
    }

    imgStatusEl.textContent = "Fetching...";
    try {
      const models = await invoke("fetch_models", { provider: prov });
      // When the provider reports a type, trust it (keep only image models).
      // Otherwise fall back to a name-substring heuristic.
      const hasTypes = models.some(m => m.type);
      const imgModels = hasTypes
        ? models.filter(m => m.type === "image")
        : models.filter(m => {
            const id = (m.id || "").toLowerCase();
            return id.includes("image") || id.includes("flux") || id.includes("dall") ||
                   id.includes("stable") || id.includes("sdxl") || id.includes("qwen-image");
          });
      const useModels = imgModels.length > 0 ? imgModels : models;
      populateImgModels(useModels, true);
      imgStatusEl.textContent = `${useModels.length} models loaded`;
      setTimeout(() => { imgStatusEl.textContent = ""; }, 3000);
    } catch (e) {
      imgStatusEl.textContent = String(e);
    }
  });

  // Default directory
  body.querySelector(`[data-s-setws="${tileObj.id}"]`).addEventListener("click", async () => {
    try {
      const result = await invoke("set_workspace_root", { req: { workspaceRoot: wsInput.value.trim() } });
      wsInput.value = field(result, "workspaceRoot", "workspace_root");
      sidebarWorkspace.textContent = projectLabel(wsInput.value);
      sidebarWorkspace.title = wsInput.value;
      statusEl.textContent = "Default directory set";
      setTimeout(() => { statusEl.textContent = ""; }, 2000);
    } catch (e) { statusEl.textContent = String(e); }
  });

  // MCP reload
  body.querySelector(`[data-s-mcp-reload="${tileObj.id}"]`).addEventListener("click", () => refreshMcpStatus(true));

  // Theme
  themeSelect.addEventListener("change", () => {
    const val = themeSelect.value;
    localStorage.setItem("muninn-theme", val);
    applyTheme(val);
  });

  // Autostart
  autostartInput.addEventListener("change", async () => {
    try {
      if (autostartInput.checked) await enableAutostart();
      else await disableAutostart();
    } catch (e) {
      autostartInput.checked = !autostartInput.checked;
    }
  });

  loadS();
}


registerTileRenderer("settings", renderSettingsTile);
