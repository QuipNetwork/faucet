#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
# Remap the quip user to the operator's PUID/PGID (default 1000) and drop
# privileges before exec'ing the faucet — the runtime convention shared with
# the quip-network-node image (nodes.quip.network's compose stack sets
# PUID/PGID on every service). tini (PID 1) runs this script as root.
set -eu

# Started with --user (any non-root uid): remapping tools are unavailable and
# pointless — run the faucet directly as that user.
if [ "$(id -u)" != "0" ]; then
    exec /usr/local/bin/quip-faucet "$@"
fi

PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
[ "$PGID" = "$(id -g quip)" ] || groupmod -o -g "$PGID" quip
[ "$PUID" = "$(id -u quip)" ] || usermod -o -u "$PUID" quip
# The faucet keeps no state on disk; home only holds skeleton dotfiles.
chown -R quip:quip /home/quip

exec gosu quip /usr/local/bin/quip-faucet "$@"
