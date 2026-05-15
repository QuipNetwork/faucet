# SPDX-License-Identifier: AGPL-3.0-or-later
# Multi-stage build for the Quip faucet bot.
# Stage 1 builds wheels (build-essential available so arm64 source-only
# transitives still install). Stage 2 is a slim runtime with tini as PID 1.

FROM python:3.13-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY requirements.txt ./
RUN pip wheel --no-cache-dir --wheel-dir /wheels -r requirements.txt


FROM python:3.13-slim-bookworm

LABEL org.opencontainers.image.title="quip-faucet"
LABEL org.opencontainers.image.description="Standalone dev faucet for Quip substrate chains."
LABEL org.opencontainers.image.source="https://gitlab.com/quip.network/faucet"
LABEL org.opencontainers.image.licenses="AGPL-3.0-or-later"

RUN apt-get update && apt-get install -y --no-install-recommends \
    tini \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /wheels /wheels
COPY requirements.txt ./
RUN pip install --no-cache-dir --no-index --find-links=/wheels -r requirements.txt \
    && rm -rf /wheels

COPY faucet_bot.py ./

# Non-root runtime user matching the PUID/PGID 1000 convention used by
# nodes.quip.network's docker-compose stack.
RUN groupadd --system --gid 1000 quip \
    && useradd --system --uid 1000 --gid 1000 --home /home/quip \
       --create-home --shell /usr/sbin/nologin quip

USER quip
EXPOSE 8087

ENTRYPOINT ["tini", "--", "python3", "/app/faucet_bot.py"]
