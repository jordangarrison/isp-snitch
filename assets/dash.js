// Dashboard: every panel on one page, btop style. The time panels (loss,
// latency, hops) share one x layout, so a column is the same time in each.
"use strict";

const DOT = 3;              // dot pitch in px: a 2px dot and a 1px gap
const STRIP_H = 36;         // one target's strip in the loss and latency panels
const HEAT_ROW_MIN = 13, HEAT_ROW_MAX = 22;
const AXIS_H = 16;
const LIVE_SECS = 300;      // trace window when no time is picked
const REFRESH_AT_MS = 12000; // reload at minute + 12 s; the service flushes at minute + 10 s
const LOSS_FLOOR = 0.1;     // loss strips use a log scale from this % up to 100%
const RANGE_KEYS = Object.keys(RANGES);
const TICK_STEPS = [300, 900, 1800, 3600, 10800, 21600, 43200, 86400, 172800, 604800, 2592000];

const S = {
  gen: 0, hopsGen: 0, traceGen: 0, timer: null,
  range: null, status: null, summary: null, where: null, series: [], hops: null,
  target: null, mode: "loss", sel: null, // sel: {from, to} picked window, null = live
  drag: null,
};

/* ---------- hash state ---------- */

function hashParams() { return new URLSearchParams(location.hash.slice(1)); }

/** Updates hash params that do not change the data range, without reloading. */
function setHash(updates) {
  const p = hashParams();
  for (const [k, v] of Object.entries(updates)) {
    if (v == null) p.delete(k); else p.set(k, String(v));
  }
  history.replaceState(null, "", "#" + p.toString());
}

/** Changes the data range; keeps target and mode. Triggers a reload. */
function goRange(params) {
  const p = hashParams();
  for (const k of ["r", "from", "to", "at", "span"]) p.delete(k);
  for (const [k, v] of Object.entries(params)) p.set(k, String(v));
  location.hash = p.toString();
}

function readState() {
  const p = hashParams();
  const t = parseInt(p.get("t"), 10);
  S.target = Number.isFinite(t) ? t : S.target;
  S.mode = p.get("m") === "lat" ? "lat" : "loss";
  const at = parseInt(p.get("at"), 10), span = parseInt(p.get("span"), 10);
  S.sel = Number.isFinite(at) && span > 0 ? { from: at, to: at + span } : null;
}

/* ---------- colors and formatting ---------- */

let C = null;
function palette() {
  const v = (n) => css(n);
  C = {
    fg: v("--fg"), title: v("--title"), muted: v("--muted"), faint: v("--faint"), hi: v("--hi"),
    zero: v("--cell-zero"), hatchA: v("--hatch-a"), hatchB: v("--hatch-b"), mono: v("--mono"),
    loss: [1, 2, 3, 4, 5, 6].map((i) => v("--loss-" + i)),
    lat: [1, 2, 3, 4, 5, 6].map((i) => v("--lat-" + i)),
  };
}
function lossC(p) {
  if (p <= 0) return C.zero;
  return C.loss[p >= 100 ? 5 : LOSS_EDGES.findIndex((e) => p <= e)];
}
function latC(ms) {
  const i = LAT_EDGES.findIndex((e) => ms < e);
  return C.lat[i === -1 ? 5 : i];
}
function sw(color) { return `<span class="sw" style="background:${color}"></span>`; }
function fmtDur(s) {
  if (s < 3600) return Math.round(s / 60) + "m";
  if (s < 86400) { const h = Math.floor(s / 3600), m = Math.round((s % 3600) / 60); return m ? `${h}h ${m}m` : `${h}h`; }
  const d = Math.floor(s / 86400), h = Math.round((s % 86400) / 3600);
  return h ? `${d}d ${h}h` : `${d}d`;
}
function fmtClock(sec) { return new Date(sec * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }); }
function targetName(id) {
  const t = S.summary?.targets.find((x) => x.id === id);
  return t ? t.name : "?";
}

/* ---------- geometry shared by the time panels ---------- */

