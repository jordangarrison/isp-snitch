# Architecture

isp-snitch is one Rust binary with three parts: tracer threads that probe,
an aggregator that writes per-minute rows to SQLite, and an HTTP server that
serves a JSON API and the UI.

```
 tracer thread (1 per target)          tokio runtime
 +---------------------------+        +----------------------------------+
 | PingSocket (enp5s0)       |        | aggregate_loop                   |
 |   TracerCore (rounds) ----+--mpsc--+->  MinuteAggregator              |
 |   runner::run             |        |    flush at minute + 10 s ----+  |
 +---------------------------+        |                               v  |
                                      | maintenance_loop (hourly)  Store |
                                      |    rollup_hours, prune ---> (SQLite)
                                      |                               ^  |
                                      | axum: /api/*, /, /assets/* ---+  |
                                      +----------------------------------+
```

## Probing

Each target has one OS thread and one unprivileged ICMP (Internet Control
Message Protocol) "ping socket": `SOCK_DGRAM` with `IPPROTO_ICMP`. The socket
is bound to the configured interface with `SO_BINDTODEVICE`. Every
`interval_ms`, the thread starts a **round**: one echo request per TTL (time
to live) from 1 to the **horizon**, spaced `probe_spacing_ms` apart.

- Routers that drop a probe whose TTL ran out send ICMP Time Exceeded. The
  kernel queues it on the socket error queue (`IP_RECVERR`), with the
  router's address and the original echo header, so the sequence number
  matches it to the probe.
- The target answers with an echo reply on the normal queue.
- A probe with no answer within `timeout_ms` is lost.

`TracerCore` (`src/tracer/mod.rs`) holds the round logic and has no I/O.
`runner::run` drives it with a `Transport`. Rounds overlap because the
timeout is longer than the interval, and they complete in start order. When
a round completes:

- the target distance `d` is the lowest TTL the target answered;
- probes past `d` are dropped, so probing past the target never makes fake
  hops;
- the horizon for new rounds becomes `d`. After a round with no target
  answer it becomes `max_ttl`, so a longer path is found again;
- `d` only grows when a router answered at the old distance. A lost probe at
  the old distance plus an echo further out is loss, not a longer path.

If the socket cannot be opened (for example, the interface is gone), each
round is recorded as send errors at every TTL up to the known distance, and
the socket is reopened every 5 s. An outage of the link shows as 100% loss
on every hop, not as a gap.

## Storage

`MinuteAggregator` folds rounds into one row per (target, minute, TTL):
`sent`, `recv`, RTT (round-trip time) sum, sum of squares, min and max, the
most common responder and the number of distinct responders. Rows are
written 10 s after each minute and on shutdown. The upsert is additive, so a
late row for a minute adds to it.

`hop_hour` holds hourly rollups, recomputed from `hop_minute` every hour.
Retention deletes minute rows after `minute_days` and hour rows after
`hour_days`. See ADR 0002.

## Analysis and API

The API (`src/web/mod.rs`, documented in `docs/api.md`) picks a bucket
`step` from a fixed ladder so a range has at most 720 buckets. It reads
buckets from the minute or hour table and runs `analysis::analyze` per
bucket:

- **loss** = `(sent - recv) / sent` per hop;
- **effective loss** = the lowest loss of this hop and every later hop. Real
  loss carries through to the target. Loss at one hop only is ICMP rate
  limiting on that router;
- **rate_limited** = raw minus effective loss is at least 5 points, with at
  least 10 probes.

## UI

Plain HTML, CSS and JavaScript in `assets/`, compiled into the binary with
`include_bytes!`. There is no build step. uPlot draws the line and bar
charts. A `<canvas>` draws the hop x time heatmap. The range lives in the URL
hash (`#r=24h`, or `#from=..&to=..`, plus `&at=..` for a selected bucket), so
every view can be shared as a link. See ADR 0003.

## Logging

`src/wide.rs` writes one JSON line per unit of work to stdout: `startup`,
`http_request`, `probe_flush`, `maintenance`, `transport_open_failed`,
`transport_recovered`, `shutdown`. Under systemd, journald keeps them.
