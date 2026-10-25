# isp-snitch Implementation Plan

> **For agentic workers:** execute task by task, in order. Steps use checkbox
> (`- [ ]`) syntax for tracking. Run `devbox run check` before every commit.

**Goal:** Always-on Rust service that records per-hop ICMP loss and latency
through one interface into SQLite, and serves a light web UI to graph it.

**Architecture:** One OS thread per target runs a ping-socket tracer and sends
completed rounds over a channel. A tokio task aggregates them into per-minute,
per-hop rows and writes them to SQLite. An axum server serves a JSON API and
embedded static pages (uPlot plus a canvas heatmap).

**Tech Stack:** Rust (devbox `rustc`/`cargo` 1.98), `libc`, `tokio`, `axum`,
`rusqlite` (bundled), `serde`/`serde_json`/`toml`, `clap`, `anyhow`. Dev:
`tempfile`, `tower`, `http-body-util`.

**Spec:** `docs/specs/2026-10-24-isp-snitch-design.md`

## Global Constraints

- Linux only. IPv4 only. No capabilities: `SOCK_DGRAM` + `IPPROTO_ICMP`.
- Defaults: `listen = 127.0.0.1:9180`, `interval_ms = 1000`, `timeout_ms = 2000`,
  `max_ttl = 30`, `probe_spacing_ms = 5`, `minute_days = 90`, `hour_days = 365`,
  targets Cloudflare 1.1.1.1, Google 8.8.8.8, Quad9 9.9.9.9.
- Step ladder (seconds): `[60, 300, 900, 1800, 3600, 10800, 21600, 86400]`,
  `max_points = 720`.
- Rate-limit flag: raw loss minus effective loss >= 5 points and sent >= 10.
- Logs: one JSON line per event to stdout, `snake_case`, `service = "isp-snitch"`.
  No `/healthz` events.
- The service never changes routes or network state.
- All dev tools come from devbox. Agent docs stay agent-neutral (`AGENTS.md`;
  `CLAUDE.md` is only a symlink to it).

## Review Focus

1. Interface missing or down (unplugged, `ENODEV`/`ENETDOWN` on open or send):
   the service keeps running, records send errors as loss at TTL 1, and
   retries opening the socket every 5 s. It never exits and never leaves a gap.
2. A range or target with no data: the API returns empty arrays and `null`
   values, the UI shows "no data", and nothing returns 500.
3. Bad query params (`from >= to`, non-numeric, range > 400 days, unknown
   target id): JSON 400 or 404 with a message, never 500.
4. Late, duplicate or unknown-seq replies, and u16 seq wraparound: ignored or
   matched correctly, with no double counting and no panic.
5. `db_path` whose parent directory does not exist: it gets created.

## File Structure

```
Cargo.toml, devbox.json, .gitignore, AGENTS.md, CLAUDE.md -> AGENTS.md, README.md
src/main.rs            CLI: run | check-config | trace
src/lib.rs             module tree
src/wide.rs            wide-event JSON logger
src/config.rs          Config, load, validate
src/icmp/mod.rs        TransportEvent, Transport trait
src/icmp/packet.rs     echo encode, reply + errqueue parsing (pure)
src/icmp/socket.rs     PingSocket: Linux ping-socket Transport
src/tracer/mod.rs      TracerCore: round, horizon, truncation logic (pure)
src/tracer/runner.rs   run loop: Transport + TracerCore -> CompletedRound sink
src/aggregate.rs       MinuteAggregator -> HopMinute rows
src/store.rs           SQLite schema, writes, rollup, prune, reads
src/analysis.rs        step picking, effective loss, target series
src/web/mod.rs         router, handlers, HTTP wide events
src/web/assets.rs      embedded static files
src/service.rs         wiring for `run`, shutdown
assets/*.html|css|js, assets/vendor/uplot/*
tests/api.rs, tests/live_lo.rs
deploy/systemd/isp-snitch.service, deploy/config.example.toml, deploy/config.dev.toml
scripts/install.sh, flake.nix, .github/workflows/ci.yml
docs/{architecture,api,operations}.md, docs/adr/000{1,2,3}-*.md
```

