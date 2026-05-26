#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Caddy + cert-issuing entrypoint for faucet.testnet.quip.network on Akash.
# Issues a cert from dnsimple-certifier on first boot (skipped if already
# present), then exec's caddy in the foreground with a background 12h
# renewal loop. Cert state lives at /certs/<hostname>/ on a persistent
# Akash volume so it survives pod restarts.
#
# Required env:
#   QUIP_HOSTNAME       e.g. faucet.testnet.quip.network
#   FAUCET_UPSTREAM     service:port for the faucet (faucet:8087)
#   CERTIFIER_HOSTNAME  e.g. certifier.quip.network:9443
#   CERTIFIER_TOKEN     bearer token for $QUIP_HOSTNAME at the certifier
#
# Optional env:
#   MOCK_CERTIFIER  if set, write a self-signed cert instead of calling the
#                   real certifier. Used by smoke harnesses.

set -eu

log() { printf '[caddy-entrypoint] %s\n' "$*"; }

: "${QUIP_HOSTNAME:?QUIP_HOSTNAME required}"
: "${FAUCET_UPSTREAM:?FAUCET_UPSTREAM required}"

CERT_DIR="/certs/${QUIP_HOSTNAME}"
CERT_FILE="${CERT_DIR}/fullchain.pem"
KEY_FILE="${CERT_DIR}/privkey.pem"
mkdir -p "${CERT_DIR}"

# ── Cert issuance (idempotent) ──────────────────────────────────────────────
# Skip if cert already on disk — the renewal loop below handles the periodic
# refresh, and the certifier enforces 1 cert/hour per domain server-side
# (memory: dnsimple-certifier-issue-throttle).
if [ -s "${CERT_FILE}" ] && [ -s "${KEY_FILE}" ]; then
    log "cert already present at ${CERT_FILE}"
elif [ -n "${MOCK_CERTIFIER:-}" ]; then
    log "MOCK_CERTIFIER set; writing self-signed cert for ${QUIP_HOSTNAME}"
    openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "${KEY_FILE}" -out "${CERT_FILE}" \
        -days 1 -subj "/CN=${QUIP_HOSTNAME}" 2>/dev/null
    chmod 600 "${KEY_FILE}"
else
    : "${CERTIFIER_HOSTNAME:?CERTIFIER_HOSTNAME required when not MOCK_CERTIFIER}"
    : "${CERTIFIER_TOKEN:?CERTIFIER_TOKEN required when not MOCK_CERTIFIER}"
    log "issuing cert for ${QUIP_HOSTNAME} via ${CERTIFIER_HOSTNAME}"
    # Retry on failure rather than exit. Akash containers can restart and
    # ephemeral /certs is wiped, so a transient certifier 429/403/500 must
    # not leave caddy fatally down. The loop never gives up — caddy exec
    # happens only after the cert is in hand. Mirrors the bootnodes'
    # caddy-rpc-bringup.sh retry behavior.
    attempt=0
    while true; do
        attempt=$((attempt + 1))
        SCRATCH="$(mktemp -d)"
        if certifier issue --domain "${QUIP_HOSTNAME}" --out "${SCRATCH}/"; then
            cp "${SCRATCH}/${QUIP_HOSTNAME}.crt" "${CERT_FILE}"
            cp "${SCRATCH}/${QUIP_HOSTNAME}.key" "${KEY_FILE}"
            chmod 644 "${CERT_FILE}"
            chmod 600 "${KEY_FILE}"
            log "cert installed at ${CERT_FILE} (attempt ${attempt})"
            rm -rf "${SCRATCH}"
            break
        fi
        rm -rf "${SCRATCH}"
        # Backoff: 6 × 5min (covers per-domain 1h cooldown),
        # then 6 × 30min, then 1h indefinitely.
        if [ "${attempt}" -lt 6 ]; then
            sleep_for=300
        elif [ "${attempt}" -lt 12 ]; then
            sleep_for=1800
        else
            sleep_for=3600
        fi
        log "cert issue failed (attempt ${attempt}); retrying in ${sleep_for}s"
        sleep "${sleep_for}"
    done
fi

# ── Renewal loop (12h, real certifier only) ─────────────────────────────────
if [ -z "${MOCK_CERTIFIER:-}" ]; then
    (
        while true; do
            sleep 43200
            log "running scheduled cert renewal"
            SCRATCH="$(mktemp -d)"
            if certifier renew --domain "${QUIP_HOSTNAME}" --cert "${CERT_FILE}" --out "${SCRATCH}/"; then
                cp "${SCRATCH}/${QUIP_HOSTNAME}.crt" "${CERT_FILE}"
                cp "${SCRATCH}/${QUIP_HOSTNAME}.key" "${KEY_FILE}"
                log "cert renewed; reloading caddy"
                caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile \
                    2>&1 | sed 's/^/  caddy: /' \
                    || log "WARN: caddy reload failed"
            else
                log "WARN: cert renewal failed; will retry in 12h"
            fi
            rm -rf "${SCRATCH}"
        done
    ) &
fi

# ── Caddy ───────────────────────────────────────────────────────────────────
export QUIP_HOSTNAME CERT_FILE KEY_FILE FAUCET_UPSTREAM
log "starting caddy: hostname=${QUIP_HOSTNAME} upstream=${FAUCET_UPSTREAM}"
exec caddy run --config /etc/caddy/Caddyfile --adapter caddyfile
