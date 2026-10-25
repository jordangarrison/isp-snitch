// Overview: one card per target with a loss bar chart and a latency line.
"use strict";

let plots = [];

function lossChart(el, s, sync) {
  const max = Math.max(1, ...s.loss_pct.filter((v) => v != null));
  return new uPlot({
    width: chartWidth(el), height: 140,
    cursor: { sync: { key: sync }, drag: { x: true, y: false } },
    scales: { x: { time: true }, y: { range: [0, max] } },
    axes: axes((v) => v + "%"),
    series: [{}, {
      label: "Loss", fill: css("--series-loss"), stroke: css("--series-loss"),
      paths: uPlot.paths.bars({ size: [0.9, 24], align: 1 }), points: { show: false },
    }],
    hooks: zoomHooks(),
    plugins: [tooltipPlugin("Target loss", fmtPct)],
  }, [s.ts, s.loss_pct], el);
}

function latencyChart(el, s, sync) {
  return new uPlot({
    width: chartWidth(el), height: 140,
    cursor: { sync: { key: sync }, drag: { x: true, y: false } },
    scales: { x: { time: true }, y: { range: (u, lo, hi) => [0, Math.max(10, hi * 1.1)] } },
    axes: axes((v) => v + " ms"),
    series: [{}, { label: "Avg latency", stroke: css("--series-lat"), width: 2, points: { show: false } }],
    hooks: zoomHooks(),
    plugins: [tooltipPlugin("Avg latency", fmtMs)],
  }, [s.ts, s.avg_ms], el);
}

async function render() {
  const root = document.getElementById("targets");
  renderRangeButtons(document.getElementById("ranges"), render);
  const { from, to } = readRange();
  plots.forEach((p) => p.destroy());
  plots = [];
  let summary;
  try {
    summary = await getJSON(`/api/targets?from=${from}&to=${to}`);
  } catch (e) {
    root.innerHTML = `<p class="empty error">Could not load targets: ${e.message}</p>`;
    return;
  }
  root.innerHTML = "";
  for (const t of summary.targets) {
    const card = document.createElement("section");
    card.className = "card";
    const href = `/target/${t.id}${location.hash}`;
    card.innerHTML = `
      <div class="head">
        <h2><a href="${href}">${t.name}</a></h2><span class="addr">${t.addr}</span>
        <span class="spacer"></span>
        <div class="stats">
          <span>Loss <b>${fmtPct(t.loss_pct)}</b></span>
          <span>Avg <b>${fmtMs(t.avg_ms)}</b></span>
          <span>Worst <b>${fmtMs(t.max_ms)}</b></span>
          <span>Buckets with loss <b>${t.lossy_buckets}</b></span>
        </div>
      </div>
      <p class="chart-label">Target loss, per ${fmtStep(summary.step)}</p><div class="chart loss"></div>
      <p class="chart-label">Average latency</p><div class="chart lat"></div>`;
    root.appendChild(card);
    if (t.loss_pct == null) {
      card.querySelector(".loss").innerHTML = `<p class="empty">No data in this range.</p>`;
      card.querySelector(".lat").remove();
      card.querySelectorAll(".chart-label")[1].remove();
      continue;
    }
    const s = await getJSON(`/api/targets/${t.id}/series?from=${from}&to=${to}`);
    plots.push(lossChart(card.querySelector(".loss"), s, "ov"));
    plots.push(latencyChart(card.querySelector(".lat"), s, "ov"));
  }
  if (!summary.targets.length) root.innerHTML = `<p class="empty">No targets configured.</p>`;
}

addEventListener("resize", () => {
  for (const p of plots) p.setSize({ width: chartWidth(p.root.parentElement), height: p.height });
});
addEventListener("mouseleave", () => tooltip.hide());
document.addEventListener("DOMContentLoaded", render);