function geom() {
  const W = document.getElementById("loss-wrap").clientWidth;
  const narrow = W < 640;
  const labelW = narrow ? 88 : 172, statW = narrow ? 82 : 112;
  const gw = Math.max(DOT * 20, Math.floor((W - labelW - statW) / DOT) * DOT);
  const ts = S.series[0]?.ts || [];
  const step = S.series[0]?.step || 60;
  const t0 = ts[0] ?? 0, t1 = ts.length ? ts[ts.length - 1] + step : 1;
  return { W, labelW, statW, gx: labelW, gw, ts, step, n: ts.length, t0, t1 };
}
function xOf(g, t) { return g.gx + ((t - g.t0) / (g.t1 - g.t0)) * g.gw; }
function idxAt(g, x) {
  if (x < g.gx || x >= g.gx + g.gw || !g.n) return null;
  return Math.min(g.n - 1, Math.floor(((x - g.gx) / g.gw) * g.n));
}

function setupCanvas(canvas, w, h) {
  const dpr = devicePixelRatio || 1;
  canvas.style.width = w + "px";
  canvas.style.height = h + "px";
  canvas.width = Math.round(w * dpr);
  canvas.height = Math.round(h * dpr);
  const ctx = canvas.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  ctx.font = `12px ${C.mono}`;
  ctx.textBaseline = "middle";
  return ctx;
}

/** Text cut to fit maxW, with an ellipsis. */
function fit(ctx, text, maxW) {
  if (ctx.measureText(text).width <= maxW) return text;
  let t = text;
  while (t.length > 1 && ctx.measureText(t + "…").width > maxW) t = t.slice(0, -1);
  return t + "…";
}

/**
 * Dot-matrix column graph. scale maps a value to 0..1 of the height,
 * levelValue maps a height fraction back to a value for that dot's color.
 * Columns with data but a zero value show one dim dot; gaps show nothing.
 */
function dots(ctx, g, y0, h, vals, scale, levelValue, colorOf, agg) {
  const cols = g.gw / DOT, rows = Math.floor(h / DOT), n = vals.length;
  for (let c = 0; c < cols; c++) {
    const i0 = Math.floor((c * n) / cols), i1 = Math.max(i0 + 1, Math.floor(((c + 1) * n) / cols));
    let v = null, sum = 0, cnt = 0;
    for (let i = i0; i < i1 && i < n; i++) {
      if (vals[i] == null) continue;
      sum += vals[i]; cnt++;
      v = v == null ? vals[i] : Math.max(v, vals[i]);
    }
    if (cnt === 0) continue;
    if (agg === "mean") v = sum / cnt;
    const lit = v > 0 ? Math.max(1, Math.round(Math.min(1, scale(v)) * rows)) : 0;
    const x = g.gx + c * DOT;
    for (let r = 0; r < Math.max(1, lit); r++) {
      ctx.fillStyle = r < lit ? colorOf(levelValue((r + 1) / rows)) : C.zero;
      ctx.fillRect(x, y0 + h - (r + 1) * DOT, DOT - 1, DOT - 1);
    }
  }
}

const lossScale = (v) => Math.log1p(v / LOSS_FLOOR) / Math.log1p(100 / LOSS_FLOOR);
const lossLevel = (f) => LOSS_FLOOR * (Math.pow(1 + 100 / LOSS_FLOOR, f) - 1);

function guide(ctx, g, y) {
  ctx.fillStyle = C.faint;
  for (let x = g.gx; x < g.gx + g.gw; x += 6) ctx.fillRect(x, Math.round(y), 2, 1);
}

function axis(ctx, g, y) {
  const span = g.t1 - g.t0;
  const want = Math.max(2, Math.floor(g.gw / 110));
  const step = TICK_STEPS.find((s) => span / s <= want) || TICK_STEPS[TICK_STEPS.length - 1];
  const off = new Date(g.t0 * 1000).getTimezoneOffset() * 60;
  ctx.fillStyle = C.muted;
  ctx.textAlign = "center";
  for (let t = Math.ceil((g.t0 - off) / step) * step + off; t < g.t1; t += step) {
    const x = xOf(g, t);
    const d = new Date(t * 1000);
    const midnight = d.getHours() === 0 && d.getMinutes() === 0;
    const label = step >= 86400 || midnight
      ? d.toLocaleDateString([], { month: "short", day: "numeric" })
      : d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
    ctx.fillRect(Math.round(x), y, 1, 3);
    if (x > g.gx + 20 && x < g.gx + g.gw - 20) ctx.fillText(label, x, y + 10);
  }
  ctx.textAlign = "left";
}

function drawLabel(ctx, g, y, h, name, sub, selected) {
  if (selected) { ctx.fillStyle = C.hi; ctx.fillText("▶", 0, y + h / 2 - 7); }
  ctx.fillStyle = selected ? C.title : C.fg;
  ctx.fillText(fit(ctx, name, g.labelW - 20), 14, y + h / 2 - 7);
  ctx.fillStyle = C.muted;
  ctx.fillText(fit(ctx, sub, g.labelW - 20), 14, y + h / 2 + 7);
}

