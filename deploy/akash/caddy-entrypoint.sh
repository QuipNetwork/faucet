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
    SCRATCH="$(mktemp -d)"
    if certifier issue --domain "${QUIP_HOSTNAME}" --out "${SCRATCH}/"; then
        # certifier writes <domain>.crt (leaf+chain) + <domain>.key.
        # Don't concat with .issuer.crt — memory: dnsimple-certifier-cert-format.
        cp "${SCRATCH}/${QUIP_HOSTNAME}.crt" "${CERT_FILE}"
        cp "${SCRATCH}/${QUIP_HOSTNAME}.key" "${KEY_FILE}"
        chmod 644 "${CERT_FILE}"
        chmod 600 "${KEY_FILE}"
        log "cert installed at ${CERT_FILE}"
    else
        log "ERROR: certifier issue failed for ${QUIP_HOSTNAME}"
        rm -rf "${SCRATCH}"
        exit 1
    fi
    rm -rf "${SCRATCH}"
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
