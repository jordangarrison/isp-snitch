// Target page: target loss bars, a hop x time heatmap, and the hop table for
// a clicked time bucket.
"use strict";

const LABEL_W = 170;
const AXIS_H = 22;
const targetId = location.pathname.split("/").pop();

let lossPlot = null;
let hops = null;
let mode = "loss";
let selected = null;

function rowHeight() { return innerWidth < 600 ? 16 : 20; }

function hatchPattern(ctx) {
  const c = document.createElement("canvas");
  c.width = c.height = 6;
  const g = c.getContext("2d");
  g.fillStyle = css("--hatch-b");
  g.fillRect(0, 0, 6, 6);
  g.strokeStyle = css("--hatch-a");
  g.lineWidth = 1.5;
  g.beginPath();
  g.moveTo(-1, 7); g.lineTo(7, -1);
  g.moveTo(5, 7); g.lineTo(7, 5);
  g.moveTo(-1, 1); g.lineTo(1, -1);
  g.stroke();
  return ctx.createPattern(c, "repeat");
}

function geometry(canvas) {
  const w = canvas.clientWidth;
  const n = hops.ts.length;
  const colW = (w - LABEL_W) / Math.max(1, n);
  return { w, n, colW, rowH: rowHeight() };
}

function drawHeat() {
  const canvas = document.getElementById("heat");
  if (!hops || !hops.hops.length) {
    canvas.style.height = "0px";
    return;
  }
  const dpr = devicePixelRatio || 1;
  const { w, n, colW, rowH } = geometry(canvas);
  const h = hops.hops.length * rowH + AXIS_H;
  canvas.style.height = h + "px";
  canvas.width = Math.round(w * dpr);
  canvas.height = Math.round(h * dpr);
  const ctx = canvas.getContext("2d");
  ctx.scale(dpr, dpr);
  ctx.clearRect(0, 0, w, h);
  const hatch = hatchPattern(ctx);
  const gap = colW > 4 ? 1 : 0;
  ctx.font = `12px ${css("--font")}`;
  ctx.textBaseline = "middle";

  hops.hops.forEach((hop, r) => {
    const y = r * rowH;
    const last = r === hops.hops.length - 1;
    ctx.fillStyle = last ? css("--ink") : css("--ink-2");
    const label = `${String(hop.ttl).padStart(2, " ")}  ${hop.addr || "???"}`;
    ctx.fillText(label, 4, y + rowH / 2, LABEL_W - 10);
    for (let i = 0; i < n; i++) {
      const x = LABEL_W + i * colW;
      let fill = null;
      if (mode === "loss") {
        const eff = hop.eff_loss_pct[i];
        if (eff == null) continue;
        fill = hop.rate_limited[i] && eff === 0 ? hatch : lossColor(eff);
      } else {
        const ms = hop.avg_ms[i];
        if (ms == null) continue;
        fill = latColor(ms);
      }
      ctx.fillStyle = fill;
      ctx.fillRect(x, y + 1, Math.max(1, colW - gap), rowH - 2);
    }
  });

  // Time axis.
  ctx.fillStyle = css("--muted");
  ctx.textBaseline = "top";
  const ticks = Math.max(2, Math.floor((w - LABEL_W) / 140));
  for (let k = 0; k <= ticks; k++) {
    const i = Math.min(n - 1, Math.round((k / ticks) * (n - 1)));
    const x = LABEL_W + i * colW;
    const text = fmtTime(hops.ts[i]);
    const tw = ctx.measureText(text).width;
    ctx.fillText(text, Math.min(Math.max(LABEL_W, x - tw / 2), w - tw), hops.hops.length * rowH + 5);
  }

  if (selected != null) {
    ctx.strokeStyle = css("--ink");
    ctx.lineWidth = 2;
    ctx.strokeRect(LABEL_W + selected * colW, 0, Math.max(2, colW), hops.hops.length * rowH);
  }
}

function drawLegend() {
  const el = document.getElementById("legend");
  if (mode === "loss") {
    const steps = ["0%", "≤1%", "≤3%", "≤10%", "≤30%", "<100%", "100%"];
    el.innerHTML = steps.map((s, i) =>
      `<span><span class="sw" style="background:${i === 0 ? css("--cell-zero") : css("--loss-" + i)}"></span>${s}</span>`).join("")
      + `<span><span class="sw hatch"></span>rate-limited (not real loss)</span><span>blank: no data</span>`;
  } else {
    const steps = ["<10 ms", "<20", "<40", "<80", "<150", "≥150 ms"];
    el.innerHTML = steps.map((s, i) =>
      `<span><span class="sw" style="background:${css("--lat-" + (i + 1))}"></span>${s}</span>`).join("")
      + `<span>blank: no answer or no data</span>`;
  }
}

function cellAt(ev) {
  const canvas = document.getElementById("heat");
  const rect = canvas.getBoundingClientRect();
  const { n, colW, rowH } = geometry(canvas);
  const x = ev.clientX - rect.left, y = ev.clientY - rect.top;
  if (x < LABEL_W) return null;
  const i = Math.floor((x - LABEL_W) / colW);
  const r = Math.floor(y / rowH);
  if (i < 0 || i >= n || r < 0 || r >= hops.hops.length) return null;
  return { i, r };
}