function drawStat(ctx, g, y, h, color, main, sub) {
  const x = g.gx + g.gw + 10;
  if (color) { ctx.fillStyle = color; ctx.fillRect(x, y + h / 2 - 11, 8, 8); }
  ctx.fillStyle = C.title;
  ctx.fillText(main, x + (color ? 13 : 0), y + h / 2 - 7);
  ctx.fillStyle = C.muted;
  ctx.fillText(fit(ctx, sub, g.statW - 12), x, y + h / 2 + 7);
}

/* ---------- time panels ---------- */

function drawLoss() {
  const g = geom();
  const targets = S.summary?.targets || [];
  const ctx = setupCanvas(document.getElementById("loss"), g.W, Math.max(1, targets.length) * STRIP_H);
  targets.forEach((t, r) => {
    const y = r * STRIP_H, h = STRIP_H - 6;
    drawLabel(ctx, g, y, STRIP_H, t.name, t.addr, t.id === S.target);
    guide(ctx, g, y + 3 + h - lossScale(1) * h);
    guide(ctx, g, y + 3 + h - lossScale(10) * h);
    const s = S.series[r];
    if (s) dots(ctx, g, y + 3, h, s.loss_pct, lossScale, lossLevel, lossC, "max");
    drawStat(ctx, g, y, STRIP_H, t.loss_pct == null ? null : lossC(t.loss_pct), fmtPct(t.loss_pct),
      `${t.lossy_buckets} lossy`);
  });
  document.getElementById("loss-sub").textContent =
    `target loss per ${fmtStep(g.step)} · log scale, guides at 1% and 10%`;
}

function drawLat() {
  const g = geom();
  const targets = S.summary?.targets || [];
  const all = S.series.flatMap((s) => s.avg_ms.filter((v) => v != null)).sort((a, b) => a - b);
  const median = all.length ? all[Math.floor(all.length / 2)] : 10;
  const top = Math.max(20, Math.ceil((median * 2.5) / 10) * 10);
  const scale = (v) => v / top, level = (f) => top * f;
  const ctx = setupCanvas(document.getElementById("lat"), g.W, Math.max(1, targets.length) * STRIP_H + AXIS_H);
  targets.forEach((t, r) => {
    const y = r * STRIP_H, h = STRIP_H - 6;
    drawLabel(ctx, g, y, STRIP_H, t.name, t.addr, t.id === S.target);
    const s = S.series[r];
    if (s) dots(ctx, g, y + 3, h, s.avg_ms, scale, level, latC, "mean");
    drawStat(ctx, g, y, STRIP_H, t.avg_ms == null ? null : latC(t.avg_ms), fmtMs(t.avg_ms),
      t.max_ms == null ? "" : `max ${t.max_ms.toFixed(0)} ms`);
  });
  axis(ctx, g, targets.length * STRIP_H + 2);
  document.getElementById("lat-sub").textContent = `average per ${fmtStep(g.step)} · top ${top.toFixed(0)} ms`;
}

/** Heatmap row height: fills the panel on wide screens, fixed when stacked. */
function heatRow(rows) {
  const wrap = document.getElementById("heat-wrap");
  if (innerWidth <= 1000 || !rows) return HEAT_ROW_MIN;
  const fill = Math.floor((wrap.clientHeight - AXIS_H - 2) / rows);
  return Math.max(HEAT_ROW_MIN, Math.min(HEAT_ROW_MAX, fill));
}

let hatch = null;
function hatchPattern(ctx) {
  const c = document.createElement("canvas");
  c.width = c.height = 4;
  const x = c.getContext("2d");
  x.fillStyle = C.hatchB; x.fillRect(0, 0, 4, 4);
  x.fillStyle = C.hatchA;
  for (let i = 0; i < 4; i++) x.fillRect(i, 3 - i, 1, 1);
  return ctx.createPattern(c, "repeat");
}

