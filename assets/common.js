// Shared helpers: time range state (kept in the URL hash), fetch, formatting,
// theme-aware colors and a tooltip element.
"use strict";

const RANGES = { "6h": 6 * 3600, "24h": 86400, "7d": 7 * 86400, "30d": 30 * 86400, "90d": 90 * 86400 };

function nowSec() { return Math.floor(Date.now() / 1000); }

/** Escapes text for use inside innerHTML. Every server or config value goes through it. */
function esc(v) {
  return String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

/** Replaces el's content with one message paragraph, set as text. */
function showMessage(el, text, isError) {
  const p = document.createElement("p");
  p.className = isError ? "empty error" : "empty";
  p.textContent = text;
  el.replaceChildren(p);
}

/** x scale covering whole buckets, so bars line up with heatmap columns. */
function bucketScale(ts, step) {
  return { time: true, range: () => [ts[0], ts[ts.length - 1] + step] };
}

/** Current range from the hash: #r=24h or #from=..&to=.. */
function readRange() {
  const p = new URLSearchParams(location.hash.slice(1));
  const from = parseInt(p.get("from"), 10), to = parseInt(p.get("to"), 10);
  if (from && to && from < to) return { key: null, from, to };
  const key = RANGES[p.get("r")] ? p.get("r") : "24h";
  const t = nowSec();
  return { key, from: t - RANGES[key], to: t };
}

function setRangeKey(key) { location.hash = "r=" + key; }
function setRangeAbs(from, to) { location.hash = `from=${Math.floor(from)}&to=${Math.ceil(to)}`; }

/** Selected heatmap bucket start (unix seconds) from the hash, if any. */
function readAt() {
  const v = parseInt(new URLSearchParams(location.hash.slice(1)).get("at"), 10);
  return Number.isFinite(v) ? v : null;
}

/** Sets or clears `at` in the hash without triggering a re-render. */
function writeAt(at) {
  const p = new URLSearchParams(location.hash.slice(1));
  if (at == null) p.delete("at"); else p.set("at", String(at));
  history.replaceState(null, "", "#" + p.toString());
}

function renderRangeButtons(el, onChange) {
  const cur = readRange();
  el.innerHTML = "";
  for (const key of Object.keys(RANGES)) {
    const b = document.createElement("button");
    b.textContent = key;
    b.setAttribute("aria-pressed", String(cur.key === key));
    b.onclick = () => setRangeKey(key);
    el.appendChild(b);
  }
  const label = document.getElementById("range-label");
  if (label) label.textContent = cur.key ? "" : `${fmtTime(cur.from)} to ${fmtTime(cur.to)}`;
  window.onhashchange = onChange;
}

async function getJSON(url) {
  const r = await fetch(url);
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body.error || `HTTP ${r.status}`);
  return body;
}

function fmtTime(sec) {
  return new Date(sec * 1000).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}
function fmtPct(v) { return v == null ? "-" : (v < 10 && v > 0 ? v.toFixed(2) : v.toFixed(1)) + "%"; }
function fmtMs(v) { return v == null ? "-" : v.toFixed(1) + " ms"; }
function fmtStep(s) { return s < 3600 ? `${s / 60} min` : s < 86400 ? `${s / 3600} h` : `${s / 86400} d`; }

function css(name) { return getComputedStyle(document.documentElement).getPropertyValue(name).trim(); }

const LOSS_EDGES = [1, 3, 10, 30, 100];
const LAT_EDGES = [10, 20, 40, 80, 150];
function lossColor(v) {
  if (v <= 0) return css("--cell-zero");
  const i = v >= 100 ? 6 : LOSS_EDGES.findIndex((e) => v <= e) + 1;
  return css("--loss-" + i);
}
function latColor(v) {
  const i = LAT_EDGES.findIndex((e) => v < e);
  return css("--lat-" + (i === -1 ? 6 : i + 1));
}

const tooltip = (() => {
  const el = document.createElement("div");
  el.className = "tooltip";
  document.addEventListener("DOMContentLoaded", () => document.body.appendChild(el));
  return {
    show(html, x, y) {
      el.innerHTML = html;
      el.style.display = "block";
      const w = el.offsetWidth, h = el.offsetHeight;
      el.style.left = Math.min(x + 14, innerWidth - w - 8) + "px";
      el.style.top = Math.max(8, y - h - 10) + "px";
    },
    hide() { el.style.display = "none"; },
  };
})();

/** uPlot axis styling from the theme tokens. */
function axes(yLabelFmt) {
  const common = { stroke: css("--muted"), grid: { stroke: css("--grid"), width: 1 }, ticks: { show: false } };
  return [
    { ...common, space: 70 },
    { ...common, size: 56, space: 24, values: (u, vals) => vals.map(yLabelFmt) },
  ];
}

/** uPlot plugin: tooltip following the cursor for a single series. */
function tooltipPlugin(label, fmt) {
  return {
    hooks: {
      setCursor: (u) => {
        const i = u.cursor.idx;
        if (i == null || u.cursor.left < 0) return tooltip.hide();
        const v = u.data[1][i];
        const rect = u.over.getBoundingClientRect();
        tooltip.show(`<div class="t">${fmtTime(u.data[0][i])}</div>${label}: <b>${v == null ? "no data" : fmt(v)}</b>`,
          rect.left + u.cursor.left, rect.top + u.cursor.top);
      },
    },
  };
}

/** uPlot drag-to-zoom: sets an absolute range instead of zooming in place. */
function zoomHooks() {
  return {
    setSelect: [(u) => {
      if (u.select.width < 5) return;
      const from = u.posToVal(u.select.left, "x");
      const to = u.posToVal(u.select.left + u.select.width, "x");
      u.setSelect({ left: 0, width: 0 }, false);
      setRangeAbs(from, to);
    }],
  };
}

function chartWidth(el) { return Math.max(280, el.clientWidth); }
