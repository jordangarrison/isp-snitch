# isp-snitch design

Status: approved 2026-10-24 (owner delegated remaining decisions to the builder).

## Intent

Matt's home connection (BigOne, Arch Linux) sometimes loses packets. He wants to
see **where** the loss happens (local link and gateway, or a few hops upstream at
the ISP) and **how often**, with at least a month of history to look back
through. `mtr` answers "where" only live. SmokePing answers "how often" only
end to end. isp-snitch keeps mtr-style per-hop data over time and graphs it.

## Goals

- Always-on service that runs continuous mtr-style traces to a few highly
  reachable targets and records per-hop loss and latency.
- Probes leave through one configured interface (`enp5s0` on BigOne), even when
  another uplink holds the default route.
- At least 30 days of full-detail history (default 90), plus hourly rollups for
  a year.
- Light web UI: an overview of all targets, and a per-target hop heatmap that
  shows where loss starts.
- Self-contained: one Rust binary plus one SQLite file. No external services.
- AI-first repo: agent-agnostic `AGENTS.md`, docs, ADRs, one validation
  command, fast tests.

## Non-goals (v1)

- Alerts or notifications.
- Multiple interfaces or uplinks at the same time.
- Markers for `net-failover` events.
- IPv6 targets (`enp5s0` has no global IPv6). Config rejects them with a clear
  error.
- Login or auth. The UI listens on loopback by default.
- OTLP export to SigNoz. Logs go to stdout as JSON, and journald keeps them.

## Anti-goals

- The service must never change routing or network state. It only observes.
- Rate-limited routers must not show up as "the problem hop". The UI must not
  point at a hop whose loss does not carry through to later hops.
- Service downtime must not look like packet loss. No data is a gap, not 100%
  loss.

## Constraints

- Dev environment: devbox (`devbox.json`). Every tool used in dev comes from it.
- Runs on BigOne (Arch, systemd, NetworkManager) now. Later it runs as a Docker
  image built from a Nix flake on a NAS or Proxmox. Code must not depend on
  systemd or Arch.
- BigOne has two uplinks on the same subnet with the same gateway IP
  (192.168.1.1): `enp5s0` goes to the ISP and `wlo1` goes to Starlink.
  `net-failover` raises `enp5s0`'s route metric during failover. A source-IP
  bind is not enough to pin probes. The probe socket must use
  `SO_BINDTODEVICE`.

## Decisions

### Probing (ADR 0001)

- Custom ICMP echo tracer, not `trippy-core` or the `mtr` binary.
  `trippy-core` 0.13 `interface()` only selects a source address (checked in
  source: `net/platform/unix.rs` `lookup_interface_addr`). It does not bind to
  the device.