function drawHeat() {
  const g = geom();
  const canvas = document.getElementById("heat");
  const hops = S.hops && S.hops.ts.length === g.n ? S.hops.hops : [];
  S.heatRow = heatRow(hops.length);
  const HEAT_ROW = S.heatRow;
  const ctx = setupCanvas(canvas, g.W, hops.length * HEAT_ROW + AXIS_H);
  hatch = hatchPattern(ctx);
  const cw = g.gw / Math.max(1, g.n);
  hops.forEach((hop, r) => {
    const y = r * HEAT_ROW;
    const last = r === hops.length - 1;
    ctx.fillStyle = last ? C.title : C.fg;
    ctx.fillText(fit(ctx, `${String(hop.ttl).padStart(2)} ${hop.addr || "???"}`, g.labelW - 8), 0, y + HEAT_ROW / 2);
    let sum = 0, cnt = 0;
    for (let i = 0; i < g.n; i++) {
      const eff = hop.eff_loss_pct[i];
      if (eff != null) { sum += eff; cnt++; }
      let fill;
      if (S.mode === "loss") {
        if (eff == null) continue;
        fill = hop.rate_limited[i] && eff === 0 ? hatch : lossC(eff);
      } else {
        const ms = hop.avg_ms[i];
        if (ms == null) continue;
        fill = latC(ms);
      }
      const x0 = Math.floor(g.gx + i * cw), x1 = Math.floor(g.gx + (i + 1) * cw) - (cw > 3 ? 1 : 0);
      ctx.fillStyle = fill;
      ctx.fillRect(x0, y + 1, Math.max(1, x1 - x0), HEAT_ROW - 2);
    }
    const mean = cnt ? sum / cnt : null;
    const x = g.gx + g.gw + 10;
    if (mean != null) {
      ctx.fillStyle = lossC(mean); ctx.fillRect(x, y + HEAT_ROW / 2 - 4, 8, 8);
      ctx.fillStyle = last ? C.title : C.fg; ctx.fillText(fmtPct(mean), x + 13, y + HEAT_ROW / 2);
    }
  });
  if (hops.length) axis(ctx, g, hops.length * HEAT_ROW + 2);
  document.getElementById("hops-sub").textContent = S.target == null ? "" :
    `${targetName(S.target)} · ${S.mode === "loss" ? "effective loss" : "latency"} · right: mean effective loss`;
  drawLegend();
}

function drawLegend() {
  const el = document.getElementById("heat-legend");
  if (S.mode === "loss") {
    const labels = ["0", "≤1%", "≤3%", "≤10%", "≤30%", "<100%", "100%"];
    el.innerHTML = labels.map((l, i) => `<span>${sw(i === 0 ? C.zero : C.loss[i - 1])}${l}</span>`).join("") +
      `<span><span class="sw hatch"></span>rate-limited, not real loss</span><span>blank = no data</span>`;
  } else {
    const labels = ["<10", "<20", "<40", "<80", "<150", "≥150 ms"];
    el.innerHTML = labels.map((l, i) => `<span>${sw(C.lat[i])}${l}</span>`).join("") + `<span>blank = no answer</span>`;
  }
}

/* ---------- overlays: crosshair, picked window, drag ---------- */

const WRAPS = ["loss-wrap", "lat-wrap", "heat-wrap"];
function overlay(wrapId, cls) {
  const wrap = document.getElementById(wrapId);
  let el = wrap.querySelector("." + cls);
  if (!el) { el = document.createElement("div"); el.className = cls; wrap.appendChild(el); }
  el.style.height = wrap.querySelector("canvas").style.height;
  return el;
}

function showXhair(g, i) {
  for (const id of WRAPS) {
    const el = overlay(id, "xhair");
    if (i == null) { el.style.display = "none"; continue; }
    el.style.left = Math.round(g.gx + ((i + 0.5) / g.n) * g.gw) + "px";
    el.style.display = "block";
  }
}

function drawBands() {
  const g = geom();
  for (const id of WRAPS) {
    const el = overlay(id, "band");
    if (!S.sel || !g.n || S.sel.to <= g.t0 || S.sel.from >= g.t1) { el.style.display = "none"; continue; }
    const a = Math.max(g.gx, xOf(g, S.sel.from)), b = Math.min(g.gx + g.gw, xOf(g, S.sel.to));
    el.style.left = a + "px";
    el.style.width = Math.max(2, b - a) + "px";
    el.style.display = "block";
  }
}

