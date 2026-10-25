# ADR 0003: Embedded UI with no frontend build step

Status: accepted, 2026-10-24

## Context

The owner asked for a lightweight UI. The service ships as one binary (and
later one Docker image). A Node toolchain would add a second dependency tree
and a build step for a few pages.

## Decision

- Plain HTML, CSS and JavaScript in `assets/`, embedded with
  `include_bytes!`. No bundler, no Node.
- uPlot 1.6.32 (MIT, about 50 KB) is vendored in `assets/vendor/uplot/`
  with its license, for line and bar charts.
- The hop x time heatmap is drawn on a `<canvas>`.
- Pages: an overview (`/`) and a per-target page (`/target/{id}`). State
  lives in the URL hash, so views are shareable links.

## Consequences

- Any change under `assets/` needs a rebuild and restart, because files are
  compiled in.
- No JavaScript unit tests. `tests/api.rs::ui_assets_served` checks the
  routes, and headless Chromium screenshots check rendering
  (see `AGENTS.md`).
- Colors are CSS custom properties with separate light and dark values. Loss
  uses a sequential red ramp with a neutral zero. Latency uses a blue ramp.
  Rate-limited cells use a hatch texture, so they do not rely on color only.
