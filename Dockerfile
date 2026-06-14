# SPDX-License-Identifier: AGPL-3.0-or-later
# Runtime image for the Rust faucet. The release binary is compiled outside the
# image — by CI's `build-binary` job (substrate toolchain + job-token auth for
# the private quip-protocol-rs dep) — and dropped in as `./quip-faucet`. This
# image only packages it, so it carries no toolchain, git, or credentials.
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

RUN apt-get update && apt-get install -y --no-install-recommends \
    tini ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user matching the PUID/PGID 1000 convention used by
# nodes.quip.network's docker-compose stack.
RUN groupadd --system --gid 1000 quip \
    && useradd --system --uid 1000 --gid 1000 --home /home/quip \
       --create-home --shell /usr/sbin/nologin quip

COPY quip-faucet /usr/local/bin/quip-faucet

USER quip
EXPOSE 8087

ENTRYPOINT ["tini", "--", "/usr/local/bin/quip-faucet"]
