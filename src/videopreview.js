// videopreview.js — instant, render-free preview for the Studio video editor.
//
// createVideoPreview(container, { onPlanChange }) builds a <video> player (fed the SOURCE
// file via convertFileSrc), a scrubbable timeline, and an interactive transcript, then
// *simulates* the current EditPlan on top of the untouched source: it skips cut ranges while
// playing, approximates the filter/aspect/speed/mute, and overlays live captions. Every plan
// mutation (from AI, checkboxes, the timeline, or the transcript) flows through the ONE
// `update({ srcPath, probe, plan, analysis, thumbs })` entry point so a single code path
// refreshes everything; edits made *inside* the preview are pushed back out via onPlanChange.
//
// TIME MODEL (important): every range here — plan.segments (keep-ranges), analysis.silences,
// analysis.filler_ranges, transcript words — is in SOURCE seconds. Because we play the
// untouched source file, video.currentTime is ALSO source seconds. playbackRate scales
// wall-clock speed but does NOT change what currentTime means, so cut ranges compare directly
// against video.currentTime with no output-timeline mapping. This is why speed ≠ 1 needs no
// special handling in the skip logic below.
//
// This is a plain module (imported by studio.js), NOT a tile type — no registry involvement.

import { convertFileSrc } from "@tauri-apps/api/core";
import { escapeHtml } from "./utils.js";

// Slack (seconds) at segment boundaries. Big enough to absorb the ~1 frame of jitter between
// setting video.currentTime and the decoder landing on a nearby keyframe, so we don't
// re-seek/stutter forever at a boundary; small enough to be imperceptible.
const EPS = 0.03;

// CSS-filter approximations of the ffmpeg FILTER_PRESETS keys (see video.rs FILTER_PRESETS).
// These only need to convey the *look*, not match ffmpeg exactly. Unknown keys → no filter.
const CSS_FILTERS = {
  none: "",
  bw: "grayscale(1) contrast(1.12)",
  cinematic: "contrast(1.12) saturate(0.92) brightness(0.98)",
  warm: "sepia(0.18) saturate(1.1) contrast(1.03)",
  cool: "saturate(1.05) contrast(1.05) hue-rotate(-12deg) brightness(1.02)",
  vintage: "sepia(0.42) saturate(0.85) contrast(0.95)",
  punchy: "saturate(1.35) contrast(1.1) brightness(1.03)",
};

// ---------- pure range helpers (source-time {start,end}) ----------

// Merge overlapping/adjacent ranges into a sorted, disjoint list. `join` closes hairline gaps
// so segments that merely touch (a.end === b.start) coalesce into one.
function mergeRanges(ranges, join = 0.001) {
  const sorted = (ranges || [])
    .filter(r => r && r.end > r.start)
    .map(r => ({ start: r.start, end: r.end }))
    .sort((a, b) => a.start - b.start);
  const out = [];
  for (const r of sorted) {
    const last = out[out.length - 1];
    if (last && r.start <= last.end + join) last.end = Math.max(last.end, r.end);
    else out.push({ ...r });
  }
  return out;
}

// The concrete keep-segments to play/draw. An empty plan.segments means "keep the whole clip",
// which we materialize to a single [0, duration] segment. Also clamps to [0, duration].
function materializeKept(segments, duration) {
  if (duration <= 0) return [];
  if (!segments || !segments.length) return [{ start: 0, end: duration }];
  return mergeRanges(segments)
    .map(s => ({ start: Math.max(0, s.start), end: Math.min(duration, s.end) }))
    .filter(s => s.end > s.start);
}

// Complement of the (sorted, disjoint) keep-segments within [0, duration] = the cut ranges.
function complement(kept, duration) {
  const out = [];
  let cursor = 0;
  for (const s of kept) {
    if (s.start > cursor) out.push({ start: cursor, end: s.start });
    cursor = Math.max(cursor, s.end);
  }
  if (cursor < duration) out.push({ start: cursor, end: duration });
  return out;
}

// Subtract [cs, ce] from keep-segments (i.e. cut that range out). Returns new keep-segments.
function subtractRange(kept, cs, ce) {
  if (ce <= cs) return kept.map(s => ({ ...s }));
  const out = [];
  for (const s of kept) {
    if (ce <= s.start || cs >= s.end) { out.push({ ...s }); continue; } // no overlap
    if (cs > s.start) out.push({ start: s.start, end: cs });            // remainder on the left
    if (ce < s.end) out.push({ start: ce, end: s.end });                // remainder on the right
  }
  return out.filter(s => s.end - s.start > 0.001);
}