function tipFor(panel, g, i, row) {
  const t = g.ts[i];
  const head = `<div class="t">${esc(fmtTime(t))} · ${fmtStep(g.step)}</div>`;
  if (panel === "heat") {
    const hop = S.hops?.hops[row];
    if (!hop) return null;
    const eff = hop.eff_loss_pct[i];
    if (eff == null) return head + `hop ${hop.ttl}: no data`;
    const rl = hop.rate_limited[i] ? `<br><span class="t">rate-limited ICMP, not real loss</span>` : "";
    return head + `hop ${hop.ttl} ${esc(hop.addr || "???")}<br>loss <b>${fmtPct(hop.loss_pct[i])}</b> · effective <b>${fmtPct(eff)}</b>` +
      `<br>avg <b>${fmtMs(hop.avg_ms[i])}</b>${rl}`;
  }
  const tg = S.summary?.targets[row], s = S.series[row];
  if (!tg || !s) return null;
  if (s.loss_pct[i] == null) return head + `${esc(tg.name)}: no data (service not running)`;
  return head + `${esc(tg.name)}<br>loss <b>${fmtPct(s.loss_pct[i])}</b> · avg <b>${fmtMs(s.avg_ms[i])}</b> · max <b>${fmtMs(s.max_ms[i])}</b>`;
}

function bindPanel(canvasId, panel, rowHeight) {
  const canvas = document.getElementById(canvasId);
  const wrap = canvas.parentElement;
  const pos = (ev) => {
    const r = canvas.getBoundingClientRect();
    return { x: ev.clientX - r.left, y: ev.clientY - r.top };
  };
  canvas.addEventListener("mousemove", (ev) => {
    const g = geom();
    const { x, y } = pos(ev);
    if (S.drag && S.drag.canvas === canvas) {
      const el = overlay(wrap.id, "drag");
      const a = Math.max(g.gx, Math.min(S.drag.x, x)), b = Math.min(g.gx + g.gw, Math.max(S.drag.x, x));
      el.style.left = a + "px"; el.style.width = (b - a) + "px"; el.style.display = "block";
    }
    const i = idxAt(g, x);
    showXhair(g, i);
    const row = Math.floor(y / rowHeight());
    const html = i == null ? null : tipFor(panel, g, i, row);
    if (html) tooltip.show(html, ev.clientX, ev.clientY); else tooltip.hide();
  });
  canvas.addEventListener("mouseleave", () => { showXhair(null, null); tooltip.hide(); });
  canvas.addEventListener("mousedown", (ev) => {
    const g = geom(), { x } = pos(ev);
    if (idxAt(g, x) != null) { S.drag = { canvas, x }; ev.preventDefault(); }
  });
  canvas.addEventListener("mouseup", (ev) => {
    const g = geom(), { x, y } = pos(ev);
    const d = S.drag;
    S.drag = null;
    overlay(wrap.id, "drag").style.display = "none";
    const row = Math.floor(y / rowHeight());
    if (panel !== "heat" && x < g.gx) {
      const t = S.summary?.targets[row];
      if (t) selectTarget(t.id);
      return;
    }
    if (!d || d.canvas !== canvas) return;
    if (Math.abs(x - d.x) > 6) {
      const a = idxAt(g, Math.max(g.gx, Math.min(d.x, x))), b = idxAt(g, Math.min(g.gx + g.gw - 1, Math.max(d.x, x)));
      if (a != null && b != null) goRange({ from: g.ts[a], to: g.ts[b] + g.step });
      return;
    }
    const i = idxAt(g, x);
    if (i == null) return;
    if (panel !== "heat") {
      const t = S.summary?.targets[row];
      if (t && t.id !== S.target) selectTarget(t.id, false);
    }
    pick({ from: g.ts[i], to: g.ts[i] + g.step });
  });
}
addEventListener("mouseup", () => {
  if (!S.drag) return;
  S.drag = null;
  for (const id of WRAPS) overlay(id, "drag").style.display = "none";
});

/* ---------- side panels ---------- */

function segLabel(s) {
  if (s.name === "gateway") return "hop 1";
  if (s.first_ttl == null) return "none";
  if (s.last_ttl == null) return `hops ${s.first_ttl}+`;
  if (s.last_ttl < s.first_ttl) return "none";
  return s.first_ttl === s.last_ttl ? `hop ${s.first_ttl}` : `hops ${s.first_ttl}-${s.last_ttl}`;
}
const SEG_TITLE = { gateway: "gateway", upstream: "upstream", past_split: "past split" };

function meter(pct) {
  const on = Math.round((pct || 0) / 5);
  let h = "";
  for (let i = 0; i < 20; i++) h += i < on ? '<i class="on"></i>' : "<i></i>";
  return `<span class="meter">${h}</span>`;
}

