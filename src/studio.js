import { registerTileRenderer } from "./registry.js";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { escapeHtml } from "./utils.js";
import { logError } from "./uikit.js";
import { tiles } from "./state.js";
import { createVideoPreview } from "./videopreview.js";
import html2canvas from "html2canvas";

// Aspect presets for carousels/graphics (width × height in px).
const ASPECTS = {
  square: { w: 1080, h: 1080, label: "Square 1:1" },
  portrait: { w: 1080, h: 1350, label: "Portrait 4:5" },
  story: { w: 1080, h: 1920, label: "Story 9:16" },
};

// Rasterize one inline-styled slide HTML string to a PNG data URL via html2canvas.
async function renderSlideToPng(slideHtml, w, h) {
  const holder = document.createElement("div");
  holder.style.cssText = `position:fixed;left:-99999px;top:0;width:${w}px;height:${h}px;`;
  holder.innerHTML = slideHtml;
  document.body.appendChild(holder);
  const target = holder.firstElementChild || holder;
  try {
    const canvas = await html2canvas(target, { width: w, height: h, scale: 1, backgroundColor: null, logging: false, useCORS: true });
    return canvas.toDataURL("image/png");
  } finally {
    document.body.removeChild(holder);
  }
}

// ========== STUDIO (creative suite) TILE ==========
// Tabbed: Media (social posts) · Writing (long-form) · Docs · Video.
// A shared brand voice (persisted to studio/brand_voice) flows into every section.

const PLATFORMS = ["X / Twitter", "LinkedIn", "Instagram", "TikTok", "YouTube"];

const TABS = [
  { key: "media", label: "Media" },
  { key: "writing", label: "Writing" },
  { key: "docs", label: "Docs" },
  { key: "video", label: "Video" },
];

export function renderStudioTile(tileObj, body) {
  const id = tileObj.id;
  body.innerHTML = `
    <div class="studio">
      <div class="studio-header">
        <input type="text" class="studio-brand" placeholder="Brand voice (saved, used everywhere — e.g. warm, witty, plain-spoken)" data-st-brand="${id}" />
        <div class="studio-tabs" data-st-tabs="${id}">
          ${TABS.map((t, i) => `<button class="studio-tab${i === 0 ? " active" : ""}" data-st-tab="${t.key}">${t.label}</button>`).join("")}
        </div>
      </div>
      <div class="studio-section" data-st-section="${id}"></div>
    </div>
  `;

  const brandEl = body.querySelector(`[data-st-brand="${id}"]`);
  const tabsEl = body.querySelector(`[data-st-tabs="${id}"]`);
  const sectionEl = body.querySelector(`[data-st-section="${id}"]`);

  // ---- Brand voice (persisted, shared) ----
  invoke("get_app_settings").then(s => { brandEl.value = (s && s["studio/brand_voice"]) || ""; }).catch(() => {});
  let brandTimer = null;
  brandEl.addEventListener("input", () => {
    clearTimeout(brandTimer);
    brandTimer = setTimeout(() => {
      invoke("set_app_settings", { req: { settings: { "studio/brand_voice": brandEl.value.trim() } } }).catch(() => {});
    }, 600);
  });

  // ---- Tabs ----
  const renderers = {
    media: renderMediaSection,
    writing: renderWritingSection,
    docs: renderDocsSection,
    video: renderVideoSection,
  };
  function selectTab(key) {
    tabsEl.querySelectorAll(".studio-tab").forEach(b => b.classList.toggle("active", b.dataset.stTab === key));
    // Let the outgoing section tear down (e.g. the video preview pauses playback + drops
    // its media element) before we blow away its DOM.
    if (typeof sectionEl._sectionCleanup === "function") {
      try { sectionEl._sectionCleanup(); } catch {}
      sectionEl._sectionCleanup = null;
    }
    sectionEl.innerHTML = "";
    (renderers[key] || renderers.media)(sectionEl, id);
  }
  tabsEl.querySelectorAll(".studio-tab").forEach(btn => {
    btn.addEventListener("click", () => selectTab(btn.dataset.stTab));
  });

  selectTab("media");
}

