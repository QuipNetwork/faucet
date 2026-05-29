#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Smoke test for the mnemonic → master-seed derivation path that
# faucet-entrypoint.sh uses in hybrid mode. Generates a throwaway mnemonic,
# runs `key inspect` through the faucet image (which bundles
# quip-network-node), and asserts the parse matches `key generate`'s
# secretSeed and conforms to 0x + 64 hex.
#
# Validates the JSON-parse logic the hybrid master-seed path depends on. The
# full hybrid round-trip (mnemonic → derived hybrid SS58) can only be verified
# at Akash deploy time via the faucet's startup "hybrid funder address: …" log
# line. Ported from the retired standalone faucet Akash-deploy repo (2026-05-29);
# that repo's sr25519 docker-compose harness was not carried over.
#
# Run:
#   ./smoke-key-derivation.sh
#
# Override the image under test:
#   IMAGE=registry.gitlab.com/quip.network/faucet:faucet-deploy-testnet-akash-1 \
#     ./smoke-key-derivation.sh

set -euo pipefail

log() { printf '[smoke-key-derivation] %s\n' "$*"; }

IMAGE="${IMAGE:-registry.gitlab.com/quip.network/faucet:faucet-deploy-testnet-akash-1}"

log "image: ${IMAGE}"

if ! docker image inspect "${IMAGE}" >/dev/null 2>&1; then
    log "ERROR: image ${IMAGE} not present locally; build first (see README)"
    exit 1
fi

log "generating throwaway mnemonic via 'key generate'"
GEN_JSON="$(
    docker run --rm --entrypoint /usr/local/bin/quip-network-node "${IMAGE}" \
        key generate --output-type json
)"

MNEMONIC="$(printf '%s' "${GEN_JSON}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["secretPhrase"])')"
EXPECTED_SEED="$(printf '%s' "${GEN_JSON}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["secretSeed"])')"
EXPECTED_SR_SS58="$(printf '%s' "${GEN_JSON}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["ss58Address"])')"

log "mnemonic generated (${#MNEMONIC} chars); sr25519 ss58: ${EXPECTED_SR_SS58}"

log "deriving master seed via 'key inspect' (mirrors faucet-entrypoint.sh)"
DERIVED_SEED="$(
    docker run --rm --entrypoint /usr/local/bin/quip-network-node "${IMAGE}" \
        key inspect "${MNEMONIC}" --output-type json \
      | python3 -c 'import sys,json; print(json.load(sys.stdin)["secretSeed"])'
)"

if [ "${DERIVED_SEED}" != "${EXPECTED_SEED}" ]; then
    log "FAIL: 'key inspect' seed disagrees with 'key generate' seed"
    log "  generate.secretSeed = ${EXPECTED_SEED}"
    log "  inspect.secretSeed  = ${DERIVED_SEED}"
    exit 1
fi

if ! [[ "${DERIVED_SEED}" =~ ^0x[0-9a-fA-F]{64}$ ]]; then
    log "FAIL: derived seed not in 0x + 64 hex shape (got length ${#DERIVED_SEED})"
    exit 1
fi

log "PASS: 'key inspect' parse round-trips and produces 0x + 64 hex"
log ""
log "NOTE: this does NOT verify the resulting hybrid SS58 — that requires"
log "      running _HybridSigner derivation in Python. In production the"
log "      faucet logs 'hybrid funder address: ...' at startup; that's the"
log "      canonical verification step before serving /request."
