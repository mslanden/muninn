import { registerTileRenderer } from "./registry.js";
import { invoke } from "@tauri-apps/api/core";
import { escapeHtml } from "./utils.js";
import { logError } from "./uikit.js";
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
    video: (el) => renderComingSoon(el, "Video", "Generate short clips or assemble slideshow videos (images + voiceover). Coming later."),
  };
  function selectTab(key) {
    tabsEl.querySelectorAll(".studio-tab").forEach(b => b.classList.toggle("active", b.dataset.stTab === key));
    sectionEl.innerHTML = "";
    (renderers[key] || renderers.media)(sectionEl, id);
  }
  tabsEl.querySelectorAll(".studio-tab").forEach(btn => {
    btn.addEventListener("click", () => selectTab(btn.dataset.stTab));
  });

  selectTab("media");
}

function renderComingSoon(el, title, note) {
  el.innerHTML = `<div class="studio-soon"><h4 class="studio-h">${escapeHtml(title)}</h4><p class="muted">${escapeHtml(note)}</p></div>`;
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

registerTileRenderer("studio", renderStudioTile);
