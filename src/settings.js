import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { enable as enableAutostart, disable as disableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
import { ui, navSettingsBtn } from "./state.js";
import { updateModelChips } from "./chat.js";
import { applyTheme, THEMES } from "./theme.js";
import { field, escapeHtml } from "./utils.js";
import { icon } from "./icons.js";
import qrcode from "qrcode-generator";
import { renderLogsTile } from "./logs.js";
import { renderToolsTile } from "./toolspanel.js";

// ========== SETTINGS TILE ==========
export function renderSettingsTile(tileObj, body) {
  body.innerHTML = `
    <div class="settings-layout">
      <nav class="settings-nav" data-s-tabs="${tileObj.id}">
        <button type="button" class="settings-tab active" data-s-tab="main">Main</button>
        <button type="button" class="settings-tab" data-s-tab="keys">API Keys</button>
        <button type="button" class="settings-tab" data-s-tab="voice">Voice &amp; Media</button>
        <button type="button" class="settings-tab" data-s-tab="ui">UI</button>
        <button type="button" class="settings-tab" data-s-tab="system">System</button>
        <button type="button" class="settings-tab" data-s-tab="remote">Remote</button>
        <button type="button" class="settings-tab" data-s-tab="mcp">MCP</button>
        <button type="button" class="settings-tab" data-s-tab="activity">Activity</button>
        <button type="button" class="settings-tab" data-s-tab="tools">Tools</button>
      </nav>
      <div class="settings-main">
        <div class="settings-pages">

          <div class="settings-page active" data-s-page="main"><div class="settings-section">
            <div class="settings-group">
              <h3>Profile</h3>
              <input class="mb-2" type="text" placeholder="Your name" data-s-name="${tileObj.id}" />
              <textarea rows="2" placeholder="About you" data-s-about="${tileObj.id}"></textarea>
              <input class="mt-2" type="text" placeholder="Your location (e.g. Mountain Center, CA)" data-s-location="${tileObj.id}" />
            </div>
            <div class="settings-group">
              <h3>Model</h3>
              <div class="form-row mb-2">
                <select class="grow min-w-0" data-s-provider="${tileObj.id}">
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
              <input class="mb-1" type="text" placeholder="http://localhost:11434/v1" data-s-custombase="${tileObj.id}" style="display:none;" />
              <input class="mb-1" type="password" placeholder="API key for the selected provider" data-s-provkey="${tileObj.id}" />
              <span class="muted text-xs" data-s-prov-hint="${tileObj.id}"></span>
              <div class="form-row mt-2 items-center">
                <button class="form-btn" data-s-test-llm="${tileObj.id}">Test</button>
                <span class="muted text-xs" data-s-model-status="${tileObj.id}"></span>
              </div>
              <div class="form-row mt-2">
                <input class="grow" type="number" min="1" step="1024" placeholder="Max response tokens (e.g. 40960)" data-s-maxtokens="${tileObj.id}" />
              </div>
              <span class="muted text-xs">Output-token ceiling per response. Blank = 40960.</span>
            </div>
            <div class="settings-group">
              <h3>Fallback</h3>
              <div class="form-row">
                <select class="grow min-w-0" data-s-fbprovider="${tileObj.id}">
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
              <span class="muted text-xs">Retried automatically when the primary model fails.</span>
              <label class="checkbox-inline mt-2">
                <input type="checkbox" data-s-keyfallback="${tileObj.id}" /> Use the OpenAI key for providers without their own key
              </label>
            </div>
            <div class="settings-group">
              <h3>Default directory</h3>
              <p class="settings-hint">Where a chat operates when no project folder is picked — set per chat with the ${icon("folder", 12)} chip in the composer.</p>
              <div class="form-row">
                <input type="text" placeholder="Default directory path" data-s-workspace="${tileObj.id}" />
                <button class="form-btn" data-s-setws="${tileObj.id}">Set</button>
              </div>
            </div>
          </div></div>

          <div class="settings-page" data-s-page="keys"><div class="settings-section">
            <div class="settings-group">
              <h3>Service keys</h3>
              <p class="settings-hint">Model-provider keys live under Main → Model. These cover everything else.</p>
              <input class="mb-2" type="password" placeholder="Groq API key (voice transcription)" data-s-groq="${tileObj.id}" />
              <input class="mb-2" type="password" placeholder="Web search API key" data-s-search="${tileObj.id}" />
              <input class="mb-2" type="password" placeholder="Together AI key (images)" data-s-together="${tileObj.id}" />
              <input class="mb-2" type="password" placeholder="Mapbox API key (maps)" data-s-mapbox="${tileObj.id}" />
              <input class="mb-2" type="password" placeholder="Fish Audio API key (TTS)" data-s-fishaudio="${tileObj.id}" />
            </div>
          </div></div>

          <div class="settings-page" data-s-page="voice"><div class="settings-section">
            <div class="settings-group">
              <h3>Voice</h3>
              <div class="form-row mb-2">
                <select class="grow min-w-0" data-s-ttsprov="${tileObj.id}">
                  <option value="minimax">MiniMax</option>
                  <option value="fishaudio">Fish Audio</option>
                  <option value="fishspeech">FishSpeech (local)</option>
                </select>
                <input class="grow-2 min-w-0" type="text" placeholder="TTS model (e.g. speech-2.8-hd)" data-s-ttsmodel="${tileObj.id}" />
              </div>
              <div class="form-row mb-2 items-center">
                <input class="grow" type="text" placeholder="Voice ID (MiniMax voice or Fish Audio reference id)" data-s-voiceid="${tileObj.id}" />
                <button class="form-btn" data-s-tts-preview="${tileObj.id}" title="Preview voice">▶</button>
              </div>
              <span class="muted text-xs" data-s-tts-status="${tileObj.id}"></span>
              <div class="form-row mt-2">
                <select class="grow" data-s-sttprov="${tileObj.id}">
                  <option value="auto">Automatic (Groq → OpenAI)</option>
                  <option value="groq">Groq</option>
                  <option value="openai">OpenAI</option>
                </select>
              </div>
              <span class="muted text-xs">Speech-to-text provider for voice input.</span>
              <label class="checkbox-inline mt-2">
                <input type="checkbox" data-s-localtranscribe="${tileObj.id}" /> On-device transcription (Apple Speech)
              </label>
              <span class="muted text-xs">Voice input and video analysis transcribe on your Mac — no key, audio never leaves the device. Falls back to cloud whisper.</span>
            </div>
            <div class="settings-group">
              <h3>Images</h3>
              <div class="form-row mb-2">
                <select data-s-imgprov="${tileObj.id}">
                  <option value="">None</option>
                  <option value="minimax">MiniMax</option>
                  <option value="together">Together</option>
                  <option value="openai">OpenAI</option>
                </select>
                <select class="grow" data-s-imgmodel="${tileObj.id}"></select>
                <button class="form-btn" data-s-fetch-img-models="${tileObj.id}">Fetch</button>
              </div>
              <span class="muted text-xs" data-s-img-status="${tileObj.id}"></span>
            </div>
            <div class="settings-group">
              <h3>Video</h3>
              <label class="checkbox-inline">
                <input type="checkbox" data-s-hwencode="${tileObj.id}" /> Hardware video encoding (VideoToolbox)
              </label>
              <span class="muted text-xs">Faster renders on the Mac's hardware encoder; falls back to software automatically.</span>
            </div>
          </div></div>

          <div class="settings-page" data-s-page="ui"><div class="settings-section">
            <div class="settings-group">
              <h3>Appearance</h3>
              <div class="form-row">
                <select data-s-theme="${tileObj.id}">
                  ${THEMES.map((t) => `<option value="${t.id}">${t.label}</option>`).join("")}
                </select>
              </div>
            </div>
            <div class="settings-group">
              <h3>Notifications</h3>
              <label class="checkbox-inline">
                <input type="checkbox" data-s-notify="${tileObj.id}" /> Desktop notifications
              </label>
            </div>
          </div></div>

          <div class="settings-page" data-s-page="system"><div class="settings-section">
            <div class="settings-group">
              <h3>System</h3>
              <label class="checkbox-inline">
                <input type="checkbox" data-s-autostart="${tileObj.id}" /> Launch at login
              </label>
              <div class="form-row mt-2">
                <input class="grow" type="text" placeholder="Quick-ask shortcut (e.g. cmd+shift+space)" data-s-qashortcut="${tileObj.id}" />
              </div>
              <span class="muted text-xs">Shortcut changes take effect after restarting Muninn.</span>
            </div>
            <div class="settings-group">
              <h3>Safety</h3>
              <label class="checkbox-inline">
                <input type="checkbox" data-s-autoverify="${tileObj.id}" /> Auto-verify dangerous commands (LLM reviews before asking you)
              </label>
              <div class="form-row mt-2">
                <input class="grow" type="number" min="15" max="3600" step="1" placeholder="120" data-s-approvaltimeout="${tileObj.id}" />
              </div>
              <span class="muted text-xs">Seconds before a verified command is deferred (15–3600).</span>
            </div>
          </div></div>

          <div class="settings-page" data-s-page="remote"><div class="settings-section">
            <div class="settings-group">
              <h3>Remote access (phone)</h3>
              <p class="settings-hint">Run a local server to chat with Muninn from your phone on the same Wi-Fi.</p>
              <div class="form-row mb-2 items-center">
                <button class="form-btn" data-s-mobile-toggle="${tileObj.id}">Enable</button>
                <span class="muted text-xs" data-s-mobile-state="${tileObj.id}"></span>
              </div>
              <div class="form-row mb-1 items-center">
                <input class="grow" type="number" min="1" max="65535" step="1" placeholder="8737" data-s-mobile-port="${tileObj.id}" />
                <span class="muted text-xs nowrap">applies on next start</span>
              </div>
              <span class="muted text-xs text-danger" data-s-mobile-error="${tileObj.id}"></span>
              <div class="mobile-pairing mt-3" data-s-mobile-pairing="${tileObj.id}" style="display:none;">
                <div class="mobile-qr" data-s-mobile-qr="${tileObj.id}"></div>
                <div class="mobile-urls" data-s-mobile-urls="${tileObj.id}"></div>
                <div class="form-row mt-2">
                  <button class="form-btn danger" data-s-mobile-regen="${tileObj.id}">Regenerate token</button>
                </div>
                <p class="settings-hint mt-2">Works on your Wi-Fi. For access from anywhere, install Tailscale on both devices — the URL then works over it too.</p>
              </div>
              <p class="settings-hint mt-2 text-danger">Anyone with this URL can chat and run commands as Muninn. Keep it private.</p>
            </div>
          </div></div>

          <div class="settings-page" data-s-page="mcp"><div class="settings-section">
            <div class="settings-group">
              <h3>MCP Servers</h3>
              <p class="settings-hint">Connect Model Context Protocol servers — their tools become available to Muninn in chat and agent runs. One JSON entry per server.</p>
              <textarea rows="6" spellcheck="false" placeholder='[{"name": "github", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "..."}}]' data-s-mcp="${tileObj.id}"></textarea>
              <div class="form-row mt-2">
                <button class="form-btn" data-s-mcp-reload="${tileObj.id}">Reload Servers</button>
              </div>
              <div class="muted text-xs" data-s-mcp-status="${tileObj.id}" style="white-space:pre-line;"></div>
            </div>
          </div></div>

          <div class="settings-page" data-s-page="activity"><div class="settings-section">
            <div class="settings-group">
              <h3>Activity log</h3>
              <p class="settings-hint">Tool runs, messages, and background events.</p>
              <div class="settings-embed" data-s-activity-host="${tileObj.id}"></div>
            </div>
          </div></div>

          <div class="settings-page" data-s-page="tools"><div class="settings-section">
            <div class="settings-group">
              <h3>Tool access</h3>
              <p class="settings-hint">Enable or disable tools the assistant can call.</p>
              <div class="settings-embed" data-s-tools-host="${tileObj.id}"></div>
            </div>
          </div></div>

        </div>
        <div class="settings-footer">
          <button class="form-btn" data-s-save="${tileObj.id}">Save Settings</button>
          <span class="muted" data-s-status="${tileObj.id}"></span>
        </div>
      </div>
    </div>
  `;

  // Section switching — Activity / Tools hosts are filled lazily on first visit.
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
  const fishaudioInput = body.querySelector(`[data-s-fishaudio="${tileObj.id}"]`);
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
    meta: "muse-spark-1.3",
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
      provHint.textContent = "muse-spark-1.3 (recommended) — 1M context, $1.25/M in, $4.25/M out";
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
    fishaudioInput.value = "";
    fishaudioInput.placeholder = s["tts/fishaudio_api_key"] ? "Fish Audio key (saved)" : "Fish Audio API key";
    voiceidInput.value = s["tts/voice_id"] || "";
    ttsProvSelect.value = s["tts/provider"] || "minimax";
    ttsModelInput.value = s["tts/model"] || "";
    sttProvSelect.value = s["stt/provider"] || "auto";
    imgProvSelect.value = s["imagegen/provider"] || "";

    const currentImgModel = s["imagegen/model"] || "";
    ensureOption(imgModelSelect, currentImgModel);

    updateModelChips();

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
    if (fishaudioInput.value.trim()) updates["tts/fishaudio_api_key"] = fishaudioInput.value.trim();
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
    updateModelChips();
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


// ========== SETTINGS MODAL (overlay) ==========
// Settings opens as a popup over the whole workspace rather than a tile.
// The tile renderer above still backs it (and legacy pop-out windows) — the
// modal simply mounts it into an overlay with a fake tile object.
let settingsOverlay = null;

export function isSettingsModalOpen() {
  return !!settingsOverlay;
}

function settingsEscListener(e) {
  if (e.key === "Escape") closeSettingsModal();
}

export function closeSettingsModal() {
  if (!settingsOverlay) return;
  settingsOverlay.remove();
  settingsOverlay = null;
  document.removeEventListener("keydown", settingsEscListener);
  if (navSettingsBtn) navSettingsBtn.classList.remove("open");
}

export function openSettingsModal(section) {
  if (settingsOverlay) {
    if (section) settingsOverlay.querySelector(`[data-s-tab="${section}"]`)?.click();
    return;
  }
  const overlay = document.createElement("div");
  overlay.className = "settings-overlay";
  overlay.innerHTML = `
    <div class="settings-modal" role="dialog" aria-modal="true" aria-label="Settings">
      <div class="settings-modal-head">
        <span class="settings-modal-title">Settings</span>
        <button class="settings-modal-close" aria-label="Close settings">${icon("x", 14)}</button>
      </div>
      <div class="settings-modal-body"></div>
    </div>
  `;
  document.body.appendChild(overlay);
  settingsOverlay = overlay;
  overlay.addEventListener("mousedown", (e) => {
    if (e.target === overlay) closeSettingsModal();
  });
  overlay.querySelector(".settings-modal-close").addEventListener("click", closeSettingsModal);
  document.addEventListener("keydown", settingsEscListener);
  renderSettingsTile({ id: "modal", el: overlay }, overlay.querySelector(".settings-modal-body"));
  if (section) overlay.querySelector(`[data-s-tab="${section}"]`)?.click();
  if (navSettingsBtn) navSettingsBtn.classList.add("open");
}

// Re-sync the open modal after out-of-band settings changes (agent set_setting).
export function refreshSettingsModalIfOpen() {
  if (!settingsOverlay) return;
  const active = settingsOverlay.querySelector(".settings-tab.active")?.dataset.sTab;
  renderSettingsTile({ id: "modal", el: settingsOverlay }, settingsOverlay.querySelector(".settings-modal-body"));
  if (active && active !== "main") settingsOverlay.querySelector(`[data-s-tab="${active}"]`)?.click();
}

registerTileRenderer("settings", renderSettingsTile);
