#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Caddy + cert-persistence entrypoint for faucet.testnet.quip.network on Akash.
#
# Boot path is Tarsnap-only: restore the cert dir from the latest archive,
# validate the cert, then exec caddy. The certifier is called ONLY from the
# 12h renewal background loop. This decouples container restarts from
# certifier rate-limit risk — the structural cause of the 2026-05-28 testnet
# outage. Mirrors the pad.quip.network pattern.
#
# Required env:
#   QUIP_HOSTNAME       e.g. faucet.testnet.quip.network
#   FAUCET_UPSTREAM     service:port for the faucet (faucet:8087)
#   TARSNAP_KEY         base64-encoded Tarsnap machine key (shared
#                       'testnet-caddy' key — see deploy/akash/README.md)
#   CERTIFIER_HOSTNAME  e.g. certifier.quip.network:9443 (renewal-only)
#   CERTIFIER_TOKEN     bearer token for $QUIP_HOSTNAME (renewal-only)
#
# Optional env:
#   MOCK_CERTIFIER  if set, write a self-signed cert and skip Tarsnap entirely.
#                   Used by smoke harnesses that don't have a Tarsnap key.
#   ARCHIVE_PREFIX  defaults to "testnet-caddy-faucet-". Override for staging.

set -eu

log() { printf '[caddy-entrypoint] %s\n' "$*"; }

: "${QUIP_HOSTNAME:?QUIP_HOSTNAME required}"
: "${FAUCET_UPSTREAM:?FAUCET_UPSTREAM required}"

CERT_DIR="/certs/${QUIP_HOSTNAME}"
CERT_FILE="${CERT_DIR}/fullchain.pem"
KEY_FILE="${CERT_DIR}/privkey.pem"
ARCHIVE_PREFIX="${ARCHIVE_PREFIX:-testnet-caddy-faucet-}"

mkdir -p "${CERT_DIR}"

# ── MOCK_CERTIFIER short-circuit (smoke tests) ─────────────────────────────
# Smoke harnesses don't have a Tarsnap key — skip the whole flow and write a
# 1-day self-signed cert. Preserves the existing smoke-test contract.
if [ -n "${MOCK_CERTIFIER:-}" ]; then
    log "MOCK_CERTIFIER set; writing self-signed cert for ${QUIP_HOSTNAME} (no Tarsnap)"
    openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "${KEY_FILE}" -out "${CERT_FILE}" \
        -days 1 -subj "/CN=${QUIP_HOSTNAME}" 2>/dev/null
    chmod 644 "${CERT_FILE}"
    chmod 600 "${KEY_FILE}"
    export QUIP_HOSTNAME CERT_FILE KEY_FILE FAUCET_UPSTREAM
    log "starting caddy: hostname=${QUIP_HOSTNAME} upstream=${FAUCET_UPSTREAM}"
    exec caddy run --config /etc/caddy/Caddyfile --adapter caddyfile
fi

: "${TARSNAP_KEY:?TARSNAP_KEY required (or set MOCK_CERTIFIER=1 for smoke)}"
: "${CERTIFIER_HOSTNAME:?CERTIFIER_HOSTNAME required (renewal loop)}"
: "${CERTIFIER_TOKEN:?CERTIFIER_TOKEN required (renewal loop)}"

# ── Block A: Tarsnap init ──────────────────────────────────────────────────
# Decode base64 key from env into the file Tarsnap's config points at.
log "writing Tarsnap key from env"
( umask 077 && printf '%s' "${TARSNAP_KEY}" | base64 -d > /etc/tarsnap/tarsnap.key )

# Rebuild cache from the server. /usr/local/tarsnap-cache is ephemeral on
# Akash, so --fsck runs every boot to pull the metadata needed for
# --list-archives. Retry every 60s on transient failure (network blip);
# never falls back to the certifier — boot is Tarsnap-only by design.
attempt=0
until tarsnap --fsck 2>&1 | sed 's/^/  tarsnap: /'; do
    attempt=$((attempt + 1))
    log "tarsnap --fsck failed (attempt ${attempt}); retrying in 60s"
    sleep 60
done
log "tarsnap cache ready"