## Shared types (defined once, used everywhere)

```rust
// src/icmp/mod.rs
pub enum TransportEvent {
    EchoReply { seq: u16, from: Ipv4Addr },
    TimeExceeded { seq: u16, from: Ipv4Addr },
    Unreachable { seq: u16, from: Ipv4Addr },
}
pub trait Transport: Send {
    fn send(&mut self, seq: u16, ttl: u8) -> std::io::Result<()>;
    /// Waits at most `timeout`. Returns the event and the instant it was read.
    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<(TransportEvent, Instant)>>;
}

// src/tracer/mod.rs
pub struct ProbeOutcome { pub ttl: u8, pub rtt: Option<Duration>, pub responder: Option<Ipv4Addr>, pub reached: bool, pub send_error: bool }
pub struct CompletedRound { pub target_id: i64, pub started_unix: i64, pub probes: Vec<ProbeOutcome> }

// src/aggregate.rs
pub struct HopMinute { pub target_id: i64, pub ts: i64, pub ttl: u8, pub sent: u32, pub recv: u32,
    pub rtt_sum_us: u64, pub rtt_sumsq_us2: f64, pub rtt_min_us: Option<u32>, pub rtt_max_us: Option<u32>,
    pub addr: Option<String>, pub addr_count: u32, pub reached: bool, pub send_errors: u32 }
```

---

### Task 1: Scaffold, devbox scripts, wide-event logger

**Files:** Create `Cargo.toml`, `src/main.rs`, `src/lib.rs`, `src/wide.rs`,
`.gitignore`, `AGENTS.md` (skeleton), `CLAUDE.md` (symlink). Modify `devbox.json`.

**Produces:** `wide::Event::new(action: &str) -> Event`, `.set(key, impl Serialize) -> &mut Self`,
`.level(Level)`, `.emit()` (stdout) and `.to_line() -> String`. Every line has
`ts` (RFC 3339 UTC), `level`, `service`, `action`.

- [ ] devbox scripts: `check` = `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`;
  also `test`, `fmt`, `dev` (`cargo run -- run --config deploy/config.dev.toml`), `trace`.
- [ ] Test `wide::tests::line_has_canonical_fields`: `to_line()` parses as JSON with
  `service == "isp-snitch"`, `action == "x"`, `level == "info"`, and a set field `post_count == 3`.
- [ ] Implement, run `devbox run check`, commit. Create private GitHub repo `mago0/isp-snitch` and push.

### Task 2: Config

**Files:** `src/config.rs`, `deploy/config.example.toml`, `deploy/config.dev.toml`.

**Produces:** `Config { interface: Option<String>, listen: SocketAddr, db_path: PathBuf, probe: ProbeConfig, retention: RetentionConfig, targets: Vec<TargetConfig> }`,
`ProbeConfig { interval_ms, timeout_ms, max_ttl: u8, probe_spacing_ms }`, `RetentionConfig { minute_days, hour_days }`,
`TargetConfig { name: String, addr: Ipv4Addr }`, `Config::load(path: &Path) -> anyhow::Result<Config>`, `Config::from_toml(s: &str) -> anyhow::Result<Config>`.

- [ ] Tests: defaults fill in from a file with only `db_path`; an IPv6 target fails
  with a message naming IPv6; duplicate target addrs fail; `interval_ms = 0` fails;
  `max_ttl` outside 1..=64 fails; `timeout_ms < interval_ms` is allowed.
- [ ] Implement with serde defaults. Parse `addr` as a string first, so an IPv6 address gets the clear message.
- [ ] `devbox run check`, commit.

### Task 3: ICMP packet and error-queue parsing (pure)

**Files:** `src/icmp/mod.rs`, `src/icmp/packet.rs`.