// ---------- MEDIA section ----------
function renderMediaSection(el, id) {
  el.innerHTML = `
    <textarea class="studio-topic" rows="2" placeholder="What's the post about? (topic, link, or a few notes)" data-st-topic="${id}"></textarea>
    <div class="studio-platforms" data-st-platforms="${id}">
      ${PLATFORMS.map(p => `
        <label class="studio-chip">
          <input type="checkbox" value="${escapeHtml(p)}" ${p === "X / Twitter" || p === "LinkedIn" ? "checked" : ""} />
          ${escapeHtml(p)}
        </label>`).join("")}
    </div>
    <input type="text" class="studio-tone" placeholder="Tone for this batch (optional, on top of brand voice)" data-st-tone="${id}" />
    <div class="studio-actions">
      <button class="form-btn" data-st-gen-copy="${id}">Generate copy</button>
      <span class="muted" data-st-copy-status="${id}" style="font-size:10px;"></span>
    </div>

    <div class="studio-results" data-st-results="${id}"></div>

    <div class="studio-image-block">
      <h4 class="studio-h">Image</h4>
      <div class="form-row" style="gap:6px; margin-bottom:6px;">
        <input type="text" class="studio-img-prompt" placeholder="Image prompt (defaults to the topic)" data-st-img-prompt="${id}" style="flex:1;" />
        <button class="form-btn" data-st-gen-img="${id}">Generate image</button>
      </div>
      <span class="muted" data-st-img-status="${id}" style="font-size:10px;"></span>
      <div class="studio-image-preview" data-st-img-preview="${id}"></div>
    </div>

    <div class="studio-image-block">
      <h4 class="studio-h">Carousel / Graphic</h4>
      <div class="form-row" style="gap:6px; align-items:center; flex-wrap:wrap;">
        <select data-st-car-aspect="${id}" style="width:auto;">
          ${Object.entries(ASPECTS).map(([k, v]) => `<option value="${k}">${v.label}</option>`).join("")}
        </select>
        <label class="muted" style="font-size:11px;">Slides <input type="number" min="1" max="10" value="5" data-st-car-slides="${id}" style="width:56px; margin-left:6px;" /></label>
        <button class="form-btn" data-st-car-gen="${id}">Generate carousel</button>
        <button class="hint-chip" data-st-car-saveall="${id}" style="display:none;">Save all to Content</button>
      </div>
      <span class="muted" data-st-car-status="${id}" style="font-size:10px;"></span>
      <div class="studio-carousel-grid" data-st-car-grid="${id}"></div>
    </div>
  `;

  const topicEl = el.querySelector(`[data-st-topic="${id}"]`);
  const toneEl = el.querySelector(`[data-st-tone="${id}"]`);
  const platformsEl = el.querySelector(`[data-st-platforms="${id}"]`);
  const resultsEl = el.querySelector(`[data-st-results="${id}"]`);
  const copyStatusEl = el.querySelector(`[data-st-copy-status="${id}"]`);
  const genCopyBtn = el.querySelector(`[data-st-gen-copy="${id}"]`);
  const imgPromptEl = el.querySelector(`[data-st-img-prompt="${id}"]`);
  const imgStatusEl = el.querySelector(`[data-st-img-status="${id}"]`);
  const imgPreviewEl = el.querySelector(`[data-st-img-preview="${id}"]`);
  const genImgBtn = el.querySelector(`[data-st-gen-img="${id}"]`);

  genCopyBtn.addEventListener("click", async () => {
    const topic = topicEl.value.trim();
    const platforms = Array.from(platformsEl.querySelectorAll("input:checked")).map(c => c.value);
    if (!topic) { copyStatusEl.textContent = "Enter a topic."; return; }
    if (!platforms.length) { copyStatusEl.textContent = "Pick at least one platform."; return; }
    genCopyBtn.disabled = true;
    copyStatusEl.textContent = "Writing…";
    try {
      const results = await invoke("generate_post_copy", { req: { topic, platforms, tone: toneEl.value.trim() || null } });
      renderCopyResults(resultsEl, results);
      copyStatusEl.textContent = "";
    } catch (e) {
      logError("generate_post_copy", e);
      copyStatusEl.textContent = String(e);
    } finally {
      genCopyBtn.disabled = false;
    }
  });

  genImgBtn.addEventListener("click", async () => {
    const prompt = (imgPromptEl.value.trim() || topicEl.value.trim());
    if (!prompt) { imgStatusEl.textContent = "Enter an image prompt or a topic."; return; }
    genImgBtn.disabled = true;
    imgStatusEl.textContent = "Generating image… (this can take a moment)";
    imgPreviewEl.innerHTML = "";
    try {
      const path = await invoke("studio_generate_image", { prompt });
      let src;
      try { src = await invoke("read_image_base64", { path }); } catch { src = path; }
      imgPreviewEl.innerHTML = `<img class="studio-image" src="${src}" alt="generated image" />`;
      imgStatusEl.textContent = "Saved to Content ✓";
    } catch (e) {
      logError("studio_generate_image", e);
      imgStatusEl.textContent = String(e);
    } finally {
      genImgBtn.disabled = false;
    }
  });

  // ---- Carousel / graphic ----
  const carAspectEl = el.querySelector(`[data-st-car-aspect="${id}"]`);
  const carSlidesEl = el.querySelector(`[data-st-car-slides="${id}"]`);
  const carGenBtn = el.querySelector(`[data-st-car-gen="${id}"]`);
  const carSaveAllBtn = el.querySelector(`[data-st-car-saveall="${id}"]`);
  const carStatusEl = el.querySelector(`[data-st-car-status="${id}"]`);
  const carGridEl = el.querySelector(`[data-st-car-grid="${id}"]`);
  let renderedSlides = []; // data URLs

  carGenBtn.addEventListener("click", async () => {
    const topic = topicEl.value.trim();
    if (!topic) { carStatusEl.textContent = "Enter a topic up top first."; return; }
    const aspect = ASPECTS[carAspectEl.value] || ASPECTS.square;
    const slides = parseInt(carSlidesEl.value, 10) || 5;

    carGenBtn.disabled = true;
    carSaveAllBtn.style.display = "none";
    carGridEl.innerHTML = "";
    renderedSlides = [];
    carStatusEl.textContent = "Designing slides…";
    try {
      const slideHtmls = await invoke("generate_carousel", {
        req: { topic, slides, width: aspect.w, height: aspect.h }
      });
      carStatusEl.textContent = "Rendering…";
      for (let i = 0; i < slideHtmls.length; i++) {
        const dataUrl = await renderSlideToPng(slideHtmls[i], aspect.w, aspect.h);
        renderedSlides.push(dataUrl);
        addSlideCard(dataUrl, i);
      }
      carStatusEl.textContent = `${renderedSlides.length} slide${renderedSlides.length === 1 ? "" : "s"} ready.`;
      if (renderedSlides.length) carSaveAllBtn.style.display = "";
    } catch (e) {
      logError("generate_carousel", e);
      carStatusEl.textContent = String(e);
    } finally {
      carGenBtn.disabled = false;
    }
  });

  function addSlideCard(dataUrl, i) {
    const card = document.createElement("div");
    card.className = "studio-slide-card";
    const img = document.createElement("img");
    img.src = dataUrl;
    img.className = "studio-slide-img";
    const saveBtn = document.createElement("button");
    saveBtn.className = "hint-chip";
    saveBtn.textContent = "Save";
    saveBtn.addEventListener("click", async () => {
      try {
        await invoke("studio_save_png", { base64: dataUrl });
        saveBtn.textContent = "Saved ✓";
        setTimeout(() => { saveBtn.textContent = "Save"; }, 1200);
      } catch (e) { logError("studio_save_png", e); saveBtn.textContent = "Failed"; }
    });
    card.appendChild(img);
    card.appendChild(saveBtn);
    carGridEl.appendChild(card);
  }

  carSaveAllBtn.addEventListener("click", async () => {
    carSaveAllBtn.disabled = true;
    carStatusEl.textContent = "Saving…";
    let n = 0;
    for (const dataUrl of renderedSlides) {
      try { await invoke("studio_save_png", { base64: dataUrl }); n++; } catch (e) { logError("studio_save_png", e); }
    }
    carStatusEl.textContent = `Saved ${n} to Content ✓`;
    carSaveAllBtn.disabled = false;
  });
}

function renderCopyResults(resultsEl, results) {
  resultsEl.innerHTML = "";
  for (const pc of results) {
    const section = document.createElement("div");
    section.className = "studio-platform";
    section.innerHTML = `<h4 class="studio-h">${escapeHtml(pc.platform)}</h4>`;
    pc.variants.forEach((variant, i) => {
      const card = document.createElement("div");
      card.className = "studio-variant";
      const ta = document.createElement("textarea");
      ta.className = "studio-variant-text";
      ta.rows = Math.min(10, Math.max(2, Math.ceil(variant.length / 60)));
      ta.value = variant;
      const bar = document.createElement("div");
      bar.className = "studio-variant-bar";
      const label = document.createElement("span");
      label.className = "muted";
      label.style.fontSize = "10px";
      label.textContent = `Variant ${i + 1} · ${variant.length} chars`;
      const copyBtn = document.createElement("button");
      copyBtn.className = "hint-chip";
      copyBtn.textContent = "Copy";
      copyBtn.addEventListener("click", async () => {
        try {
          await navigator.clipboard.writeText(ta.value);
          copyBtn.textContent = "Copied ✓";
          setTimeout(() => { copyBtn.textContent = "Copy"; }, 1200);
        } catch { copyBtn.textContent = "Copy failed"; }
      });
      ta.addEventListener("input", () => { label.textContent = `Variant ${i + 1} · ${ta.value.length} chars`; });
      bar.appendChild(label);
      bar.appendChild(copyBtn);
      card.appendChild(ta);
      card.appendChild(bar);
      section.appendChild(card);
    });
    resultsEl.appendChild(section);
  }
}