function onHeatMove(ev) {
  const c = hops && cellAt(ev);
  if (!c) return tooltip.hide();
  const hop = hops.hops[c.r];
  const t = hops.ts[c.i];
  const loss = hop.loss_pct[c.i];
  const lines = [`<div class="t">${fmtTime(t)} (${fmtStep(hops.step)})</div>`,
    `Hop ${hop.ttl} <b>${hop.addr || "???"}</b>`];
  if (loss == null) lines.push("no data");
  else {
    lines.push(`Effective loss <b>${fmtPct(hop.eff_loss_pct[c.i])}</b>`);
    lines.push(`Raw loss ${fmtPct(loss)}${hop.rate_limited[c.i] ? " (rate-limited)" : ""}`);
    lines.push(`Avg ${fmtMs(hop.avg_ms[c.i])}`);
  }
  tooltip.show(lines.join("<br>"), ev.clientX, ev.clientY);
}

async function onHeatClick(ev) {
  const c = hops && cellAt(ev);
  if (!c) return;
  selected = c.i;
  drawHeat();
  const from = hops.ts[c.i];
  await loadSlice(from, from + hops.step);
}

async function loadSlice(from, to) {
  const el = document.getElementById("slice");
  document.getElementById("slice-time").textContent = `${fmtTime(from)} to ${fmtTime(to)}`;
  let s;
  try {
    s = await getJSON(`/api/targets/${targetId}/slice?from=${from}&to=${to}`);
  } catch (e) {
    el.innerHTML = `<p class="empty error">${e.message}</p>`;
    return;
  }
  if (!s.hops.length) {
    el.innerHTML = `<p class="empty">No data in this window.</p>`;
    return;
  }
  const rows = s.hops.map((h) => `
    <tr>
      <td>${h.ttl}</td>
      <td>${h.addr || "???"}${h.addr_changed ? '<span class="tag">route changed</span>' : ""}${h.reached ? '<span class="tag">target</span>' : ""}</td>
      <td>${fmtPct(h.eff_loss_pct)}</td>
      <td class="${h.rate_limited ? "rl" : ""}">${fmtPct(h.loss_pct)}${h.rate_limited ? '<span class="tag">rate-limited</span>' : ""}</td>
      <td>${h.sent}</td>
      <td>${fmtMs(h.avg_ms)}</td>
      <td>${fmtMs(h.min_ms)}</td>
      <td>${fmtMs(h.max_ms)}</td>
    </tr>`).join("");
  el.innerHTML = `<table class="mtr"><thead><tr>
      <th>Hop</th><th>Host</th><th>Eff. loss</th><th>Raw loss</th><th>Sent</th><th>Avg</th><th>Best</th><th>Worst</th>
    </tr></thead><tbody>${rows}</tbody></table>`;
}

function drawLossChart(s) {
  const el = document.getElementById("loss");
  if (lossPlot) lossPlot.destroy();
  lossPlot = null;
  if (s.loss_pct.every((v) => v == null)) {
    el.innerHTML = `<p class="empty">No data in this range.</p>`;
    return;
  }
  el.innerHTML = "";
  const max = Math.max(1, ...s.loss_pct.filter((v) => v != null));
  const ax = axes((v) => v + "%");
  ax[1].size = LABEL_W;
  lossPlot = new uPlot({
    width: chartWidth(el), height: 130, padding: [8, 0, 0, 0],
    cursor: { drag: { x: true, y: false } },
    scales: { x: { time: true }, y: { range: [0, max] } },
    axes: ax,
    series: [{}, {
      label: "Loss", fill: css("--series-loss"), stroke: css("--series-loss"),
      paths: uPlot.paths.bars({ size: [0.9, 24], align: 1 }), points: { show: false },
    }],
    hooks: zoomHooks(),
    plugins: [tooltipPlugin("Target loss", fmtPct)],
  }, [s.ts, s.loss_pct], el);
}

function setMode(m) {
  mode = m;
  document.getElementById("tab-loss").setAttribute("aria-pressed", String(m === "loss"));
  document.getElementById("tab-lat").setAttribute("aria-pressed", String(m === "lat"));
  drawLegend();
  drawHeat();
}

async function render() {
  renderRangeButtons(document.getElementById("ranges"), render);
  document.getElementById("home").href = "/" + location.hash;
  const { from, to } = readRange();
  selected = null;
  try {
    const [summary, series, h] = await Promise.all([
      getJSON(`/api/targets?from=${from}&to=${to}`),
      getJSON(`/api/targets/${targetId}/series?from=${from}&to=${to}`),
      getJSON(`/api/targets/${targetId}/hops?from=${from}&to=${to}`),
    ]);
    const t = summary.targets.find((x) => String(x.id) === targetId);
    if (t) {
      document.getElementById("crumb").textContent = `${t.name} (${t.addr})`;
      document.title = `${t.name} - isp-snitch`;
    }
    document.getElementById("step").textContent = `per ${fmtStep(series.step)}`;
    hops = h;
    drawLossChart(series);
    drawLegend();
    drawHeat();
    if (!h.hops.length) document.getElementById("legend").innerHTML = `<p class="empty">No hop data in this range.</p>`;
  } catch (e) {
    document.querySelector("main").innerHTML = `<p class="empty error">Could not load target: ${e.message}</p>`;
  }
}

document.addEventListener("DOMContentLoaded", () => {
  const heat = document.getElementById("heat");
  heat.addEventListener("mousemove", onHeatMove);
  heat.addEventListener("mouseleave", () => tooltip.hide());
  heat.addEventListener("click", onHeatClick);
  document.getElementById("tab-loss").onclick = () => setMode("loss");
  document.getElementById("tab-lat").onclick = () => setMode("lat");
  addEventListener("resize", () => {
    if (lossPlot) lossPlot.setSize({ width: chartWidth(lossPlot.root.parentElement), height: 130 });
    drawHeat();
  });
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", render);
  render();
});