- Socket: `socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)` (unprivileged "ping
  socket"), plus `SO_BINDTODEVICE`, `IP_TTL` per probe, and `IP_RECVERR`. TTL
  Exceeded replies arrive on the socket error queue (`MSG_ERRQUEUE`), with
  `sock_extended_err` and the offender address. Echo replies arrive as normal
  datagrams. Verified on BigOne as uid 1000 through both `enp5s0` and `wlo1`.
- Needs `net.ipv4.ping_group_range` to include the service's group. BigOne has
  `0 2147483647` (systemd default). No capabilities are needed. Since Linux 5.7,
  `SO_BINDTODEVICE` works without `CAP_NET_RAW`.
- Docker trap: with `--network host`, the container uses the **host's**
  `ping_group_range`, and `--sysctl` cannot change it. NAS distros may keep the
  kernel default `1 0`, which blocks ping sockets. The deploy docs state the
  host requirement. v1 ships only the ping-socket transport, behind a
  `Transport` trait, so a `SOCK_RAW` + `CAP_NET_RAW` transport can be added
  later without touching the round logic. (Builder's call.)
- `interface` is optional. When it is unset, probes follow the routing table
  and no device bind happens (for a single-NIC host like a NAS). BigOne's
  config sets `enp5s0`.
- One socket and one OS thread per target. The kernel picks the ICMP id per
  socket. The tracer matches replies by sequence number. Errors from the error
  queue carry the original echo header, so they are matched the same way.
- Rounds: every `interval_ms` (default 1000), send one probe per TTL from 1 to
  the horizon, spaced `probe_spacing_ms` apart (default 5). A probe with no
  answer within `timeout_ms` (default 2000) counts as lost. A round is
  *completed* when every probe has an answer or has timed out. Rounds overlap,
  because the timeout is longer than the interval.
- Horizon (self-healing): the horizon for a new round is the target distance
  `d` from the most recent completed round. `d` = the lowest TTL that got an
  echo reply. If that round got no echo reply, the horizon is `max_ttl`
  (default 30). A longer path then gets found again within one or two rounds,
  instead of showing false 100% loss.
- Truncation: when a round completes, probes with TTL > `d_round` (the lowest
  echo-reply TTL in that round) are dropped from stats. If the round had no
  echo reply, probes with TTL > `d_known` (the last known distance) are
  dropped. If no distance has been known yet, all probes are kept. Probing past
  the target therefore never creates fake hops or fake loss rows.
- Distance only grows when a router answers at the old distance. The target
  answers every TTL at or past its distance. So if the probe at `d_known` is
  lost and a higher-TTL probe gets an echo, that is loss at the target, not a
  longer path: the round is cut at `d_known` and the horizon stays.
  (Found in live data on 2026-10-24, during a real 33% loss event.)
- A send error (interface down, no route) counts as a sent probe with no
  answer and increments `send_errors`. A round with no usable socket is
  recorded at every TTL up to the known distance (TTL 1 if none is known), so
  an outage shows as loss on every hop and at the target, not as rate
  limiting at hop 1. Such rounds do not change the horizon.

### Storage (ADR 0002)

- SQLite via `rusqlite` with the `bundled` feature, WAL mode. Schema version
  lives in `PRAGMA user_version`, with forward-only migrations in code.
- `hop_minute`: one row per (target, minute, ttl) with `sent`, `recv`,
  `rtt_sum_us`, `rtt_sumsq_us2`, `rtt_min_us`, `rtt_max_us`, `addr` (the
  responder seen most often), `addr_count` (distinct responders), `reached`
  (that TTL got an echo reply from the target), and `send_errors`.
  `WITHOUT ROWID`, primary key `(target_id, ts, ttl)`.
- `hop_hour`: same columns, rolled up from `hop_minute`, written for each
  completed hour.
- `targets`: `id`, `addr`, `name`.
- Retention job (hourly): delete `hop_minute` rows older than `minute_days`
  (default 90) and `hop_hour` rows older than `hour_days` (default 365).
- Size estimate: 3 targets x ~12 hops x 1440 minutes x 90 days = ~4.7M rows,
  a few hundred MB.

### Analysis

- **Effective loss** of hop h in a bucket = min(raw loss of hops h..target).
  Real loss carries through to the target. Loss that only one hop shows is ICMP
  rate limiting on that router.
- `rate_limited` flag = raw loss minus effective loss is at least 5 percentage
  points, and the hop has at least 10 probes sent.
- Server-side downsampling: pick the smallest step from
  [60, 300, 900, 1800, 3600, 10800, 21600, 86400] seconds that keeps buckets at
  or under `max_points` (default 720). Use `hop_hour` when the step is 3600 or
  more, or when the range starts before the minute retention cutoff.

### Web UI (ADR 0003)

- `axum` serves a JSON API and static files embedded in the binary with
  `include_str!` / `include_bytes!`. No frontend build step and no Node.
- uPlot (MIT) is vendored under `assets/vendor/` with its license, for line
  and bar charts. The heatmap is drawn on a plain `<canvas>`.
- Pages (superseded for `/` by ADR 0004: `/` is now a single-page btop-style
  dashboard with loss, latency, hops, where, incidents and trace boxes):
  - `/`: overview. One strip per target: a loss bar plus a latency line on one
    time axis, and summary stats (loss %, avg and worst latency, number of
    minutes with loss). Range picker (6h, 24h, 7d, 30d, 90d). Drag to zoom.
    Click a target to open its page.
  - `/target/{id}`: target loss bar on top, then a hop x time heatmap colored
    by effective loss, with rate-limited cells hatched. A tab switches the
    heatmap to latency. Click a column to see the mtr-style table for that
    bucket (hop, IP, loss, effective loss, avg, worst, and a marker where the
    responder IP changed from the previous bucket).
- API:
  - `GET /api/targets?from&to` lists targets with summary stats.
  - `GET /api/targets/{id}/series?from&to` gives target-hop loss and latency
    per bucket.
  - `GET /api/targets/{id}/hops?from&to` gives a hop x bucket matrix.
  - `GET /api/targets/{id}/slice?from&to` gives the mtr table for one window.
  - `GET /healthz`.
- Theme follows the system light or dark preference on `/target/{id}`. The
  `/` dashboard is dark only (ADR 0004).

### Config

TOML file, path given with `--config`. Validated at startup. Fields:
`interface` (optional), `listen` (default `127.0.0.1:9180`), `db_path`, a `[probe]` table
(`interval_ms`, `timeout_ms`, `max_ttl`, `probe_spacing_ms`), a `[retention]`
table (`minute_days`, `hour_days`), and `[[targets]]` (`name`, `addr`).
Defaults: Cloudflare 1.1.1.1, Google 8.8.8.8, Quad9 9.9.9.9.

### CLI

- `isp-snitch run --config <path>` runs the service.
- `isp-snitch check-config --config <path>` validates the config and exits.
- `isp-snitch trace --interface <if> <addr> [--rounds N]` runs a one-shot
  trace and prints an mtr-style table. Used for debugging and as live proof.

### Logging

Wide-event format (Projects/personal standard): JSON to stdout, `snake_case`
fields, one event per HTTP request (`method`, `path`, `status`, `duration_ms`,
`outcome`), one event per minute flush (`action=probe_flush`, with per-target
counts), and one event per rollup or retention run. `/healthz` is not logged.
`service=isp-snitch`.

### Packaging

- `deploy/systemd/isp-snitch.service`: `DynamicUser=yes`,
  `StateDirectory=isp-snitch`, no capabilities, strict sandboxing.
  `scripts/install.sh` (needs sudo) installs the binary, config and unit.
- `flake.nix`: `packages.default` (the binary via `rustPlatform`) and
  `packages.docker` (`dockerTools.buildLayeredImage`). The container needs
  `--network host` to see `enp5s0`, and the host needs
  `net.ipv4.ping_group_range` to cover the container's group (see Probing).

## Testing

- Unit: packet encode and decode, error-queue parsing, tracer round logic with
  a fake transport (horizon, timeouts, matching, send errors), the minute
  aggregator, analysis (effective loss, rate-limit flag, step picking), config
  validation, store migrations, insert, rollup and retention on in-memory
  SQLite.
- Integration: HTTP API against a seeded temp DB.
- Live: trace 127.0.0.1 over `lo` (no network needed, no privileges).
- One validation command: `devbox run check` runs fmt check, clippy with
  `-D warnings`, and tests. CI runs the same command.

## Left to the builder

Module layout, crate choices beyond those named above, exact CSS, colors and
heatmap scale, and the API JSON shapes (documented in `docs/api.md`).