function renderWhere() {
  const w = S.where, el = document.getElementById("where");
  if (!w) return;
  document.getElementById("where-sub").textContent = `loss starts at · mean target loss ${fmtPct(w.loss_pct)}`;
  const segs = w.segments;
  const top = segs.filter((s) => s.share_pct != null).sort((a, b) => b.share_pct - a.share_pct)[0];
  let verdict;
  if (!top || w.loss_pct < 0.005) {
    verdict = `<p class="verdict ok"><b>No loss</b> in this range.</p>`;
  } else {
    const where = {
      gateway: `<b>your gateway</b> (hop 1): this host, its cable or NIC, or the router itself`,
      upstream: w.split_ttl == null
        ? `<b>upstream</b> (${segLabel(top)}), past your gateway`
        : `<b>upstream</b> (${segLabel(top)}), on the path every target shares: likely your ISP`,
      past_split: `<b>past the split</b> (${segLabel(top)}), on the target networks, not yours`,
    }[top.name];
    verdict = `<p class="verdict">${top.share_pct.toFixed(0)}% of loss starts at ${where}.</p>`;
  }
  let html = verdict + `<div class="rows seg-rows">`;
  for (const s of segs) {
    html += `<span>${SEG_TITLE[s.name]}</span><span class="dim">${segLabel(s)}</span>${meter(s.share_pct)}` +
      `<span class="n">${s.share_pct == null ? "-" : s.share_pct.toFixed(0) + "%"}</span><span class="n dim">${fmtPct(s.loss_pct)}</span>`;
  }
  html += `</div><div class="rule">by hop, all targets</div><div class="rows hop-rows">`;
  for (const h of w.hops) {
    const addr = h.addr ? esc(h.addr) : `<span class="dim">differs per target</span>`;
    html += `<span class="n dim">${h.ttl}</span><span class="addr">${addr}</span>${meter(h.share_pct)}` +
      `<span class="n">${h.share_pct == null ? "-" : h.share_pct.toFixed(0) + "%"}</span>`;
    if (w.split_ttl != null && h.ttl === w.split_ttl) {
      html += `<span></span><span class="dim" style="grid-column: span 3">── paths split ──</span>`;
    }
  }
  el.innerHTML = html + `</div>`;
}

function renderIncidents() {
  const w = S.where, el = document.getElementById("incidents");
  if (!w) return;
  const inc = w.incidents;
  const total = inc.reduce((a, x) => a + (x.end - x.start), 0);
  document.getElementById("inc-sub").textContent = `${inc.length} · ${fmtDur(total)} with loss ≥ 1%`;
  if (!inc.length) { el.innerHTML = `<p class="empty">No ${fmtStep(w.step)} bucket with target loss of 1% or more.</p>`; return; }
  let rows = "";
  for (const x of inc) {
    const sel = S.sel && S.sel.from === x.start && S.sel.to === x.end;
    const where = x.origin_ttl == null ? "-" : `${SEG_TITLE[x.segment]} h${x.origin_ttl}`;
    rows += `<tr class="click${sel ? " sel" : ""}" data-from="${x.start}" data-to="${x.end}" title="${esc(x.origin_addr || "")}">` +
      `<td class="l">${esc(fmtTime(x.start))}</td><td>${fmtDur(x.end - x.start)}</td>` +
      `<td class="l">${sw(lossC(x.peak_loss_pct))}${fmtPct(x.peak_loss_pct)}</td>` +
      `<td>${x.targets_hit}/${w.targets_total}</td><td class="l">${esc(where)}</td></tr>`;
  }
  el.innerHTML = `<table class="t"><thead><tr><th class="l">start</th><th>dur</th><th class="l">peak</th><th>tgts</th><th class="l">starts at</th></tr></thead><tbody>${rows}</tbody></table>`;
  el.querySelectorAll("tr.click").forEach((tr) => {
    tr.onclick = () => pick({ from: +tr.dataset.from, to: +tr.dataset.to });
  });
}