// ---------- WRITING section ----------
const WRITING_ACTIONS = [
  { label: "Improve", instr: "Improve the clarity, flow, and impact of this text while keeping its meaning and roughly its length." },
  { label: "Shorten", instr: "Make this significantly more concise without losing the key points." },
  { label: "Expand", instr: "Expand this with more detail, examples, and depth, in the same voice." },
  { label: "Continue", instr: "Continue writing naturally from where this leaves off, in the same voice." },
  { label: "Fix grammar", instr: "Fix grammar, spelling, and punctuation. Make only the minimal necessary changes." },
];

function renderWritingSection(el, id) {
  el.innerHTML = `
    <textarea class="studio-draft" rows="14" placeholder="Write here — or describe what you want and hit a button below to draft it." data-st-draft="${id}"></textarea>
    <div class="studio-write-actions" data-st-write-actions="${id}">
      ${WRITING_ACTIONS.map((a, i) => `<button class="hint-chip" data-st-wact="${i}">${escapeHtml(a.label)}</button>`).join("")}
    </div>
    <div class="form-row" style="gap:6px; margin-top:6px;">
      <input type="text" class="studio-write-instr" placeholder="Or tell the AI what to do (e.g. 'rewrite as a LinkedIn post', 'make it punchier')" data-st-write-instr="${id}" style="flex:1;" />
      <button class="form-btn" data-st-write-apply="${id}">Apply</button>
    </div>
    <div class="studio-actions" style="margin-top:6px;">
      <button class="hint-chip" data-st-write-undo="${id}" style="display:none;">Undo</button>
      <button class="hint-chip" data-st-write-copy="${id}">Copy all</button>
      <span class="muted" data-st-write-status="${id}" style="font-size:10px;"></span>
    </div>
  `;

  const draftEl = el.querySelector(`[data-st-draft="${id}"]`);
  const instrEl = el.querySelector(`[data-st-write-instr="${id}"]`);
  const applyBtn = el.querySelector(`[data-st-write-apply="${id}"]`);
  const undoBtn = el.querySelector(`[data-st-write-undo="${id}"]`);
  const copyBtn = el.querySelector(`[data-st-write-copy="${id}"]`);
  const statusEl = el.querySelector(`[data-st-write-status="${id}"]`);
  let prevDraft = null;

  async function runInstruction(instruction) {
    if (!instruction.trim()) { statusEl.textContent = "Tell me what to do."; return; }
    if (!draftEl.value.trim() && /improve|shorten|expand|continue|fix/i.test(instruction) && !instrEl.value.trim()) {
      statusEl.textContent = "Write or paste some text first.";
      return;
    }
    applyBtn.disabled = true;
    statusEl.textContent = "Working…";
    try {
      const result = await invoke("writing_assist", { req: { text: draftEl.value, instruction } });
      prevDraft = draftEl.value;
      draftEl.value = result;
      undoBtn.style.display = "";
      statusEl.textContent = "Done ✓";
      setTimeout(() => { statusEl.textContent = ""; }, 1500);
    } catch (e) {
      logError("writing_assist", e);
      statusEl.textContent = String(e);
    } finally {
      applyBtn.disabled = false;
    }
  }

  el.querySelectorAll("[data-st-wact]").forEach(btn => {
    btn.addEventListener("click", () => runInstruction(WRITING_ACTIONS[parseInt(btn.dataset.stWact, 10)].instr));
  });
  applyBtn.addEventListener("click", () => runInstruction(instrEl.value));
  instrEl.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); runInstruction(instrEl.value); } });
  undoBtn.addEventListener("click", () => {
    if (prevDraft !== null) { draftEl.value = prevDraft; prevDraft = null; undoBtn.style.display = "none"; }
  });
  copyBtn.addEventListener("click", async () => {
    try { await navigator.clipboard.writeText(draftEl.value); copyBtn.textContent = "Copied ✓"; setTimeout(() => { copyBtn.textContent = "Copy all"; }, 1200); }
    catch { copyBtn.textContent = "Copy failed"; }
  });
}

// ---------- DOCS section ----------
function renderDocsSection(el, id) {
  el.innerHTML = `
    <div class="studio-seg" data-st-doctype="${id}">
      <button class="studio-seg-btn active" data-st-dt="slides">Slide deck</button>
      <button class="studio-seg-btn" data-st-dt="document">Document</button>
    </div>
    <textarea class="studio-topic" rows="3" placeholder="What should the document cover? (topic, outline, key points)" data-st-doc-topic="${id}"></textarea>
    <div class="form-row" style="gap:6px; align-items:center;">
      <label class="muted" style="font-size:11px;">Slides <input type="number" min="3" max="20" value="7" data-st-doc-slides="${id}" style="width:60px; margin-left:6px;" /></label>
      <button class="form-btn" data-st-doc-gen="${id}">Generate</button>
      <span class="muted" data-st-doc-status="${id}" style="font-size:10px;"></span>
    </div>
    <div class="studio-doc-result" data-st-doc-result="${id}" style="display:none;">
      <div class="studio-actions" style="margin-bottom:6px;">
        <button class="form-btn" data-st-doc-open="${id}">Open in browser → ⌘P → Save as PDF</button>
        <button class="hint-chip" data-st-doc-copy="${id}">Copy HTML</button>
      </div>
      <iframe class="studio-doc-frame" data-st-doc-frame="${id}" sandbox="allow-same-origin"></iframe>
    </div>
  `;

  const segEl = el.querySelector(`[data-st-doctype="${id}"]`);
  const topicEl = el.querySelector(`[data-st-doc-topic="${id}"]`);
  const slidesEl = el.querySelector(`[data-st-doc-slides="${id}"]`);
  const genBtn = el.querySelector(`[data-st-doc-gen="${id}"]`);
  const statusEl = el.querySelector(`[data-st-doc-status="${id}"]`);
  const resultEl = el.querySelector(`[data-st-doc-result="${id}"]`);
  const frameEl = el.querySelector(`[data-st-doc-frame="${id}"]`);
  const openBtn = el.querySelector(`[data-st-doc-open="${id}"]`);
  const copyBtn = el.querySelector(`[data-st-doc-copy="${id}"]`);

  let docType = "slides";
  let html = "";

  segEl.querySelectorAll(".studio-seg-btn").forEach(btn => {
    btn.addEventListener("click", () => {
      docType = btn.dataset.stDt;
      segEl.querySelectorAll(".studio-seg-btn").forEach(b => b.classList.toggle("active", b === btn));
      slidesEl.parentElement.style.display = docType === "slides" ? "" : "none";
    });
  });

  genBtn.addEventListener("click", async () => {
    const topic = topicEl.value.trim();
    if (!topic) { statusEl.textContent = "Describe the document."; return; }
    genBtn.disabled = true;
    statusEl.textContent = "Generating… (this can take a bit)";
    try {
      html = await invoke("generate_doc_html", {
        req: { docType, topic, slides: parseInt(slidesEl.value, 10) || 7 }
      });
      frameEl.srcdoc = html;
      resultEl.style.display = "";
      statusEl.textContent = "";
    } catch (e) {
      logError("generate_doc_html", e);
      statusEl.textContent = String(e);
    } finally {
      genBtn.disabled = false;
    }
  });

  openBtn.addEventListener("click", async () => {
    if (!html) return;
    try {
      await invoke("open_html", { html, title: topicEl.value.trim().slice(0, 60) || "studio-doc" });
    } catch (e) {
      logError("open_html", e);
      statusEl.textContent = String(e);
    }
  });

  copyBtn.addEventListener("click", async () => {
    if (!html) return;
    try { await navigator.clipboard.writeText(html); copyBtn.textContent = "Copied ✓"; setTimeout(() => { copyBtn.textContent = "Copy HTML"; }, 1200); }
    catch { copyBtn.textContent = "Copy failed"; }
  });
}