# ── Block B: restore latest archive ────────────────────────────────────────
# Archive names sort lexicographically by trailing UTC timestamp, so
# `sort -r | head -1` reliably picks the newest.
LATEST=$(tarsnap --list-archives 2>/dev/null \
    | grep "^${ARCHIVE_PREFIX}" | sort -r | head -1)

if [ -z "${LATEST}" ]; then
    log "FATAL: no Tarsnap archive matching prefix '${ARCHIVE_PREFIX}'"
    log "  Operator must seed a first archive — see deploy/akash/README.md"
    exit 1
fi

log "restoring from archive: ${LATEST}"
tarsnap -x -f "${LATEST}" -C / 2>&1 | sed 's/^/  tarsnap: /'

# ── Block B': cert validity check ──────────────────────────────────────────
# Refuse to serve a missing, expired, or hostname-mismatched cert.
if ! [ -s "${CERT_FILE}" ] || ! [ -s "${KEY_FILE}" ]; then
    log "FATAL: restore did not produce ${CERT_FILE} or ${KEY_FILE}"
    log "  Archive layout mismatch — see deploy/akash/README.md 'archive contents'"
    exit 1
fi

if ! openssl x509 -in "${CERT_FILE}" -checkend 86400 -noout 2>/dev/null; then
    log "FATAL: cert at ${CERT_FILE} expires within 24h or is malformed"
    log "  Operator must seed a fresh archive — see deploy/akash/README.md"
    exit 1
fi

if ! openssl x509 -in "${CERT_FILE}" -noout -text 2>/dev/null \
       | grep -qF "DNS:${QUIP_HOSTNAME}"; then
    log "FATAL: cert SAN does not include ${QUIP_HOSTNAME}"
    log "  Archive belongs to a different hostname; check ARCHIVE_PREFIX"
    exit 1
fi

log "cert valid; SAN matches ${QUIP_HOSTNAME}"

# ── Block C: renewal loop (background) ─────────────────────────────────────
# Certifier is invoked ONLY here. On success: copy into cert dir, caddy
# reload, push a new archive, prune to 10 most recent matching our prefix.
# No periodic safety-net backup — pushes are renewal-driven only.
(
    while true; do
        sleep 43200
        log "running scheduled cert renewal"
        SCRATCH="$(mktemp -d)"
        if certifier renew --domain "${QUIP_HOSTNAME}" \
                --cert "${CERT_FILE}" --out "${SCRATCH}/"; then
            cp "${SCRATCH}/${QUIP_HOSTNAME}.crt" "${CERT_FILE}"
            cp "${SCRATCH}/${QUIP_HOSTNAME}.key" "${KEY_FILE}"
            log "cert renewed; reloading caddy"
            caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile \
                2>&1 | sed 's/^/  caddy: /' \
                || log "WARN: caddy reload failed"

            NEW_ARCHIVE="${ARCHIVE_PREFIX}$(date -u +%Y%m%d-%H%M%S)"
            log "pushing new archive: ${NEW_ARCHIVE}"
            if tarsnap -c -f "${NEW_ARCHIVE}" -C / certs \
                    2>&1 | sed 's/^/  tarsnap: /'; then
                log "archive push complete"
                ALL_ARCHIVES=$(tarsnap --list-archives 2>/dev/null \
                    | grep "^${ARCHIVE_PREFIX}" | sort -r)
                COUNT=$(printf '%s\n' "${ALL_ARCHIVES}" | wc -l)
                if [ "${COUNT}" -gt 10 ]; then
                    printf '%s\n' "${ALL_ARCHIVES}" | tail -n +11 \
                        | while read -r old; do
                            log "pruning ${old}"
                            tarsnap -d -f "${old}" 2>&1 \
                                | sed 's/^/  tarsnap: /' \
                                || log "WARN: prune of ${old} failed"
                          done
                fi
            else
                log "WARN: tarsnap -c failed; no new archive this cycle"
            fi
        else
            log "WARN: cert renewal failed; will retry in 12h"
        fi
        rm -rf "${SCRATCH}"
    done
) &

# ── Caddy (foreground) ─────────────────────────────────────────────────────
export QUIP_HOSTNAME CERT_FILE KEY_FILE FAUCET_UPSTREAM
log "starting caddy: hostname=${QUIP_HOSTNAME} upstream=${FAUCET_UPSTREAM}"
exec caddy run --config /etc/caddy/Caddyfile --adapter caddyfile
