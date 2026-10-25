#!/usr/bin/env bash
# One-time setup: lets the repo owner (and agents running as them) redeploy
# isp-snitch with `devbox run deploy`, without a password.
# Run from the repo as your normal user; it calls sudo to install:
#   /usr/local/sbin/isp-snitch-deploy   root-owned, fixed paths, no arguments
#   /etc/sudoers.d/isp-snitch           NOPASSWD for that one command
# Undo: sudo rm /etc/sudoers.d/isp-snitch /usr/local/sbin/isp-snitch-deploy
set -euo pipefail

cd "$(dirname "$0")/.."
OWNER="$(id -un)"
REPO="$(pwd -P)"

if [[ "$OWNER" == root ]]; then
    echo "run as your normal user, not root" >&2
    exit 1
fi

tmpd=$(mktemp -d)
trap 'rm -rf "$tmpd"' EXIT
sed -e "s|@OWNER@|$OWNER|g" -e "s|@REPO@|$REPO|g" deploy/sudo/isp-snitch-deploy > "$tmpd/deploy"
sed -e "s|@OWNER@|$OWNER|g" deploy/sudo/isp-snitch.sudoers > "$tmpd/sudoers"
bash -n "$tmpd/deploy"

sudo visudo -cf "$tmpd/sudoers"
sudo install -o root -g root -m 755 "$tmpd/deploy" /usr/local/sbin/isp-snitch-deploy
sudo install -o root -g root -m 440 "$tmpd/sudoers" /etc/sudoers.d/isp-snitch
sudo visudo -c > /dev/null

echo "installed. test: sudo -n /usr/local/sbin/isp-snitch-deploy"
