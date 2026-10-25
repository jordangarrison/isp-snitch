# HTTP API

All endpoints are `GET` and return JSON. Times are unix seconds (UTC).

## Common query parameters

| Param  | Default        | Rules                                |
|--------|----------------|--------------------------------------|
| `from` | `to - 86400`   | integer, must be before `to`         |
| `to`   | now            | integer; range at most 400 days      |

Both must be between 0 and 4102444800 (year 2100).

Errors: `{"error": "<message>"}` with status 400 (bad params, bad id) or 404
(unknown target id). Database failures return 500.

## Buckets

The server splits `[from, to)` into buckets of `step` seconds. `step` is the
smallest value from `[60, 300, 900, 1800, 3600, 10800, 21600, 86400]` that
keeps the number of buckets at or under 720. Bucket `i` starts at
`floor(from / step) * step + i * step`. The data come from the minute table,
or from the hour table when `step >= 3600` or `from` is older than
`retention.minute_days`.

A bucket with no stored probes is a gap: its values are `null`. Gaps mean
the service was not running. Lost probes are not gaps.

## Loss terms

- `loss_pct`: probes with no answer at that hop, in percent.
- `eff_loss_pct` (effective loss): the lowest `loss_pct` of this hop and
  every later hop up to the target. Real loss carries through to the target.
  Loss at one hop only is that router rate-limiting its ICMP replies.
- `rate_limited`: `loss_pct - eff_loss_pct >= 5` and the hop got at least 10
  probes.
- Target loss: for each stored minute (or hour), the loss at the lowest TTL
  the target answered in that minute; with no answer, every round counts as
  lost. These are summed over the bucket, so a short path change cannot turn
  a rate-limiting router's loss into target loss.

## `GET /healthz`

`200 ok` as plain text. Not logged.

## `GET /api/status`

Not range-based.

```json
{"version": "0.1.0", "interface": "enp5s0", "now": 1792937751, "latest_minute": 1792937640}
```

`interface` is `null` when the config sets none. `latest_minute` is the start
of the newest stored minute (`null` with no data). The UI calls data stale
when `now - (latest_minute + 60)` passes 150 s.

## `GET /api/where`

Where loss starts, and how often it happens, across all targets.

```json
{"from": 1792851351, "to": 1792937751, "step": 300,
 "split_ttl": 6, "loss_pct": 1.22, "targets_total": 3,
 "segments": [
   {"name": "gateway", "first_ttl": 1, "last_ttl": 1, "loss_pct": 0.76, "share_pct": 62.3},
   {"name": "upstream", "first_ttl": 2, "last_ttl": 6, "loss_pct": 0.31, "share_pct": 25.4},
   {"name": "past_split", "first_ttl": 7, "last_ttl": null, "loss_pct": 0.15, "share_pct": 12.3}
 ],
 "hops": [{"ttl": 1, "addr": "192.168.1.1", "loss_pct": 0.76, "share_pct": 62.3}],
 "incidents": [
   {"start": 1792930800, "end": 1792931400, "peak_loss_pct": 8.3, "targets_hit": 3,
    "origin_ttl": 1, "origin_addr": "192.168.1.1", "segment": "gateway"}
 ]}
```

- Each hop's **added loss** in a bucket is its `eff_loss_pct` minus the
  previous hop's. Per target, added loss is averaged over buckets weighted by
  probes sent; then targets are averaged with equal weight. `loss_pct` at the
  top is the sum over hops, the mean target loss.
- `split_ttl`: the last TTL at which every target's most common responder is
  the same address. `null` with fewer than 2 targets with data; then
  `upstream` is open-ended and `past_split` is empty (`first_ttl` `null`).
- `hops[].addr` is set only when every target has the same responder there.
- `share_pct` is that part of all loss (`null` when there is no loss).
- An incident is a run of consecutive buckets in which at least one target
  lost 1% or more. `origin_ttl` is the hop with the most added loss in the
  run. Newest first, at most 200.

## `GET /api/targets`

```json
{
  "from": 1792800000, "to": 1792886400, "step": 300,
  "targets": [
    {"id": 1, "name": "Cloudflare", "addr": "1.1.1.1",
     "loss_pct": 0.42, "avg_ms": 15.1, "max_ms": 177.3, "lossy_buckets": 3}
  ]
}
```

`loss_pct`, `avg_ms` and `max_ms` cover the whole range (`null` with no data).
`lossy_buckets` counts the buckets with target loss above 0.

## `GET /api/targets/{id}/series`

Target loss and latency per bucket. All arrays have the same length as `ts`.

```json
{"step": 60, "ts": [1792886340, ...],
 "loss_pct": [0.0, null, ...], "avg_ms": [15.2, null, ...], "max_ms": [17.0, null, ...]}
```

## `GET /api/targets/{id}/hops`

A hop x bucket matrix for the heatmap. `hops` is ordered by `ttl`. `addr` is
the responder with the most answers over the range. Every array has the same
length as `ts`.

```json
{"step": 60, "ts": [...],
 "hops": [
   {"ttl": 1, "addr": "192.168.1.1",
    "loss_pct": [0.0, ...], "eff_loss_pct": [0.0, ...],
    "rate_limited": [false, ...], "avg_ms": [0.6, ...]}
 ]}
```

## `GET /api/targets/{id}/slice`

An mtr-style table for one window (the UI sends one bucket's bounds).

```json
{"from": 1792886340, "to": 1792886400,
 "hops": [
   {"ttl": 4, "addr": "174.111.74.206", "sent": 60, "recv": 48,
    "loss_pct": 20.0, "eff_loss_pct": 0.0, "added_loss_pct": 0.0, "rate_limited": true, "reached": false,
    "avg_ms": 5.8, "min_ms": 4.5, "max_ms": 7.4, "addr_changed": false}
 ]}
```

`addr_changed` is true when more than one responder answered at that TTL
inside the window (a route change or ECMP).