// ---------- VIDEO section ----------
// AI video editor: pick a file → analyze (silences + timestamped transcript) → build an
// edit plan (by hand or from a natural-language instruction) → render via ffmpeg.

const VIDEO_ASPECTS = [
  ["", "Keep aspect"],
  ["9:16", "9:16 vertical"],
  ["1:1", "1:1 square"],
  ["4:5", "4:5 portrait"],
  ["16:9", "16:9 wide"],
];

const CAPTION_STYLES = [
  ["boxed", "Boxed"],
  ["outline", "Outline"],
  ["karaoke", "Karaoke"],
];

// One progress listener per tile; re-rendering the section replaces it.
const videoUnlisteners = new Map();

function fmtT(s) {
  return (Math.round(s * 100) / 100).toFixed(2);
}

// Extensions that count as still-image inserts (title/intro cards). Mirrors is_image_source
// in video.rs.
const INSERT_IMAGE_EXTS = ["png", "jpg", "jpeg", "webp"];
function isImageInsertPath(p) {
  const ext = (p || "").split(".").pop().toLowerCase();
  return INSERT_IMAGE_EXTS.includes(ext);
}

// The preview edits only MAIN segments (drag/cut/toggle); this re-weaves the existing inserts
// back into the new main list so their relative position (after the Nth main segment) survives.
// Each insert is anchored to the count of main segments that preceded it in the old array.
function mergeMainWithInserts(newMains, oldSegments) {
  const inserts = [];
  let mainCount = 0;
  for (const s of oldSegments || []) {
    if (s.source) inserts.push({ seg: s, anchor: mainCount });
    else mainCount++;
  }
  if (!inserts.length) return newMains.slice();
  const placed = new Array(inserts.length).fill(false);
  const result = [];
  const flushAt = (n) => inserts.forEach((ins, i) => {
    if (!placed[i] && ins.anchor === n) { result.push(ins.seg); placed[i] = true; }
  });
  flushAt(0);
  newMains.forEach((m, idx) => { result.push(m); flushAt(idx + 1); });
  inserts.forEach((ins, i) => { if (!placed[i]) result.push(ins.seg); }); // anchor past the end
  return result;
}