// Add [as, ae] back into keep-segments (restore a cut), clamped to [0, duration].
function addRange(kept, as, ae, duration) {
  const s = Math.max(0, as), e = Math.min(duration, ae);
  if (e <= s) return kept.map(x => ({ ...x }));
  return mergeRanges([...kept, { start: s, end: e }]);
}

// Is source-time t inside any kept segment? Boundary slack (EPS) keeps a word/marker sitting
// exactly on an edge from flickering between kept/cut.
function isKept(kept, t) {
  return kept.some(s => t >= s.start - EPS && t < s.end - EPS);
}

// Decide what playback should do at source-time t to honor the cuts. `kept` must be sorted &
// disjoint (materializeKept guarantees this).
//   { action: "play" }        — t is inside a kept segment, keep going
//   { action: "seek", to }    — t is in a cut gap, jump forward to the next kept start
//   { action: "end" }         — t is past the last kept segment, stop
function playbackTarget(kept, t) {
  for (const seg of kept) {
    if (t < seg.start - EPS) return { action: "seek", to: seg.start }; // in the gap before seg
    if (t < seg.end - EPS) return { action: "play" };                  // inside seg
    // else t >= seg.end - EPS: past this segment, fall through to the next
  }
  return { action: "end" };
}

// Group transcript words into short caption lines for the overlay — a preview approximation of
// the backend's ASS grouping (does not need to match it). Breaks a line at ~4 words, ~28 chars,
// or a >0.8s gap. Words whose midpoint lands inside a cut are dropped so captions track edits.
function buildCaptionLines(words, kept, duration) {
  const lines = [];
  let cur = null;
  const keptWhole = kept.length === 1 && kept[0].start <= EPS && kept[0].end >= duration - EPS;
  for (const w of words) {
    const text = (w.word || "").trim();
    if (!text) continue;
    const mid = (w.start + w.end) / 2;
    if (!keptWhole && !isKept(kept, mid)) { cur = null; continue; }
    const gap = cur ? w.start - cur.words[cur.words.length - 1].end : 0;
    const projected = cur ? cur.text.length + 1 + text.length : text.length;
    if (!cur || cur.words.length >= 4 || projected > 28 || gap > 0.8) {
      cur = { start: w.start, end: w.end, text, words: [{ start: w.start, end: w.end, text }] };
      lines.push(cur);
    } else {
      cur.words.push({ start: w.start, end: w.end, text });
      cur.end = w.end;
      cur.text += " " + text;
    }
  }
  return lines;
}

function fmtClock(s) {
  if (!isFinite(s) || s < 0) s = 0;
  const m = Math.floor(s / 60), sec = Math.floor(s % 60);
  return `${m}:${String(sec).padStart(2, "0")}`;
}
function fmt1(s) { return (Math.round(s * 10) / 10).toFixed(1); }