async function loadTrace() {
  const el = document.getElementById("trace"), sub = document.getElementById("trace-sub");
  if (S.target == null) { el.innerHTML = ""; return; }
  const gen = ++S.traceGen;
  const live = !S.sel;
  const to = live ? nowSec() : S.sel.to, from = live ? to - LIVE_SECS : S.sel.from;
  sub.textContent = `${targetName(S.target)} · ` + (live ? "live, last 5 min" : `${fmtTime(from)} to ${fmtClock(to)}`);
  let d;
  try {
    d = await getJSON(`/api/targets/${S.target}/slice?from=${from}&to=${to}`);
  } catch (e) {
    if (gen === S.traceGen) showMessage(el, `Could not load trace: ${e.message}`, true);
    return;
  }
  if (gen !== S.traceGen) return;
  if (!d.hops.length) { showMessage(el, live ? "No data in the last 5 minutes yet." : "No data in this window."); return; }
  let rows = "";
  for (const h of d.hops) {
    const lossCell = h.rate_limited ? `<td class="dim">${fmtPct(h.loss_pct)}<span class="tag">rl</span></td>` : `<td>${fmtPct(h.loss_pct)}</td>`;
    rows += `<tr class="${h.reached ? "target-row" : ""}"><td class="dim">${h.ttl}</td>` +
      `<td class="l addr">${esc(h.addr || "???")}${h.addr_changed ? '<span class="tag" title="more than one responder">*</span>' : ""}</td>` +
      `${lossCell}<td class="l">${sw(lossC(h.eff_loss_pct))}${fmtPct(h.eff_loss_pct)}</td><td>${h.sent}</td>` +
      `<td>${h.avg_ms == null ? "-" : h.avg_ms.toFixed(1)}</td><td>${h.min_ms == null ? "-" : h.min_ms.toFixed(1)}</td>` +
      `<td>${h.max_ms == null ? "-" : h.max_ms.toFixed(1)}</td></tr>`;
  }
  el.innerHTML = `<table class="t"><thead><tr><th>#</th><th class="l">host</th><th>loss</th><th class="l">eff</th><th>snt</th>` +
    `<th>avg</th><th>best</th><th>wrst</th></tr></thead><tbody>${rows}</tbody></table>`;
}

/* ---------- header ---------- */

function renderHeader() {
  const nav = document.getElementById("ranges");
  nav.innerHTML = "";
  RANGE_KEYS.forEach((key, i) => {
    const b = document.createElement("button");
    b.innerHTML = `<sup>${i + 1}</sup>${key}`;
    b.setAttribute("aria-pressed", String(S.range?.key === key));
    b.onclick = () => goRange({ r: key });
    nav.appendChild(b);
  });
  document.getElementById("range-label").textContent =
    S.range && !S.range.key ? `${fmtTime(S.range.from)} to ${fmtTime(S.range.to)}` : "";
  const st = S.status;
  document.getElementById("version").textContent = st ? `v${st.version}` : "";
  document.getElementById("iface").textContent = st?.interface || "";
  renderHealth();
}

function renderHealth(error) {
  const el = document.getElementById("health");
  const st = S.status;
  let cls, text;
  if (error) { cls = "error"; text = `error: ${error}`; }
  else if (!st) { cls = "paused"; text = "loading"; }
  else if (st.latest_minute == null) { cls = "stale"; text = "no data yet"; }
  else {
    const age = nowSec() - (st.latest_minute + 60);
    const fresh = age < 150;
    cls = !fresh ? "stale" : S.range?.key ? "live" : "paused";
    text = `${!fresh ? "stale" : S.range?.key ? "live" : "zoomed"} · data to ${fmtClock(st.latest_minute + 60)}`;
  }
  el.className = "health " + cls;
  el.textContent = text;
}

function renderTabs() {
  const tabs = document.getElementById("target-tabs");
  tabs.innerHTML = "";
  for (const t of S.summary?.targets || []) {
    const b = document.createElement("button");
    b.textContent = t.name;
    b.setAttribute("aria-pressed", String(t.id === S.target));
    b.onclick = () => selectTarget(t.id);
    tabs.appendChild(b);
  }
  const modes = document.getElementById("mode-tabs");
  modes.innerHTML = "";
  for (const [m, label] of [["loss", "loss"], ["lat", "latency"]]) {
    const b = document.createElement("button");
    b.textContent = label;
    b.setAttribute("aria-pressed", String(S.mode === m));
    b.onclick = () => setMode(m);
    modes.appendChild(b);
  }
  const p = new URLSearchParams();
  if (S.range?.key) p.set("r", S.range.key); else if (S.range) { p.set("from", S.range.from); p.set("to", S.range.to); }
  document.getElementById("detail-link").href = S.target == null ? "/" : `/target/${S.target}#${p}`;
}

/* ---------- actions ---------- */

function drawTime() {
  if (!S.summary) return;
  drawLoss(); drawLat(); drawHeat(); drawBands();
}