function renderVideoSection(el, id) {
  el.innerHTML = `
    <div class="form-row" style="gap:6px; align-items:center; flex-wrap:wrap;">
      <button class="form-btn" data-vd-pick="${id}">Choose video…</button>
      <span class="muted" data-vd-info="${id}" style="font-size:11px;"></span>
    </div>
    <div class="studio-filmstrip" data-vd-strip="${id}"></div>

    <div class="vp-mount" data-vd-preview="${id}" style="display:none;"></div>

    <div class="form-row" style="gap:6px; align-items:center;">
      <button class="form-btn" data-vd-analyze="${id}" disabled>Analyze speech &amp; silence</button>
      <span class="muted" data-vd-an-status="${id}" style="font-size:10px;"></span>
    </div>
    <div class="studio-transcript" data-vd-transcript="${id}" style="display:none;"></div>

    <h4 class="studio-h" style="margin-top:8px;">Edit</h4>
    <div class="form-row" style="gap:6px;">
      <textarea class="studio-topic" rows="2" style="flex:1;"
        placeholder="Describe the edit (e.g. 'keep the best 30s, cut the dead air, add a title, make it punchy for TikTok')"
        data-vd-instr="${id}"></textarea>
      <button class="form-btn" data-vd-plan="${id}" disabled>Plan with AI</button>
    </div>

    <div class="studio-plan" data-vd-planbox="${id}"></div>

    <div class="form-row" style="gap:6px; align-items:center; margin-top:4px;">
      <button class="form-btn" data-vd-render="${id}" disabled>Render</button>
      <button class="hint-chip" data-vd-cancel="${id}" style="display:none;">Cancel</button>
      <div class="studio-progress" data-vd-progwrap="${id}" style="display:none;">
        <div class="studio-progress-bar" data-vd-prog="${id}"></div>
      </div>
      <span class="muted" data-vd-status="${id}" style="font-size:10px;"></span>
    </div>
    <div class="studio-video-out" data-vd-out="${id}"></div>
  `;

  const pickBtn = el.querySelector(`[data-vd-pick="${id}"]`);
  const infoEl = el.querySelector(`[data-vd-info="${id}"]`);
  const stripEl = el.querySelector(`[data-vd-strip="${id}"]`);
  const previewMount = el.querySelector(`[data-vd-preview="${id}"]`);
  const analyzeBtn = el.querySelector(`[data-vd-analyze="${id}"]`);
  const anStatusEl = el.querySelector(`[data-vd-an-status="${id}"]`);
  const transcriptEl = el.querySelector(`[data-vd-transcript="${id}"]`);
  const instrEl = el.querySelector(`[data-vd-instr="${id}"]`);
  const planBtn = el.querySelector(`[data-vd-plan="${id}"]`);
  const planBox = el.querySelector(`[data-vd-planbox="${id}"]`);
  const renderBtn = el.querySelector(`[data-vd-render="${id}"]`);
  const cancelBtn = el.querySelector(`[data-vd-cancel="${id}"]`);
  const progWrap = el.querySelector(`[data-vd-progwrap="${id}"]`);
  const progBar = el.querySelector(`[data-vd-prog="${id}"]`);
  const statusEl = el.querySelector(`[data-vd-status="${id}"]`);
  const outEl = el.querySelector(`[data-vd-out="${id}"]`);

  let srcPath = null;
  let probe = null;
  let analysis = null;
  let thumbs = [];
  let currentJobId = null;
  // Default shape for `plan` — used to reset on a new file pick, to apply an AI/saved plan
  // on top of, and to detect whether the user has made any edit yet (drives refinement mode
  // and the "restored previous edit" note). A fresh object each call so callers get their own
  // arrays instead of sharing references with a previous plan.
  function defaultPlanFields() {
    return {
      segments: [], filter: "none", texts: [], aspect: "", speed: 1, mute: false,
      captions: false, captionStyle: "boxed",
      normalizeAudio: false, denoise: false, fade: null, output_name: null,
      smartReframe: false,
    };
  }
  // The single source of truth for what Render produces. "Plan with AI" fills it;
  // every control below edits it.
  const plan = defaultPlanFields();
  function resetPlanToDefault() { Object.assign(plan, defaultPlanFields()); }
  // True once the plan has any AI/manual edit — drives "Plan with AI" refinement mode (send
  // the current plan so the model mutates it instead of starting over) and the instruction
  // box's placeholder hint.
  function isDefaultPlan() {
    const d = defaultPlanFields();
    return plan.segments.length === d.segments.length &&
      plan.filter === d.filter &&
      plan.texts.length === d.texts.length &&
      plan.aspect === d.aspect &&
      plan.speed === d.speed &&
      plan.mute === d.mute &&
      plan.captions === d.captions &&
      plan.normalizeAudio === d.normalizeAudio &&
      plan.denoise === d.denoise &&
      plan.fade === d.fade &&
      plan.smartReframe === d.smartReframe &&
      !plan.output_name;
  }
  // Copy a plan-shaped object (an AI result or a saved plan — both use EditPlan's snake_case
  // fields) onto the shared `plan`, defaulting anything missing. Shared by "Plan with AI" and
  // the saved-plan restore on file pick so the two paths can't drift apart.
  function applyPlanFields(src) {
    const d = defaultPlanFields();
    plan.segments = src.segments || d.segments;
    plan.filter = src.filter || d.filter;
    plan.texts = src.texts || d.texts;
    plan.aspect = src.aspect || d.aspect;
    plan.speed = src.speed || d.speed;
    plan.mute = !!src.mute;
    plan.captions = !!src.captions;
    plan.captionStyle = src.caption_style || d.captionStyle;
    plan.normalizeAudio = !!src.normalize_audio;
    plan.denoise = !!src.denoise;
    plan.fade = src.fade || d.fade;
    plan.smartReframe = !!src.smart_reframe;
    plan.output_name = src.output_name || d.output_name;
  }
  // UI-only toggle for the "Cut filler words" checkbox — not part of the render payload.
  let cutFillers = false;
  let presets = [{ key: "none", label: "None" }];
  invoke("video_filter_presets").then(p => { presets = p; drawPlan(); }).catch(() => {});

  // Autosave: debounced so dragging a slider or typing doesn't spam saves. Every plan
  // mutation path (drawPlan()'s many callers, plus the handful of controls that only call
  // refreshPreview()) calls this — it's the one place that actually talks to the DB.
  let saveTimer = null;
  function scheduleAutosave() {
    if (!srcPath) return;
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      invoke("video_save_plan", { req: { path: srcPath, plan: planPayload() } }).catch(() => {});
    }, 800);
  }

  // Instant edit preview (player + timeline + interactive transcript). Edits made inside it
  // (marker toggles, edge drags, transcript cuts) come back through onPlanChange, which mutates
  // the shared `plan` and re-runs drawPlan() so the checkbox/pill UI and the preview stay in sync.
  const preview = createVideoPreview(previewMount, {
    onPlanChange: (patch) => {
      // The preview sends back MAIN segments only; re-weave any inserts so they keep their slot.
      if (patch && Array.isArray(patch.segments)) {
        plan.segments = mergeMainWithInserts(patch.segments, plan.segments);
      }
      drawPlan(); // drawPlan() calls refreshPreview() at its end, so the preview redraws too
    },
  });
  function refreshPreview() {
    if (!srcPath || !probe) return;
    previewMount.style.display = "";
    preview.update({ srcPath, probe, plan, analysis, thumbs });
    // Every drawPlan() call ends here, and so do the handful of controls that mutate `plan`
    // without a full drawPlan() rebuild (filter/aspect/mute/caption style) — so this one spot
    // catches nearly every plan change for autosave. See scheduleAutosave() for the rest.
    scheduleAutosave();
  }
  // Tear the preview down when the tile closes or the Studio tab switches away (see selectTab).
  el._sectionCleanup = () => { try { preview.cleanup(); } catch {} };
  const tileObj = tiles.find(t => t.id === id);
  if (tileObj) {
    const prevCleanup = tileObj.cleanup;
    tileObj.cleanup = () => { try { preview.cleanup(); } catch {} if (typeof prevCleanup === "function") { try { prevCleanup(); } catch {} } };
  }

  // Progress events for this tile's renders.
  const prev = videoUnlisteners.get(id);
  if (prev) prev.then(un => un()).catch(() => {});
  videoUnlisteners.set(id, listen("video-render-progress", (e) => {
    const p = e.payload || {};
    if (p.job_id !== currentJobId) return;
    progBar.style.width = `${Math.max(1, p.pct || 0)}%`;
    statusEl.textContent = `Rendering… ${Math.floor(p.pct || 0)}%`;
  }));

  pickBtn.addEventListener("click", async () => {
    let selected;
    try {
      selected = await openDialog({
        multiple: false,
        filters: [{ name: "Video", extensions: ["mp4", "mov", "m4v", "avi", "mkv", "webm"] }],
      });
    } catch (e) { logError("video pick", e); return; }
    if (!selected) return;
    srcPath = typeof selected === "string" ? selected : selected.path;
    probe = null;
    analysis = null;
    thumbs = [];
    resetPlanToDefault();
    cutFillers = false;
    transcriptEl.style.display = "none";
    previewMount.style.display = "none";
    outEl.innerHTML = "";
    stripEl.innerHTML = "";
    statusEl.textContent = "";
    infoEl.textContent = "Reading file…";
    try {
      probe = await invoke("video_probe", { path: srcPath });
      const name = srcPath.split("/").pop();
      const mins = Math.floor(probe.duration_s / 60);
      const secs = Math.round(probe.duration_s % 60);
      infoEl.textContent = `${name} — ${mins}:${String(secs).padStart(2, "0")}, ${probe.width}×${probe.height}, ${probe.fps} fps${probe.has_audio ? "" : ", no audio"}`;
      analyzeBtn.disabled = !probe.has_audio;
      anStatusEl.textContent = probe.has_audio ? "" : "No audio track — silence/transcript analysis unavailable.";
      planBtn.disabled = false;
      renderBtn.disabled = false;
      // Restore a previously saved edit for this exact file, if any — "Keep everything"
      // below resets back to the default plan.
      try {
        const savedPlan = await invoke("video_load_plan", { req: { path: srcPath } });
        if (savedPlan) {
          applyPlanFields(savedPlan);
          statusEl.textContent = "Restored previous edit — \"Keep everything\" resets.";
        }
      } catch (e) {
        logError("video_load_plan", e);
      }
      drawPlan();
      invoke("video_thumbnails", { path: srcPath, count: 8 }).then(t => {
        thumbs = t || [];
        stripEl.innerHTML = thumbs.map(u => `<img src="${u}" alt="frame" />`).join("");
        refreshPreview(); // fill the timeline strip once frames arrive
      }).catch(() => {});
    } catch (e) {
      logError("video_probe", e);
      infoEl.textContent = String(e);
      srcPath = null;
    }
  });

  analyzeBtn.addEventListener("click", async () => {
    if (!srcPath) return;
    analyzeBtn.disabled = true;
    anStatusEl.textContent = "Analyzing… (detecting silence, transcribing speech)";
    try {
      analysis = await invoke("video_analyze", { req: { path: srcPath } });
      const parts = [];
      if (analysis.silences.length) {
        parts.push(`<div class="muted" style="font-size:10px;">${analysis.silences.length} silence${analysis.silences.length === 1 ? "" : "s"} detected — "Cut silences" below removes them.</div>`);
      }
      if (analysis.filler_ranges?.length) {
        parts.push(`<div class="muted" style="font-size:10px;">${analysis.filler_ranges.length} filler word${analysis.filler_ranges.length === 1 ? "" : "s"}/stutter${analysis.filler_ranges.length === 1 ? "" : "s"} detected — "Cut filler words" below removes them.</div>`);
      }
      if (analysis.transcript && analysis.transcript.segments.length) {
        parts.push(analysis.transcript.segments.map(s =>
          `<div class="studio-tr-line"><span class="studio-tr-time">${fmtT(s.start)}–${fmtT(s.end)}</span> ${escapeHtml(s.text)}</div>`
        ).join(""));
      } else if (analysis.transcript_error) {
        parts.push(`<div class="muted" style="font-size:10px;">Transcript unavailable: ${escapeHtml(analysis.transcript_error)}</div>`);
      } else {
        parts.push(`<div class="muted" style="font-size:10px;">No speech found.</div>`);
      }
      transcriptEl.innerHTML = parts.join("");
      transcriptEl.style.display = "";
      anStatusEl.textContent = "";
      drawPlan();
    } catch (e) {
      logError("video_analyze", e);
      anStatusEl.textContent = String(e);
    } finally {
      analyzeBtn.disabled = false;
    }
  });

  planBtn.addEventListener("click", async () => {
    if (!srcPath) return;
    const instruction = instrEl.value.trim();
    if (!instruction) { statusEl.textContent = "Describe the edit first."; return; }
    planBtn.disabled = true;
    statusEl.textContent = "Planning edit…";
    // Any edit so far (a prior AI plan or manual tweaks) is sent along so the model refines
    // it in place instead of starting from a blank plan — see isDefaultPlan()/applyPlanFields.
    const refining = !isDefaultPlan();
    try {
      const aiPlan = await invoke("video_plan_edit", {
        req: {
          path: srcPath,
          instruction,
          transcript: analysis?.transcript || null,
          silences: analysis?.silences || null,
          fillerRanges: analysis?.filler_ranges || null,
          currentPlan: refining ? planPayload() : null,
        },
      });
      applyPlanFields(aiPlan);
      statusEl.textContent = "Plan ready — review below, then Render.";
      drawPlan();
    } catch (e) {
      logError("video_plan_edit", e);
      statusEl.textContent = String(e);
    } finally {
      planBtn.disabled = false;
    }
  });

  function drawPlan() {
    // Once there's anything to refine (a prior AI plan or a manual tweak), hint that the next
    // instruction refines rather than replaces — matches the currentPlan wiring in planBtn.
    instrEl.placeholder = isDefaultPlan()
      ? "Describe the edit (e.g. 'keep the best 30s, cut the dead air, add a title, make it punchy for TikTok')"
      : "Refine the current edit… (e.g. 'make the captions karaoke style and speed it up a bit')";
    const dur = probe ? probe.duration_s : 0;
    const hasTranscript = !!(analysis && analysis.transcript &&
      (analysis.transcript.words?.length || analysis.transcript.segments?.length));
    if (!hasTranscript) plan.captions = false;
    const fillerCount = analysis?.filler_ranges?.length || 0;
    if (!fillerCount) cutFillers = false;
    const kept = plan.segments.length
      ? plan.segments.reduce((a, s) => a + (s.end - s.start), 0)
      : dur;
    const outDur = kept / (plan.speed || 1);
    planBox.innerHTML = `
      <div class="form-row" style="gap:6px; align-items:center; flex-wrap:wrap;">
        <button class="hint-chip" data-vd-cutsil>Cut silences</button>
        <button class="hint-chip" data-vd-clearsegs>Keep everything</button>
        <button class="hint-chip" data-vd-addinsert title="Splice in a b-roll clip or a still-image title card after the current spot">Insert clip/image…</button>
        <label class="studio-chip" title="${fillerCount ? "Cut detected filler words (um, uh, ...) and repeated-word stutters" : "Analyze speech first to find filler words"}">
          <input type="checkbox" data-vd-fillers ${cutFillers ? "checked" : ""} ${fillerCount ? "" : "disabled"} /> Cut filler words (${fillerCount} found)
        </label>
        <span class="muted" style="font-size:10px;">
          ${plan.segments.length ? `${plan.segments.length} segment${plan.segments.length === 1 ? "" : "s"} kept` : "keeping the whole video"}${probe ? ` · output ≈ ${fmtT(outDur)}s` : ""}
        </span>
      </div>
      <div class="studio-segs" data-vd-segs></div>
      <div class="form-row" style="gap:6px; align-items:center; flex-wrap:wrap;">
        <select data-vd-filter style="width:auto;">
          ${presets.map(p => `<option value="${escapeHtml(p.key)}" ${p.key === plan.filter ? "selected" : ""}>${escapeHtml(p.label)}</option>`).join("")}
        </select>
        <select data-vd-aspect style="width:auto;">
          ${VIDEO_ASPECTS.map(([v, l]) => `<option value="${v}" ${v === plan.aspect ? "selected" : ""}>${l}</option>`).join("")}
        </select>
        <label class="studio-chip" title="${plan.aspect ? "Track faces so speakers stay in frame instead of a static center-crop (only when cropping to a narrower aspect)" : "Choose an aspect ratio first"}">
          <input type="checkbox" data-vd-reframe ${plan.smartReframe ? "checked" : ""} ${plan.aspect ? "" : "disabled"} /> Keep subject in frame
        </label>
        <label class="muted" style="font-size:11px;">Speed
          <input type="number" min="0.25" max="4" step="0.25" value="${plan.speed}" data-vd-speed style="width:64px; margin-left:4px;" />
        </label>
        <label class="studio-chip"><input type="checkbox" data-vd-mute ${plan.mute ? "checked" : ""} /> Mute</label>
      </div>
      <div class="form-row" style="gap:6px; align-items:center; flex-wrap:wrap;">
        <label class="studio-chip" title="${hasTranscript ? "Burn word-timed subtitles from the transcript" : "Analyze speech first to enable captions"}">
          <input type="checkbox" data-vd-captions ${plan.captions ? "checked" : ""} ${hasTranscript ? "" : "disabled"} /> Burn captions
        </label>
        <select data-vd-capstyle style="width:auto;" ${hasTranscript && plan.captions ? "" : "disabled"}>
          ${CAPTION_STYLES.map(([v, l]) => `<option value="${v}" ${v === plan.captionStyle ? "selected" : ""}>${l}</option>`).join("")}
        </select>
        <button class="hint-chip" data-vd-exportsrt ${hasTranscript ? "" : "disabled"}>Export SRT</button>
        ${hasTranscript ? "" : `<span class="muted" style="font-size:10px;">Analyze speech to enable captions.</span>`}
      </div>
      <div class="form-row" style="gap:6px; align-items:center; flex-wrap:wrap;">
        <label class="studio-chip" title="Loudness-normalize the mix for publishable audio (EBU R128, -14 LUFS)">
          <input type="checkbox" data-vd-normalize ${plan.normalizeAudio ? "checked" : ""} /> Normalize loudness
        </label>
        <label class="studio-chip" title="Reduce background hiss/hum/fan noise">
          <input type="checkbox" data-vd-denoise ${plan.denoise ? "checked" : ""} /> Reduce noise
        </label>
        <label class="muted" style="font-size:11px;" title="Fade in/out at the start and end of the clip (video + audio)">Fade (s)
          <input type="number" min="0.1" max="5" step="0.1" placeholder="none" value="${plan.fade ?? ""}" data-vd-fade style="width:64px; margin-left:4px;" />
        </label>
      </div>
      <div class="studio-texts" data-vd-texts></div>
      <div class="form-row" style="gap:6px; align-items:center; flex-wrap:wrap;">
        <input type="text" placeholder="Overlay text (optional)" data-vd-newtext style="flex:1; min-width:140px;" />
        <input type="number" min="0" step="0.5" placeholder="start s" data-vd-newstart style="width:70px;" />
        <input type="number" min="0" step="0.5" placeholder="end s" data-vd-newend style="width:70px;" />
        <select data-vd-newpos style="width:auto;">
          <option value="bottom">Bottom</option><option value="center">Center</option><option value="top">Top</option>
        </select>
        <button class="hint-chip" data-vd-addtext>Add text</button>
      </div>
    `;

    const segsEl = planBox.querySelector("[data-vd-segs]");
    segsEl.innerHTML = plan.segments.map((s, i) => {
      if (s.source) {
        const name = s.source.split("/").pop();
        return `<span class="studio-seg-pill studio-seg-insert" title="${escapeHtml(s.source)}">insert: ${escapeHtml(name)} (${fmtT(s.end - s.start)}s) <button data-vd-rmseg="${i}" title="Remove">×</button></span>`;
      }
      return `<span class="studio-seg-pill">${fmtT(s.start)}–${fmtT(s.end)}s <button data-vd-rmseg="${i}" title="Remove">×</button></span>`;
    }).join("");
    segsEl.querySelectorAll("[data-vd-rmseg]").forEach(btn => {
      btn.addEventListener("click", () => { plan.segments.splice(parseInt(btn.dataset.vdRmseg, 10), 1); drawPlan(); });
    });

    const textsEl = planBox.querySelector("[data-vd-texts]");
    textsEl.innerHTML = plan.texts.map((t, i) =>
      `<span class="studio-seg-pill">"${escapeHtml(t.text)}" ${fmtT(t.start)}–${fmtT(t.end)}s (${escapeHtml(t.position || "bottom")}) <button data-vd-rmtext="${i}" title="Remove">×</button></span>`
    ).join("");
    textsEl.querySelectorAll("[data-vd-rmtext]").forEach(btn => {
      btn.addEventListener("click", () => { plan.texts.splice(parseInt(btn.dataset.vdRmtext, 10), 1); drawPlan(); });
    });

    planBox.querySelector("[data-vd-cutsil]").addEventListener("click", () => {
      if (!analysis || !analysis.suggested_segments?.length) {
        statusEl.textContent = analysis ? "No silences to cut." : "Run Analyze first.";
        return;
      }
      plan.segments = analysis.suggested_segments.map(s => ({ start: s.start, end: s.end }));
      cutFillers = false;
      drawPlan();
    });
    planBox.querySelector("[data-vd-clearsegs]").addEventListener("click", () => { plan.segments = []; cutFillers = false; drawPlan(); });
    planBox.querySelector("[data-vd-addinsert]").addEventListener("click", addInsert);
    const fillersBox = planBox.querySelector("[data-vd-fillers]");
    if (fillersBox) {
      fillersBox.addEventListener("change", (e) => {
        cutFillers = e.target.checked;
        // Same one-shot "apply a suggestion" pattern as "Cut silences" above: checking
        // this replaces plan.segments with the combined silence+filler keep-segments
        // (which reduces to filler-only cuts when no silences were detected).
        if (cutFillers && analysis?.filler_keep_segments?.length) {
          plan.segments = analysis.filler_keep_segments.map(s => ({ start: s.start, end: s.end }));
        }
        drawPlan();
      });
    }
    // These edit `plan` in place without a full drawPlan() rebuild, so refresh the preview
    // directly (filter/aspect are visible in the player; mute/caption-style feed it too).
    planBox.querySelector("[data-vd-filter]").addEventListener("change", (e) => { plan.filter = e.target.value; refreshPreview(); });
    planBox.querySelector("[data-vd-aspect]").addEventListener("change", (e) => {
      plan.aspect = e.target.value;
      // "Keep subject in frame" only makes sense with an aspect chosen — toggle its enabled
      // state without a full rebuild, and clear it when the aspect is removed.
      const rf = planBox.querySelector("[data-vd-reframe]");
      if (rf) {
        rf.disabled = !plan.aspect;
        if (!plan.aspect) { rf.checked = false; plan.smartReframe = false; }
      }
      refreshPreview();
    });
    planBox.querySelector("[data-vd-reframe]").addEventListener("change", (e) => { plan.smartReframe = e.target.checked; scheduleAutosave(); });
    planBox.querySelector("[data-vd-speed]").addEventListener("change", (e) => { plan.speed = parseFloat(e.target.value) || 1; drawPlan(); });
    planBox.querySelector("[data-vd-mute]").addEventListener("change", (e) => { plan.mute = e.target.checked; refreshPreview(); });
    planBox.querySelector("[data-vd-captions]").addEventListener("change", (e) => { plan.captions = e.target.checked; drawPlan(); });
    planBox.querySelector("[data-vd-capstyle]").addEventListener("change", (e) => { plan.captionStyle = e.target.value; refreshPreview(); });
    planBox.querySelector("[data-vd-normalize]").addEventListener("change", (e) => { plan.normalizeAudio = e.target.checked; scheduleAutosave(); });
    planBox.querySelector("[data-vd-denoise]").addEventListener("change", (e) => { plan.denoise = e.target.checked; scheduleAutosave(); });
    planBox.querySelector("[data-vd-fade]").addEventListener("change", (e) => {
      const v = parseFloat(e.target.value);
      plan.fade = isNaN(v) ? null : Math.min(5, Math.max(0.1, v));
      scheduleAutosave();
    });
    planBox.querySelector("[data-vd-exportsrt]").addEventListener("click", exportSrt);
    planBox.querySelector("[data-vd-addtext]").addEventListener("click", () => {
      const text = planBox.querySelector("[data-vd-newtext]").value.trim();
      const start = parseFloat(planBox.querySelector("[data-vd-newstart]").value);
      const end = parseFloat(planBox.querySelector("[data-vd-newend]").value);
      if (!text || isNaN(start) || isNaN(end) || end <= start) {
        statusEl.textContent = "Text overlays need text plus valid start/end seconds.";
        return;
      }
      plan.texts.push({ text, start, end, position: planBox.querySelector("[data-vd-newpos]").value });
      drawPlan();
    });

    // Every plan mutation funnels through drawPlan(), so refreshing the preview here means one
    // code path keeps the player/timeline/transcript in lockstep with the plan.
    refreshPreview();
  }
  drawPlan();

  // Pick a b-roll clip or still-image card and splice it in AFTER the main kept segment the
  // preview playhead sits in (else at the end). Images default to a 3s card; videos default to
  // their full length capped at 5s.
  async function addInsert() {
    if (!srcPath) { statusEl.textContent = "Choose a main video first."; return; }
    let selected;
    try {
      selected = await openDialog({
        multiple: false,
        filters: [{ name: "Clip or image", extensions: ["mp4", "mov", "m4v", "avi", "mkv", "webm", "png", "jpg", "jpeg", "webp"] }],
      });
    } catch (e) { logError("insert pick", e); return; }
    if (!selected) return;
    const path = typeof selected === "string" ? selected : selected.path;

    let seg;
    if (isImageInsertPath(path)) {
      seg = { start: 0, end: 3, source: path }; // 3s title/intro card
    } else {
      let dur = 5;
      try {
        const ip = await invoke("video_probe", { path });
        if (ip && ip.duration_s > 0) dur = Math.min(5, ip.duration_s);
      } catch (e) { logError("insert probe", e); }
      seg = { start: 0, end: dur > 0 ? dur : 5, source: path };
    }

    // An empty plan means "keep the whole main video"; once we add an insert the segment list
    // becomes explicit, so materialize the whole main range first or the backend would render
    // only the insert and drop the main clip.
    if (!plan.segments.length && probe) {
      plan.segments = [{ start: 0, end: probe.duration_s }];
    }
    const t = preview.currentTime ? preview.currentTime() : 0;
    let idx = plan.segments.length; // default: append at the end
    for (let i = 0; i < plan.segments.length; i++) {
      const s = plan.segments[i];
      if (!s.source && t >= s.start && t < s.end) { idx = i + 1; break; }
    }
    plan.segments.splice(idx, 0, seg);
    statusEl.textContent = `Inserted ${path.split("/").pop()} — review below, then Render.`;
    drawPlan();
  }

  // Snake-case plan payload shared by Render and Export (EditPlan uses snake_case fields).
  function planPayload() {
    return {
      segments: plan.segments,
      filter: plan.filter === "none" ? null : plan.filter,
      texts: plan.texts,
      aspect: plan.aspect || null,
      speed: plan.speed === 1 ? null : plan.speed,
      mute: plan.mute,
      captions: plan.captions,
      caption_style: plan.captions ? plan.captionStyle : null,
      normalize_audio: plan.normalizeAudio,
      denoise: plan.denoise,
      fade: plan.fade || null,
      smart_reframe: plan.smartReframe,
      output_name: plan.output_name || null,
    };
  }

  async function exportSrt() {
    if (!srcPath) return;
    statusEl.textContent = "Exporting SRT…";
    try {
      const out = await invoke("video_export_subtitles", {
        req: {
          path: srcPath,
          transcript: analysis?.transcript || null,
          format: "srt",
          plan: planPayload(),
        },
      });
      statusEl.textContent = `Saved ${out.split("/").pop()} to Content ✓`;
    } catch (e) {
      logError("video_export_subtitles", e);
      statusEl.textContent = String(e);
    }
  }

  renderBtn.addEventListener("click", async () => {
    if (!srcPath) return;
    currentJobId = `studio-${id}-${Date.now()}`;
    renderBtn.disabled = true;
    cancelBtn.style.display = "";
    progWrap.style.display = "";
    progBar.style.width = "1%";
    statusEl.textContent = "Rendering…";
    outEl.innerHTML = "";
    try {
      const req = {
        path: srcPath,
        jobId: currentJobId,
        plan: planPayload(),
        transcript: analysis?.transcript || null,
      };
      const outPath = await invoke("video_render", { req });
      statusEl.textContent = "Saved to Content ✓";
      progBar.style.width = "100%";
      outEl.innerHTML = `<video controls src="${convertFileSrc(outPath)}" class="studio-video-preview"></video>`;
      // Save the plan that just rendered immediately, ahead of the debounced autosave.
      clearTimeout(saveTimer);
      invoke("video_save_plan", { req: { path: srcPath, plan: req.plan } }).catch(() => {});
    } catch (e) {
      logError("video_render", e);
      statusEl.textContent = String(e);
    } finally {
      renderBtn.disabled = false;
      cancelBtn.style.display = "none";
      setTimeout(() => { progWrap.style.display = "none"; }, 1500);
    }
  });

  cancelBtn.addEventListener("click", async () => {
    if (!currentJobId) return;
    try { await invoke("video_cancel_render", { jobId: currentJobId }); } catch {}
    statusEl.textContent = "Cancelled.";
  });
}

registerTileRenderer("studio", renderStudioTile);