**Produces:** `encode_echo(seq: u16) -> [u8; 16]` (type 8, code 0, id 0, checksum 0, seq big-endian,
8-byte payload `b"ispsnch\0"`). The kernel fills in id and checksum.
`parse_echo_reply(buf: &[u8], from: Ipv4Addr) -> Option<TransportEvent>` (type 0, seq at bytes 6..8).
`parse_errqueue(ext_err: &[u8], payload: &[u8]) -> Option<TransportEvent>`, where `ext_err` is the
`IP_RECVERR` cmsg data: `sock_extended_err` (16 bytes: errno u32, origin u8, type u8, code u8, pad u8, info u32, data u32),
then the offender `sockaddr_in`. Origin must be 2 (ICMP). ICMP type 11 -> `TimeExceeded`, type 3 -> `Unreachable`.
The seq comes from `payload[6..8]`, the original echo header.

- [ ] Tests: round trip of seq 0, 1 and 65535; a short buffer gives `None`; type 8 in a reply
  buffer gives `None`; an errqueue blob with type 11 and offender 10.0.0.1 gives `TimeExceeded{seq, 10.0.0.1}`;
  origin != 2 gives `None`.
- [ ] Implement, `devbox run check`, commit.

### Task 4: PingSocket transport + live loopback test

**Files:** `src/icmp/socket.rs`, `tests/live_lo.rs`.

**Produces:** `PingSocket::open(target: Ipv4Addr, interface: Option<&str>) -> io::Result<PingSocket>` implementing `Transport`.

- [ ] `open`: `socket(AF_INET, SOCK_DGRAM|SOCK_NONBLOCK|SOCK_CLOEXEC, IPPROTO_ICMP)`, then
  `SO_BINDTODEVICE` when interface is `Some`, then `IP_RECVERR = 1`. `send`: `IP_TTL`, then `sendto`.
  `recv`: `poll(POLLIN|POLLERR)` with the timeout. Read the error queue first (`recvmsg` with
  `MSG_ERRQUEUE`), then the normal queue. Take `Instant::now()` right after the read returns.
  `EAGAIN` gives `Ok(None)`.
- [ ] `tests/live_lo.rs::loopback_echo_reached_at_ttl_1`: open on `lo` to 127.0.0.1, send seq 7
  at TTL 1, recv within 1 s, expect `EchoReply{seq: 7, from: 127.0.0.1}`. If `open` fails with
  `EACCES`/`EPERM` (ping sockets not allowed), print "skipped" and return.
- [ ] `devbox run check`, commit.

### Task 5: TracerCore and runner

**Files:** `src/tracer/mod.rs`, `src/tracer/runner.rs`.

**Produces:**
- `TracerCore::new(target_id: i64, max_ttl: u8, timeout: Duration) -> Self`
- `fn horizon(&self) -> u8`
- `fn begin_round(&mut self, now: Instant, started_unix: i64) -> Vec<(u16, u8)>` gives (seq, ttl) for TTL 1..=horizon. Seq wraps around.
- `fn on_send_error(&mut self, seq: u16)`
- `fn on_event(&mut self, ev: TransportEvent, at: Instant)` ignores unknown or already answered seqs.
- `fn poll_completed(&mut self, now: Instant) -> Vec<CompletedRound>` times out old probes, then pops
  completed rounds in start order. It applies the truncation and horizon rules from the spec.
- `runner::run(transport_factory: impl FnMut() -> io::Result<Box<dyn Transport>>, core: TracerCore, interval: Duration, spacing: Duration, stop: Arc<AtomicBool>, sink: impl FnMut(CompletedRound), max_rounds: Option<usize>)`.
  Send loop: after each send, drain events until the spacing deadline. Between rounds, recv until the
  next round or the earliest timeout. If the factory fails, every round becomes a single TTL-1
  probe with `send_error`, and the factory is retried every 5 s.

