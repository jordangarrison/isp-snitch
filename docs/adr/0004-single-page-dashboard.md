# ADR 0004: Single-page btop-style dashboard replaces the overview

Status: accepted, 2026-10-25. Supersedes the `/` overview in ADR 0003 and
the spec's "Web UI" pages list.

## Context

The owner likes btop: dense, all the information on one screen. The
original `/` overview showed one card per target (loss bars and a latency
line), and the answer to "where does loss start" needed a click into each
target page and a reading of its heatmap.

## Decision

- `/` is one dark, terminal-style page with six boxes: loss and latency
  (one strip per target), the hop heatmap of a selected target, **where**
  (which segment and hop the loss starts at, across all targets),
  **incidents** (runs of lossy buckets) and **trace** (an mtr table, live
  or for a picked time).
- The loss, latency and hop panels share one x layout, so a crosshair,
  a picked window and drag-to-zoom line up across them.
- Graphs are drawn on `<canvas>` as dot-matrix columns, colored by level.
  The loss and latency ramps are the same ones as before (sequential, one
  hue each), so color keeps one meaning on every page. Loss strips use a
  log scale (0.1% to 100%) so that both 0.3% and 30% are visible.
- The page is dark only, by design: it emulates a terminal. The
  `/target/{id}` page stays as the detailed view and still follows the
  system theme.
- "Where" is computed on the server (`src/blame.rs`, `GET /api/where`), so
  the blame rules have unit tests:
  - each hop's **added loss** is its effective loss minus the previous
    hop's; summed over a path it is the target's loss;
  - **gateway** is hop 1; **upstream** is hop 2 up to the last hop that
    every target's path shares (the split); **past split** is after it.
    The split needs no ASN (autonomous system number) data: the shared part
    of the paths is your access network and ISP.
  - an **incident** is a run of consecutive buckets in which a target lost
    1% or more. Its origin is the hop with the most added loss in the run.
- uPlot is no longer used by `/`; the target page still uses it.

## Consequences

- One page answers "is it me, my ISP, or them" and "how often" without
  clicks. The rest stays one click away (`detail` link).
- The incident threshold is per bucket, so the count depends on the step:
  at a 1-minute step one lost probe in 60 is 1.7% and counts; at 5 minutes
  it takes 3 lost probes. The UI states the threshold and the step.
- With one target there is no split: everything after hop 1 is "upstream".
