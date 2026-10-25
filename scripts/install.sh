#!/usr/bin/env bash
# Build isp-snitch and install it as a systemd service on this host.
# Run from the repo root as your normal user; it calls sudo for the install.
#
#   ./scripts/install.sh            build + install/update + (re)start
#   INTERFACE=eth0 ./scripts/install.sh   first install with another interface
set -euo pipefail

cd "$(dirname "$0")/.."
INTERFACE="${INTERFACE:-enp5s0}"

devbox run build

sudo install -Dm755 target/release/isp-snitch /usr/local/bin/isp-snitch
sudo install -Dm644 deploy/systemd/isp-snitch.service /etc/systemd/system/isp-snitch.service

if [[ ! -f /etc/isp-snitch/config.toml ]]; then
    sed -e "s/^interface = .*/interface = \"$INTERFACE\"/" deploy/config.example.toml \
        | sudo install -Dm644 /dev/stdin /etc/isp-snitch/config.toml
    echo "wrote /etc/isp-snitch/config.toml (interface = $INTERFACE)"
else
    echo "kept existing /etc/isp-snitch/config.toml"
fi

/usr/local/bin/isp-snitch check-config --config /etc/isp-snitch/config.toml > /dev/null
sudo systemctl daemon-reload
sudo systemctl enable isp-snitch.service
sudo systemctl restart isp-snitch.service
sleep 2
systemctl --no-pager --lines=5 status isp-snitch.service
echo "UI: http://$(grep -E '^listen' /etc/isp-snitch/config.toml | cut -d'"' -f2)/"