export function createVideoPreview(container, callbacks = {}) {
  const onPlanChange = callbacks.onPlanChange || (() => {});

  // ----- state -----
  let srcPath = null, probe = null, plan = {}, analysis = null, thumbs = [];
  let duration = 0;
  let kept = [];   // materialized keep-segments (source seconds)
  let cuts = [];   // complement of kept = cut ranges
  let captionLines = [];
  let rafId = null;
  let seeking = false;   // pointer is scrubbing the timeline
  let drag = null;       // active handle-drag state: { edge, i, segs }
  let capKey = "";       // cache key so the caption DOM only rebuilds when the line/word changes
  let curWordIdx = -1;   // transcript word currently under the playhead
  let hiEl = null;       // that word's span (for the "now" highlight)
  // transcript selection
  let selAnchor = null, selRange = null, dragSelecting = false;

  // ----- persistent DOM -----
  // Built ONCE. update() never rebuilds the <video>, so switching the plan never reloads the
  // media or drops the playback position. Only the timeline/transcript inner content is redrawn.
  container.classList.add("vp-root");
  container.innerHTML = `
    <div class="vp-stage" data-vp-stage>
      <video class="vp-video" data-vp-video playsinline preload="auto"></video>
      <div class="vp-caption" data-vp-caption></div>
      <div class="vp-caveat">preview ≈ approximation — the final render differs</div>
    </div>
    <div class="vp-controls">
      <button class="vp-play" data-vp-play type="button" aria-label="Play/pause">&#9654;</button>
      <span class="vp-time" data-vp-time>0:00 / 0:00</span>
    </div>
    <div class="vp-timeline" data-vp-timeline>
      <div class="vp-tl-layers" data-vp-layers></div>
      <div class="vp-tl-playhead" data-vp-playhead></div>
    </div>
    <div class="vp-legend">
      <span class="vp-lg vp-lg-cut">cut</span>
      <span class="vp-lg vp-lg-sil">silence</span>
      <span class="vp-lg vp-lg-fill">filler</span>
      <span class="vp-lg-hint">click a marker to toggle · drag segment edges to trim</span>
    </div>
    <div class="vp-transcript" data-vp-transcript></div>
    <button class="vp-cut-sel" data-vp-cutsel type="button" style="display:none;">Cut selection</button>
  `;

  const stageEl = container.querySelector("[data-vp-stage]");
  const video = container.querySelector("[data-vp-video]");
  const captionEl = container.querySelector("[data-vp-caption]");
  const playBtn = container.querySelector("[data-vp-play]");
  const timeEl = container.querySelector("[data-vp-time]");
  const timelineEl = container.querySelector("[data-vp-timeline]");
  const layersEl = container.querySelector("[data-vp-layers]");
  const playhead = container.querySelector("[data-vp-playhead]");
  const transcriptEl = container.querySelector("[data-vp-transcript]");
  const cutSelBtn = container.querySelector("[data-vp-cutsel]");

  // ----- playback + rAF skip loop -----
  function setPlayIcon() { playBtn.innerHTML = video.paused ? "&#9654;" : "&#10073;&#10073;"; }
  function startLoop() { if (rafId == null) rafId = requestAnimationFrame(tick); }
  function stopLoop() { if (rafId != null) { cancelAnimationFrame(rafId); rafId = null; } }

  function togglePlay() {
    if (!srcPath) return;
    if (video.paused) {
      // If parked past the last kept segment (or inside a cut), start from a sensible spot.
      const tgt = kept.length ? playbackTarget(kept, video.currentTime) : { action: "play" };
      if (tgt.action === "end") video.currentTime = kept[0].start;
      else if (tgt.action === "seek") video.currentTime = tgt.to;
      video.play().catch(() => {});
    } else {
      video.pause();
    }
  }

  function tick() {
    rafId = null;
    if (video.paused) { setPlayIcon(); return; }
    const t = video.currentTime;
    if (kept.length) {
      const tgt = playbackTarget(kept, t);
      if (tgt.action === "end") {
        video.pause();
        video.currentTime = kept[kept.length - 1].end;
        renderPlayhead();
        return;
      }
      // Guard the assignment: setting currentTime to (roughly) where we already are churns the
      // decoder for nothing and can fight the boundary. Only seek when actually off-target.
      if (tgt.action === "seek" && Math.abs(video.currentTime - tgt.to) > EPS) {
        video.currentTime = tgt.to;
      }
    }
    renderPlayhead();
    rafId = requestAnimationFrame(tick);
  }

  function renderPlayhead() {
    const t = video.currentTime || 0;
    const pct = duration > 0 ? Math.min(100, Math.max(0, (t / duration) * 100)) : 0;
    playhead.style.left = pct + "%";
    timeEl.textContent = `${fmtClock(t)} / ${fmtClock(duration)}`;
    renderCaption(t);
    highlightWord(t);
  }

  function renderCaption(t) {
    if (!plan.captions || !captionLines.length) {
      if (capKey !== "") { captionEl.innerHTML = ""; captionEl.className = "vp-caption"; capKey = ""; }
      return;
    }
    const line = captionLines.find(l => t >= l.start - 0.15 && t <= l.end + 0.15);
    if (!line) {
      if (capKey !== "") { captionEl.innerHTML = ""; captionEl.className = "vp-caption"; capKey = ""; }
      return;
    }
    const style = plan.captionStyle || "boxed";
    let key, html;
    if (style === "karaoke") {
      const idx = line.words.findIndex(w => t >= w.start && t < w.end);
      key = "k|" + line.start + "|" + idx;
      html = line.words.map((w, i) =>
        `<span class="vp-cap-word${i === idx ? " on" : ""}">${escapeHtml(w.text)}</span>`
      ).join(" ");
    } else {
      key = style + "|" + line.start;
      html = `<span>${escapeHtml(line.text)}</span>`;
    }
    if (key === capKey) return;
    capKey = key;
    captionEl.className = "vp-caption vp-cap-show vp-cap-" + style;
    captionEl.innerHTML = html;
  }

  function highlightWord(t) {
    const words = analysis?.transcript?.words;
    if (!words || !words.length) return;
    // binary search: last word whose start <= t
    let lo = 0, hi = words.length - 1, found = -1;
    while (lo <= hi) { const m = (lo + hi) >> 1; if (words[m].start <= t) { found = m; lo = m + 1; } else hi = m - 1; }
    const idx = (found >= 0 && t < words[found].end) ? found : -1;
    if (idx === curWordIdx) return;
    curWordIdx = idx;
    if (hiEl) { hiEl.classList.remove("now"); hiEl = null; }
    if (idx >= 0) { hiEl = transcriptEl.querySelector(`.vp-w[data-i="${idx}"]`); if (hiEl) hiEl.classList.add("now"); }
  }

  // ----- timeline -----
  const pctLeft = (x) => (duration > 0 ? (x / duration) * 100 : 0);
  const pctWidth = (a, b) => (duration > 0 ? ((b - a) / duration) * 100 : 0);

  function renderTimeline() {
    const thumbHtml = thumbs.length
      ? `<div class="vp-tl-thumbs">${thumbs.map(u => `<img src="${escapeHtml(u)}" alt="" draggable="false" />`).join("")}</div>`
      : "";
    const cutHtml = cuts.map(c =>
      `<div class="vp-tl-cut" style="left:${pctLeft(c.start)}%;width:${pctWidth(c.start, c.end)}%"></div>`
    ).join("");
    const sil = (analysis?.silences || []).map((r, i) =>
      `<div class="vp-tl-mk vp-tl-sil" data-mk="sil:${i}" title="silence ${fmt1(r.start)}–${fmt1(r.end)}s (click to toggle)" style="left:${pctLeft(r.start)}%;width:${Math.max(0.5, pctWidth(r.start, r.end))}%"></div>`
    ).join("");
    const fill = (analysis?.filler_ranges || []).map((r, i) =>
      `<div class="vp-tl-mk vp-tl-fill" data-mk="fill:${i}" title="filler ${fmt1(r.start)}–${fmt1(r.end)}s (click to toggle)" style="left:${pctLeft(r.start)}%;width:${Math.max(0.5, pctWidth(r.start, r.end))}%"></div>`
    ).join("");
    const handles = kept.map((s, i) =>
      `<div class="vp-tl-handle vp-h-start" data-h="start:${i}" title="Trim in" style="left:${pctLeft(s.start)}%"></div>` +
      `<div class="vp-tl-handle vp-h-end" data-h="end:${i}" title="Trim out" style="left:${pctLeft(s.end)}%"></div>`
    ).join("");
    layersEl.innerHTML = thumbHtml + cutHtml + sil + fill + handles;
  }

  function posToTime(clientX) {
    const rect = timelineEl.getBoundingClientRect();
    const frac = rect.width > 0 ? Math.min(1, Math.max(0, (clientX - rect.left) / rect.width)) : 0;
    return frac * duration;
  }
  function seekTo(t) {
    if (!srcPath || duration <= 0) return;
    video.currentTime = Math.min(duration, Math.max(0, t));
    renderPlayhead();
  }

  // Push new keep-segments to studio. If they cover the whole clip, normalize to [] so the
  // shared plan keeps its "keep everything" meaning (and the checkbox UI stays consistent).
  function commitSegments(segs) {
    const merged = mergeRanges(segs).filter(s => s.end - s.start > 0.05);
    const whole = merged.length === 1 && merged[0].start <= EPS && merged[0].end >= duration - EPS;
    onPlanChange({ segments: whole ? [] : merged });
  }

  function toggleMarker(spec) {
    const [kind, idxStr] = spec.split(":");
    const arr = kind === "sil" ? (analysis?.silences || []) : (analysis?.filler_ranges || []);
    const r = arr[parseInt(idxStr, 10)];
    if (!r) return;
    const cur = materializeKept(plan.segments, duration);
    const mid = (r.start + r.end) / 2;
    // If this range is currently kept, cutting it means subtracting it; if already cut, restore.
    const next = isKept(cur, mid) ? subtractRange(cur, r.start, r.end) : addRange(cur, r.start, r.end, duration);
    commitSegments(next);
  }

  function startHandleDrag(el, e) {
    const [edge, iStr] = el.dataset.h.split(":");
    drag = { edge, i: parseInt(iStr, 10), segs: kept.map(s => ({ ...s })) };
    try { timelineEl.setPointerCapture(e.pointerId); } catch {}
    e.preventDefault();
  }
  function updateHandleDrag(e) {
    const segs = drag.segs;
    const seg = segs[drag.i];
    if (!seg) return;
    const t = posToTime(e.clientX);
    const MIN = 0.05; // clamp so a segment can never invert or collapse
    if (drag.edge === "start") {
      const lower = drag.i > 0 ? segs[drag.i - 1].end + MIN : 0;
      seg.start = Math.min(seg.end - MIN, Math.max(lower, t));
    } else {
      const upper = drag.i < segs.length - 1 ? segs[drag.i + 1].start - MIN : duration;
      seg.end = Math.max(seg.start + MIN, Math.min(upper, t));
    }
    // Live redraw only — the plan isn't committed until pointerup.
    kept = segs.map(s => ({ ...s }));
    cuts = complement(kept, duration);
    renderTimeline();
    renderPlayhead();
  }
  function endHandleDrag() {
    const segs = drag.segs.filter(s => s.end - s.start > 0.05);
    drag = null;
    commitSegments(segs);
  }

  timelineEl.addEventListener("pointerdown", (e) => {
    if (duration <= 0) return;
    const handleEl = e.target.closest("[data-h]");
    if (handleEl) { startHandleDrag(handleEl, e); return; }
    if (e.target.closest("[data-mk]")) return; // markers toggle on click, not scrub
    seeking = true;
    try { timelineEl.setPointerCapture(e.pointerId); } catch {}
    seekTo(posToTime(e.clientX));
  });
  timelineEl.addEventListener("pointermove", (e) => {
    if (drag) { updateHandleDrag(e); return; }
    if (seeking) seekTo(posToTime(e.clientX));
  });
  timelineEl.addEventListener("pointerup", (e) => {
    if (drag) endHandleDrag();
    seeking = false;
    try { timelineEl.releasePointerCapture(e.pointerId); } catch {}
  });
  timelineEl.addEventListener("click", (e) => {
    const mk = e.target.closest("[data-mk]");
    if (mk) toggleMarker(mk.dataset.mk);
  });

  // ----- interactive transcript -----
  function renderTranscript() {
    const words = analysis?.transcript?.words || [];
    if (!words.length) {
      const segs = analysis?.transcript?.segments || [];
      transcriptEl.innerHTML = segs.length
        ? segs.map(s => `<span class="vp-w-plain">${escapeHtml(s.text)}</span> `).join("")
        : `<span class="vp-tr-empty">Analyze speech to enable the interactive transcript.</span>`;
      return;
    }
    const fillers = analysis?.filler_ranges || [];
    transcriptEl.innerHTML = words.map((w, i) => {
      const mid = (w.start + w.end) / 2;
      const cls = ["vp-w"];
      if (!isKept(kept, mid)) cls.push("cut");
      if (fillers.some(r => mid >= r.start && mid < r.end)) cls.push("filler");
      return `<span class="${cls.join(" ")}" data-i="${i}" data-start="${w.start}" data-end="${w.end}">${escapeHtml(w.word)}</span>`;
    }).join(" ");
    hiEl = null; curWordIdx = -1; // spans were replaced
    paintSelection();
  }

  function paintSelection() {
    transcriptEl.querySelectorAll(".vp-w.sel").forEach(el => el.classList.remove("sel"));
    if (!selRange) return;
    transcriptEl.querySelectorAll(".vp-w[data-i]").forEach(el => {
      const i = parseInt(el.dataset.i, 10);
      if (i >= selRange.a && i <= selRange.b) el.classList.add("sel");
    });
  }
  function showCutSel(e) {
    const rect = container.getBoundingClientRect();
    cutSelBtn.style.left = Math.max(4, Math.min(rect.width - 96, e.clientX - rect.left)) + "px";
    cutSelBtn.style.top = Math.max(4, e.clientY - rect.top - 34) + "px";
    cutSelBtn.style.display = "";
  }
  function hideCutSel() { cutSelBtn.style.display = "none"; }
  function clearSelection() { selRange = null; selAnchor = null; dragSelecting = false; paintSelection(); hideCutSel(); }
  function setSelection(a, b, e) { selRange = { a, b }; paintSelection(); showCutSel(e); }

  transcriptEl.addEventListener("mousedown", (e) => {
    const w = e.target.closest(".vp-w[data-i]");
    if (!w) return;
    const i = parseInt(w.dataset.i, 10);
    if (e.shiftKey && selAnchor != null) { // shift-click extends the selection
      setSelection(Math.min(selAnchor, i), Math.max(selAnchor, i), e);
      e.preventDefault();
      return;
    }
    selAnchor = i;
    selRange = null;
    dragSelecting = true;
    paintSelection();
    hideCutSel();
  });
  transcriptEl.addEventListener("mouseover", (e) => {
    if (!dragSelecting || selAnchor == null) return;
    const w = e.target.closest(".vp-w[data-i]");
    if (!w) return;
    const i = parseInt(w.dataset.i, 10);
    selRange = { a: Math.min(selAnchor, i), b: Math.max(selAnchor, i) };
    paintSelection();
  });
  transcriptEl.addEventListener("mouseup", (e) => {
    if (!dragSelecting) return;
    dragSelecting = false;
    const w = e.target.closest(".vp-w[data-i]");
    if (!w || selAnchor == null) return;
    const i = parseInt(w.dataset.i, 10);
    if (i === selAnchor) { seekTo(parseFloat(w.dataset.start)); clearSelection(); return; } // plain click
    setSelection(Math.min(selAnchor, i), Math.max(selAnchor, i), e);
  });
  cutSelBtn.addEventListener("click", () => {
    if (!selRange) return;
    const words = analysis?.transcript?.words || [];
    const a = words[selRange.a], b = words[selRange.b];
    if (!a || !b) { clearSelection(); return; }
    const cur = materializeKept(plan.segments, duration);
    // ~40ms padding so we don't clip the neighbouring kept words.
    const next = subtractRange(cur, Math.max(0, a.start - 0.04), Math.min(duration, b.end + 0.04));
    clearSelection();
    commitSegments(next);
  });

  // ----- video-level plan approximations -----
  function applyAspect() {
    const a = plan.aspect || "";
    let ratio = null;
    if (a.includes(":")) { const [w, h] = a.split(":").map(Number); if (w > 0 && h > 0) ratio = w / h; }
    if (ratio) {
      // Backend center-crops to fill the target aspect → object-fit:cover mirrors the framing.
      stageEl.style.aspectRatio = String(ratio);
      video.style.objectFit = "cover";
    } else {
      stageEl.style.aspectRatio = (probe && probe.width && probe.height) ? `${probe.width} / ${probe.height}` : "16 / 9";
      video.style.objectFit = "contain";
    }
  }

  // ----- media element listeners -----
  playBtn.addEventListener("click", togglePlay);
  stageEl.addEventListener("click", (e) => { if (e.target === video || e.target === stageEl || e.target === captionEl) togglePlay(); });
  video.addEventListener("play", () => { setPlayIcon(); startLoop(); });
  video.addEventListener("pause", () => { setPlayIcon(); });
  video.addEventListener("seeked", renderPlayhead);
  video.addEventListener("ended", () => { setPlayIcon(); });
  video.addEventListener("loadedmetadata", renderPlayhead);

  // ----- the single refresh entry point -----
  function update(payload) {
    const p = payload || {};
    if (p.plan) plan = p.plan;
    if (p.probe !== undefined) probe = p.probe;
    if (p.analysis !== undefined) analysis = p.analysis;
    if (Array.isArray(p.thumbs)) thumbs = p.thumbs;
    duration = probe ? probe.duration_s : 0;

    // Reload the source only when the path actually changes, so plan tweaks never interrupt
    // playback or reset the position.
    if (p.srcPath && p.srcPath !== srcPath) {
      srcPath = p.srcPath;
      video.src = convertFileSrc(srcPath);
      capKey = ""; curWordIdx = -1; hiEl = null;
      clearSelection();
    }

    // Don't clobber a live drag's geometry; updateHandleDrag owns kept/cuts until pointerup.
    if (!drag) {
      kept = materializeKept(plan.segments, duration);
      cuts = complement(kept, duration);
    }
    captionLines = plan.captions ? buildCaptionLines(analysis?.transcript?.words || [], kept, duration) : [];
    // Sentinel (never equals a real key): forces renderCaption to re-evaluate — and, when
    // captions were just turned off, to run its clear branch and wipe any stale overlay.
    capKey = " ";

    video.playbackRate = plan.speed || 1;
    video.muted = !!plan.mute;
    video.style.filter = CSS_FILTERS[plan.filter] || "";
    applyAspect();

    renderTimeline();
    renderTranscript();
    renderPlayhead();
    setPlayIcon();
  }

  function cleanup() {
    stopLoop();
    try { video.pause(); } catch {}
    video.removeAttribute("src");
    try { video.load(); } catch {}
  }

  return { update, cleanup };
}