Tests use a `FakeNet` transport (a scripted path: hop i answers `TimeExceeded` from 10.0.0.i,
the target answers at TTL `d`, and loss can be set per hop):
- [ ] `first_round_probes_to_max_ttl`: horizon starts at 30.
- [ ] `horizon_shrinks_to_target_distance`: d = 5, so after round 1 completes, the next round has 5 probes.
- [ ] `path_lengthens_heals`: d moves 5 -> 7, so one round with no reply, then horizon 30, then 7.
- [ ] `truncates_probes_past_target`: a round with replies at TTL 5..30 yields probes 1..=5 only.
- [ ] `no_reply_round_truncates_to_known_distance`: d_known = 5, a total-loss round yields probes 1..=5, all lost.
- [ ] `late_and_duplicate_replies_ignored`: a reply after the timeout and a second reply for one seq do not change counts.
- [ ] `seq_wraps`: start near 65535, run 3 rounds, all matched.
- [ ] `send_error_counts_as_lost`: `on_send_error` gives `send_error: true, rtt: None`.
- [ ] `runner_factory_failure_records_ttl1_send_errors` (Review Focus 1): the factory always fails, so with
  `max_rounds = 2` the sink gets 2 rounds, each with one TTL-1 probe where `send_error` is true.
- [ ] Implement, `devbox run check`, commit.

### Task 6: `trace` CLI (live proof)

**Files:** `src/main.rs`.

- [ ] `isp-snitch trace [--interface IF] [--rounds N=10] ADDR` runs the runner with `PingSocket`
  and prints an mtr-style table: ttl, host, loss%, sent, avg, best, worst.
- [ ] Verify: `devbox run -- cargo run -q -- trace --interface enp5s0 --rounds 5 1.1.1.1` shows
  hop 1 = 192.168.1.1, upstream Spectrum hops, and 1.1.1.1 reached. That proves error-queue parsing on a real path.
- [ ] Commit.

### Task 7: MinuteAggregator

**Files:** `src/aggregate.rs`.

**Produces:** `MinuteAggregator::new() -> Self`, `add(&mut self, r: &CompletedRound)` (buckets by
`started_unix / 60 * 60`, keyed `(target_id, ts, ttl)`), `drain_before(&mut self, ts: i64) -> Vec<HopMinute>`,
`drain_all(&mut self) -> Vec<HopMinute>`. `addr` is the responder seen most often; `addr_count` is the number of distinct responders.

- [ ] Tests: two rounds in one minute sum `sent`/`recv` and min/max; rounds in different minutes give
  separate rows; `drain_before` keeps the current minute; the most common responder wins.
- [ ] Implement, `devbox run check`, commit.

### Task 8: Store

**Files:** `src/store.rs`.

**Produces:** `Store::open(path: &Path) -> anyhow::Result<Store>` (creates parent dirs, WAL, migrations),
`Store::open_in_memory()`, `ensure_target(&self, name: &str, addr: Ipv4Addr) -> Result<i64>`,
`targets(&self) -> Result<Vec<TargetRow { id, name, addr }>>`,
`insert_minutes(&self, rows: &[HopMinute]) -> Result<()>` (additive upsert: sums counts, min/max merge, keeps the existing addr),
`rollup_hours(&self, now: i64) -> Result<usize>` (recomputes every completed hour from the latest hour
row minus 1 h, or from the oldest minute row, `INSERT OR REPLACE`),
`prune(&self, now: i64, minute_days: u32, hour_days: u32) -> Result<(usize, usize)>`,
`hop_buckets(&self, table: Table, target_id: i64, from: i64, to: i64, step: i64) -> Result<Vec<HopBucket>>`,
where `Table::{Minute, Hour}` and `HopBucket { ts, ttl, sent, recv, rtt_sum_us, rtt_min_us, rtt_max_us, addr, reached }`.
The bucket `ts` = `ts / step * step`, and `addr` is the addr of the row with the most `recv`.
The store holds a single `Mutex<Connection>`.

- [ ] Tests: `open_creates_parent_dir` (Review Focus 5); `migrate_is_idempotent` (open twice);
  `ensure_target_updates_name`; `insert_twice_adds_counts`; `rollup_sums_minutes_into_hour`
  (60 minute rows -> 1 hour row with summed sent); `prune_deletes_old_rows`; `hop_buckets_groups_by_step`.
- [ ] Implement, `devbox run check`, commit.

### Task 9: Analysis

