# ADR 0002: SQLite with per-minute and per-hour hop tables

Status: accepted, 2026-10-24

## Context

The owner wants at least a month of history, a self-contained service, and
no external database. Data must keep recording while the internet is down,
so a remote backend (SigNoz, Prometheus remote write) is not an option.

## Decision

- One SQLite file (`rusqlite`, bundled), WAL mode.
- `hop_minute`: one row per (target, minute, TTL) with counts and RTT
  (round-trip time) sums, not raw probes. About 4.7M rows for 3 targets,
  12 hops and 90 days.
- `hop_hour`: the same columns rolled up per hour, kept for a year.
- Additive upsert, so a late round for a minute adds to its row.
- Migrations are a forward-only list in `src/store.rs`, tracked with
  `PRAGMA user_version`.

## Consequences

- Sums (`rtt_sum_us`, `rtt_sumsq_us2`) let any bucket size compute mean and
  standard deviation without raw samples. Percentiles are not possible.
- Queries read the minute table for short ranges and the hour table for
  steps of 1 h or more, or for ranges older than minute retention.
- One process writes. The store uses one mutex-guarded connection. That is
  enough at about 3 writes per minute plus UI reads.
