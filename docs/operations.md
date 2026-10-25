# Operations

## Install on BigOne (systemd)

```
cd ~/Projects/personal/isp-snitch
./scripts/install.sh
```

The script builds a release binary through devbox, then uses sudo to:

- install `/usr/local/bin/isp-snitch`;
- install `/etc/systemd/system/isp-snitch.service`;
- write `/etc/isp-snitch/config.toml` from `deploy/config.example.toml` (first
  install only; an existing config is kept);
- enable and restart the service.

Run it again to update. Set `INTERFACE=eth0` for a first install on another
interface.

The unit uses `DynamicUser=yes` with no capabilities. Data are in
`/var/lib/isp-snitch/isp-snitch.db` (systemd maps it to
`/var/lib/private/isp-snitch/`).

## Check that it works

```
systemctl status isp-snitch
journalctl -u isp-snitch -f              # JSON wide events
journalctl -u isp-snitch | grep probe_flush | tail -3   # one per minute
curl -s localhost:9180/api/targets | jq .
```

UI: `http://localhost:9180/`.

## Requirements

- Linux 5.7 or later (unprivileged `SO_BINDTODEVICE`).
- `net.ipv4.ping_group_range` covers the service group. systemd sets
  `0 2147483647` by default. Check with `sysctl net.ipv4.ping_group_range`.
  If it is `1 0`, ping sockets are off:
  `sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"` and persist it in
  `/etc/sysctl.d/`.

## Access from the LAN

The UI listens on `127.0.0.1:9180`. For LAN access, set
`listen = "0.0.0.0:9180"` in `/etc/isp-snitch/config.toml`, restart, and
open the port in the firewall (BigOne runs ufw):

```
sudo ufw allow from 192.168.1.0/24 to any port 9180 proto tcp
```

There is no login. Do not expose the port beyond the LAN.

## Backup

The database is one SQLite file in WAL mode. Take a consistent copy while
the service runs:

```
sudo sqlite3 /var/lib/private/isp-snitch/isp-snitch.db ".backup '/tmp/isp-snitch.db'"
```

## Docker (NAS or Proxmox)

```
nix --extra-experimental-features 'nix-command flakes' build .#docker
docker load -i result
docker run -d --name isp-snitch --restart unless-stopped --network host \
  -v /srv/isp-snitch/config.toml:/config/config.toml:ro \
  -v /srv/isp-snitch/data:/data \
  isp-snitch:0.1.0
```

In the mounted config, set `db_path = "/data/isp-snitch.db"`, and set
`interface` to the host NIC, or remove it to follow the routing table.

Trap: with `--network host` the container uses the **host's**
`net.ipv4.ping_group_range`, and `--sysctl` cannot change it. Many NAS
distributions keep the kernel default `1 0`, which blocks ping sockets. Set
it on the host first. The symptom is `transport_open_failed` events with
"Permission denied" (`EACCES`).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `transport_open_failed`, "No such device" | `interface` does not exist | fix `interface`; the service retries every 5 s |
| `transport_open_failed`, "Permission denied" | ping sockets blocked by sysctl | see Requirements |
| 100% loss at TTL 1 with `send_errors` | link down or unplugged | expected: the outage is recorded as loss |
| Loss at one hop, hatched in the UI | that router rate-limits ICMP | not real loss; look at effective loss |
| Empty charts for a period | the service was not running | gaps are `null`, not loss |