**Files:** `src/analysis.rs`.

**Produces:**
- `pick_step(from: i64, to: i64, max_points: i64) -> i64`
- `pick_table(from: i64, now: i64, step: i64, minute_days: u32) -> Table`
- `HopStat { ttl, addr, sent, recv, loss_pct: f64, eff_loss_pct: f64, rate_limited: bool, avg_ms: Option<f64>, min_ms, max_ms }`
- `analyze(hops: &[HopBucket]) -> Vec<HopStat>`, one bucket's rows sorted by ttl and cut at the lowest
  `reached` ttl. `eff_loss_pct[h] = min(loss_pct[h..])`.
- `target_point(stats: &[HopStat]) -> Option<(f64 loss, Option<f64> avg, Option<f64> max)>`: the reached hop's
  loss and latency. 100% loss when nothing was reached but probes were sent. `None` when nothing was sent (a gap).

- [ ] Tests: `pick_step(0, 86400, 720) == 300`, `pick_step(0, 6*3600, 720) == 60`,
  `pick_step(0, 90*86400, 720) == 21600`; a table older than the retention cutoff gives `Hour`;
  hop 3 raw 30% with later hops 0% gives eff 0 and `rate_limited`; loss from hop 4 onward gives eff = raw;
  rows past the reached hop are dropped; nothing reached -> 100%; empty -> `None`.
- [ ] Implement, `devbox run check`, commit.

### Task 10: Web API

**Files:** `src/web/mod.rs`, `tests/api.rs`, `docs/api.md`.

**Produces:** `web::router(state: AppState) -> axum::Router` where `AppState { store: Arc<Store>, targets: Vec<TargetRow>, minute_days: u32 }`.
Routes per the spec. `from`/`to` are unix seconds, defaulting to now-24h and now. JSON shapes are in `docs/api.md`:
- `/api/targets` -> `{from,to,step,targets:[{id,name,addr,loss_pct,avg_ms,max_ms,lossy_buckets}]}`
- `/series` -> `{step,ts:[],loss_pct:[],avg_ms:[],max_ms:[]}` (`null` for gaps)
- `/hops` -> `{step,ts:[],hops:[{ttl,addr,loss_pct:[],eff_loss_pct:[],rate_limited:[],avg_ms:[]}]}`
- `/slice` -> `{from,to,hops:[HopStat + addr_changed]}`

DB calls run in `spawn_blocking`. Errors return `{error: "..."}`.

- [ ] Integration tests (seed a temp store with `insert_minutes`, use `tower::ServiceExt::oneshot`):
  `targets_summary_reports_loss`, `series_has_nulls_for_gaps`, `hops_flags_rate_limited`,
  `empty_db_returns_empty_arrays` (Review Focus 2), `bad_params_are_400` (`from>=to`, `from=abc`,
  range of 401 days) and `unknown_target_is_404` (Review Focus 3), `healthz_ok`.
- [ ] Implement, `devbox run check`, commit.

### Task 11: Service wiring

**Files:** `src/service.rs`, `src/main.rs`, `src/web/mod.rs`.

- [ ] `service::run(cfg: Config) -> anyhow::Result<()>`: open the store, run `ensure_target` for each
  configured target, spawn one runner thread per target (factory = `PingSocket::open`), and use a tokio
  mpsc channel to an aggregator task. The aggregator flushes `drain_before(current_minute)` 10 s after
  each minute and logs `action=probe_flush` with `rows`, `targets` and `lost_probes`. A maintenance task
  runs at startup and then hourly: `rollup_hours` and `prune`, logging `action=maintenance`. Serve the
  router on `listen`. On SIGINT or SIGTERM: set stop, `drain_all`, write, exit 0.
- [ ] HTTP middleware emits one `action=http_request` event (`method`, `path`, `status`,
  `duration_ms`, `outcome`, `client.user_agent`). It skips `/healthz`, and the level follows the status.
- [ ] `check-config` prints the parsed config and exits 0, or prints the error and exits 1.
- [ ] Verify: `devbox run dev` for 3 minutes, then `sqlite3 data/isp-snitch.db 'select count(*) from hop_minute'` > 0. Commit.

