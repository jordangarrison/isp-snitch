# ADR 0001: Custom tracer on unprivileged ping sockets

Status: accepted, 2026-10-24

## Context

isp-snitch must run mtr-style traces (one probe per TTL, time to live)
through one named interface. On BigOne that is `enp5s0`, the ISP link. BigOne
also has a Starlink uplink on `wlo1`. Both uplinks use gateway 192.168.1.1,
and `net-failover` moves the default route between them by changing route
metrics. Probes must keep using `enp5s0` during a failover, or the graphs mix
two paths.

Options:

1. **`trippy-core`** (Rust library behind Trippy). Checked in its 0.13 source:
   `Builder::interface()` only looks up a source address
   (`net/platform/unix.rs`, `lookup_interface_addr`). It does not bind to the
   device. With two default routes, a probe with `enp5s0`'s source IP still
   follows the lowest-metric route, which during failover is `wlo1`.
2. **Shell out to `mtr --json -I enp5s0`.** mtr binds correctly, but needs an
   external binary with setuid or `CAP_NET_RAW`, runs as batches instead of
   a continuous stream, and is parsed from text output.
3. **Own tracer on a Linux ping socket**: `socket(AF_INET, SOCK_DGRAM,
   IPPROTO_ICMP)` with `SO_BINDTODEVICE`, `IP_TTL` per probe, and
   `IP_RECVERR`. TTL Exceeded replies arrive on the error queue with the
   router's address.

## Decision

Option 3. It was tested on BigOne as uid 1000 before the decision: traces
through `enp5s0` (Spectrum path) and `wlo1` (Starlink path) gave different
hops, with no privileges.

## Consequences

- No capabilities or setuid. The systemd unit has an empty capability set.
- Needs Linux 5.7+ (unprivileged `SO_BINDTODEVICE`) and a
  `net.ipv4.ping_group_range` that covers the service group.
- Under Docker with `--network host`, the host's sysctl applies. A NAS with
  the kernel default `1 0` blocks it (see `docs/operations.md`).
- Linux only, IPv4 only for now. The `Transport` trait keeps a `SOCK_RAW`
  transport (with `CAP_NET_RAW`) possible without touching the round logic.
- Trap handled in `src/icmp/socket.rs`: a queued ICMP error makes the next
  `sendto` fail once with `EHOSTUNREACH` and drop the packet. The send path
  retries once for those errnos.
