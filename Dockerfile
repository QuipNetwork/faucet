# SPDX-License-Identifier: AGPL-3.0-or-later
# Runtime image for the Rust faucet. The release binary is compiled outside the
# image — by CI's per-arch `build-binary-<arch>` jobs (substrate toolchain +
# job-token auth for the private quip-protocol-rs dep) — and dropped in as
# `./quip-faucet`. This image only packages it, so it carries no toolchain,
# git, or credentials. Arch-neutral: CI builds it once per architecture
# (amd64/arm64) from the matching binary.
#
# Local build:
#   cargo build --release --locked
#   install -m 0755 target/release/quip-faucet quip-faucet
#   docker build -t quip-faucet .
#
# The binary links TLS via rustls/ring (no OpenSSL), so the runtime needs only
# ca-certificates for trust roots.
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="quip-faucet"
LABEL org.opencontainers.image.description="Concurrent dev faucet for Quip substrate chains."
LABEL org.opencontainers.image.source="https://gitlab.com/quip.network/faucet"
LABEL org.opencontainers.image.licenses="AGPL-3.0-or-later"

# Runtime deps track the Debian base; pinning point versions adds churn
# without a security benefit (same policy as the quip-network-node image).
# hadolint ignore=DL3008
RUN apt-get update && apt-get install -y --no-install-recommends \
    tini ca-certificates gosu \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user. The entrypoint remaps it to PUID/PGID (default 1000)
# at start — the runtime convention used by nodes.quip.network's compose
# stack — then drops privileges via gosu.
RUN groupadd --system --gid 1000 quip \
    && useradd --system --uid 1000 --gid 1000 --home /home/quip \
       --create-home --shell /usr/sbin/nologin quip

COPY quip-faucet /usr/local/bin/quip-faucet
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# No USER: the container starts as root only so the entrypoint can remap
# quip to PUID/PGID; it execs the faucet as quip immediately. Starting with
# --user still works — the entrypoint then execs directly, skipping the remap.
EXPOSE 8087

ENTRYPOINT ["tini", "--", "/usr/local/bin/entrypoint.sh"]