function pick(sel) {
  S.sel = sel;
  setHash(sel ? { at: sel.from, span: sel.to - sel.from } : { at: null, span: null });
  drawBands();
  renderIncidents();
  loadTrace();
}

function setMode(m) {
  S.mode = m;
  setHash({ m: m === "lat" ? "lat" : null });
  renderTabs();
  drawHeat();
  drawBands();
}

async function selectTarget(id, reloadTrace = true) {
  if (id === S.target) return;
  S.target = id;
  setHash({ t: id });
  renderTabs();
  drawLoss(); drawLat();
  if (reloadTrace) loadTrace();
  const gen = ++S.hopsGen;
  const { from, to } = S.range;
  try {
    const hops = await getJSON(`/api/targets/${id}/hops?from=${from}&to=${to}`);
    if (gen !== S.hopsGen) return;
    S.hops = hops;
  } catch (e) {
    if (gen === S.hopsGen) S.hops = null;
  }
  drawHeat(); drawBands();
}

function cycleTarget(dir) {
  const ts = S.summary?.targets || [];
  if (!ts.length) return;
  const i = Math.max(0, ts.findIndex((t) => t.id === S.target));
  selectTarget(ts[(i + dir + ts.length) % ts.length].id);
}

async function load() {
  clearTimeout(S.timer);
  readState();
  const range = readRange();
  const gen = ++S.gen;
  const q = `from=${range.from}&to=${range.to}`;
  try {
    const [status, summary, where] = await Promise.all([
      getJSON("/api/status"), getJSON(`/api/targets?${q}`), getJSON(`/api/where?${q}`),
    ]);
    const series = await Promise.all(summary.targets.map((t) => getJSON(`/api/targets/${t.id}/series?${q}`)));
    if (!summary.targets.some((t) => t.id === S.target)) S.target = summary.targets[0]?.id ?? null;
    const hops = S.target == null ? null : await getJSON(`/api/targets/${S.target}/hops?${q}`);
    if (gen !== S.gen) return;
    Object.assign(S, { range, status, summary, where, series, hops });
    if (S.sel && (S.sel.to <= range.from || S.sel.from >= range.to)) pick(null);
    renderHeader();
    renderTabs();
    drawTime();
    if (!summary.targets.length) document.getElementById("loss-sub").textContent = "no targets configured";
    renderWhere();
    renderIncidents();
    loadTrace();
  } catch (e) {
    if (gen === S.gen) renderHealth(e.message);
  }
  scheduleRefresh();
}

function scheduleRefresh() {
  clearTimeout(S.timer);
  if (!readRange().key) return;
  let ms = (REFRESH_AT_MS - (Date.now() % 60000) + 60000) % 60000;
  if (ms < 2000) ms += 60000;
  S.timer = setTimeout(load, ms);
}

/* ---------- wiring ---------- */

document.addEventListener("DOMContentLoaded", () => {
  palette();
  renderHeader();
  bindPanel("loss", "loss", () => STRIP_H);
  bindPanel("lat", "lat", () => STRIP_H);
  bindPanel("heat", "heat", () => S.heatRow || HEAT_ROW_MIN);
  const clock = document.getElementById("clock");
  const tick = () => { clock.textContent = new Date().toLocaleTimeString(); };
  tick();
  setInterval(() => { tick(); if (S.status) renderHealth(); }, 1000);
  load();
});

addEventListener("hashchange", () => {
  const p = hashParams();
  const r = readRange();
  const same = S.range && r.key === S.range.key && (r.key || (r.from === S.range.from && r.to === S.range.to));
  if (same && !p.has("from")) { readState(); return; }
  load();
});

let resizeQueued = false;
addEventListener("resize", () => {
  if (resizeQueued) return;
  resizeQueued = true;
  requestAnimationFrame(() => { resizeQueued = false; drawTime(); });
});

addEventListener("keydown", (ev) => {
  if (ev.ctrlKey || ev.metaKey || ev.altKey || ev.target.closest("input, textarea, select")) return;
  const n = parseInt(ev.key, 10);
  if (n >= 1 && n <= RANGE_KEYS.length) goRange({ r: RANGE_KEYS[n - 1] });
  else if (ev.key === "ArrowLeft") cycleTarget(-1);
  else if (ev.key === "ArrowRight") cycleTarget(1);
  else if (ev.key === "l") setMode(S.mode === "loss" ? "lat" : "loss");
  else if (ev.key === "Escape") pick(null);
  else return;
  ev.preventDefault();
});
