# AGENTS.md

Guidance for any coding agent working in this repo. Tool-neutral: nothing here
assumes a specific agent or harness.

## What this is

isp-snitch: an always-on Rust service that runs mtr-style ICMP traces through
one network interface, stores per-hop loss and latency in SQLite, and serves a
small web UI to graph where and when packet loss happens.

## Validate every change

```
devbox run check   # cargo fmt --check, clippy -D warnings, cargo test
```

All tools come from `devbox.json`. Do not install toolchains globally.