### Task 12: UI

**Files:** `src/web/assets.rs`, `assets/index.html`, `assets/target.html`, `assets/app.css`,
`assets/common.js`, `assets/overview.js`, `assets/target.js`, `assets/vendor/uplot/*`.

- [ ] Routes: `/` -> index, `/target/{id}` -> target page, `/assets/{*path}` -> embedded files with the right content type, 404 otherwise.
- [ ] Overview: a range picker (6h/24h/7d/30d/90d, kept in the URL hash). For each target: a stats line,
  a uPlot loss bar chart and a latency line, with synced cursors. Drag to zoom (updates from/to).
  Clicking the target name opens its page.
- [ ] Target page: a target loss bar (uPlot), then a canvas heatmap (rows = hops labeled
  `ttl addr`, columns = buckets, color by eff loss: 0 green, <3 amber, <10 orange, else red,
  hatched grey when `rate_limited`, blank for gaps). A latency tab. Clicking a column fetches
  `/slice` for that bucket and renders the table. Light and dark via `prefers-color-scheme`.
  Show "no data" when the arrays are empty.
- [ ] Test `tests/api.rs::ui_assets_served`: `/`, `/target/1` and `/assets/vendor/uplot/uPlot.iife.min.js` return 200 with the right content types.
- [ ] Verify in a browser against the live dev DB (screenshot). Commit.

### Task 13: Packaging

**Files:** `deploy/systemd/isp-snitch.service`, `scripts/install.sh`, `flake.nix`, `flake.lock`.

- [ ] Unit: `DynamicUser=yes`, `StateDirectory=isp-snitch`, `ExecStart=/usr/local/bin/isp-snitch run --config /etc/isp-snitch/config.toml`,
  `Restart=always`, `CapabilityBoundingSet=` (empty), `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
  `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK`. `AF_NETLINK` is there because interface lookups may need it.
- [ ] `install.sh`: `cargo build --release` (as the user, via devbox), then with sudo: install the binary, the
  config (only if none exists, with `db_path = /var/lib/isp-snitch/isp-snitch.db`) and the unit, then
  `daemon-reload` and `enable --now`.
- [ ] Flake: `packages.default` (`rustPlatform.buildRustPackage`, `cargoLock.lockFile`, `doCheck = false`,
  because the live test needs a network namespace) and `packages.docker` (`dockerTools.buildLayeredImage`, entrypoint
  `isp-snitch run --config /config/config.toml`).
- [ ] Verify: `nix --extra-experimental-features 'nix-command flakes' build .#default` and `.#docker` succeed.
  Run `systemd-analyze verify` on the unit. Commit.

### Task 14: Docs, agent rules, CI

**Files:** `AGENTS.md`, `README.md`, `docs/architecture.md`, `docs/operations.md`,
`docs/adr/0001-custom-ping-socket-tracer.md`, `docs/adr/0002-sqlite-minute-and-hour-tables.md`,
`docs/adr/0003-embedded-ui-no-build-step.md`, `.github/workflows/ci.yml`.

- [ ] `AGENTS.md`: purpose, the one validation command, the repo map, invariants (observe-only, no gaps-as-loss,
  effective loss, IPv4 only, `SO_BINDTODEVICE`), how to prove a change (`trace`, `dev`, API curl),
  logging rules, and where docs live. Agent-neutral wording.
- [ ] `operations.md`: install on BigOne, logs (`journalctl -u isp-snitch`), DB location, the backup
  command (`sqlite3 .backup`), the Docker host sysctl requirement, LAN exposure (`listen` + ufw).
- [ ] CI: `jetify-com/devbox-install-action`, then `devbox run check` on push and PR.
- [ ] Commit and push.

### Task 15: Live verification and final review

- [ ] Run the service against `enp5s0` for 10+ minutes, check the API output and the UI screenshot.
- [ ] Fresh reviewer over the whole repo against the spec, then fix the findings.
- [ ] Push. Write a decision summary for Matt.
