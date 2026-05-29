# `faucet.testnet.quip.network` on Akash (v2)

Deployment artifacts for the post-`v1`-teardown faucet, using:

- **Patched faucet image built from `deploy/testnet-akash` branch**: `registry.gitlab.com/quip.network/faucet:faucet-deploy-testnet-akash-1`. Same code as `main` (1cd4ff1) plus a small 3-edit patch to `faucet_bot.py` that adds `--hybrid-master-seed-hex` (and `QUIP_FAUCET_HYBRID_MASTER_SEED_HEX` env) so the funder can be op-1's real hybrid account, not just the `//Alice` / `//Bob` / `//Alice//stash` dev URIs that the upstream `DEV_HYBRID_SEEDS` table hardcodes. Patch source: `faucet_bot.py` diff vs `main` on this branch.
- **Public bootnode RPC** (no internal `quipnode`): `wss://bootnode-1.testnet.quip.network:20049/rpc` — added in [`bootnodes.quip.network`](https://gitlab.com/quip-infra/bootnodes.quip.network) v0.2-preview-2.
- **Tarsnap-backed cert persistence** (caddy image `:caddy-deploy-testnet-akash-3` onwards). `/certs` is still ephemeral, but the entrypoint restores the cert dir from the latest matching Tarsnap archive on every boot and exec's caddy without ever calling the certifier. The 12h renewal loop is the *only* certifier caller; on successful renewal it pushes a fresh archive and prunes to the 10 most recent. Decouples container restarts from certifier rate-limit risk — the structural cause of the 2026-05-28 outage. Same pattern as `pad.quip.network`.

## Cert persistence: recovery story

Container restart (Akash pod kill, provider migration, lease churn): the entrypoint runs `tarsnap --fsck` → `tarsnap --list-archives | grep "^testnet-caddy-faucet-" | sort -r | head -1` → `tarsnap -x -f $LATEST -C /` → cert validity check (`openssl x509 -checkend 86400` + SAN match) → `exec caddy run`. No certifier call. Recovery is bounded by `--fsck` + extract time (single-digit seconds at this archive size). The 12h renewal loop then resumes on its existing schedule.

If the validity check fails (cert expired or hostname mismatch), the entrypoint fatal-exits with a documented message and the operator re-seeds Tarsnap from a laptop using the same procedure as the first cutover. The Caddy container will restart-loop in the meantime — better than serving a stale cert.

Refusing to seed a first archive (operator hasn't run the one-time setup yet) also fatal-exits with a clear error. See the design spec at `quipnetwork/docs/superpowers/specs/2026-05-29-testnet-cert-persistence-design.md` for the full flow and the one-time-setup procedure.

## Files

- [`deploy.yaml`](deploy.yaml) — Akash SDL v2.0 (2 services: faucet + caddy).
- [`Dockerfile.caddy`](Dockerfile.caddy) — builds the caddy + dnsimple-certifier-client image (single image; no fork of the faucet).
- [`Caddyfile.template`](Caddyfile.template) — TLS reverse-proxy config (443 → faucet:8087).
- [`caddy-entrypoint.sh`](caddy-entrypoint.sh) — cert-issue-or-skip, then `exec caddy run` with a 12h background renewal loop.
- [`env.example`](env.example) — secrets reference (mnemonic, certifier token, caddy image tag).

## Build prep

```sh
cd deploy/akash
cp -r ../../../dnsimple-certifier .   # vendored for build context; .gitignored
```

## Build + push the caddy image

```sh
cd deploy/akash
docker build -f Dockerfile.caddy \
    -t registry.gitlab.com/quip.network/faucet:caddy-deploy-testnet-akash-1 .
docker push registry.gitlab.com/quip.network/faucet:caddy-deploy-testnet-akash-1
```

## Build + push the patched faucet image

The deploy/testnet-akash branch contains a 3-edit patch to faucet_bot.py (FaucetConfig field + _init_funder branch + CLI arg). The image is built from the standard repo-root Dockerfile.

```sh
# from repo root, on branch deploy/testnet-akash
docker build \
    -t registry.gitlab.com/quip.network/faucet:faucet-deploy-testnet-akash-1 .
docker push registry.gitlab.com/quip.network/faucet:faucet-deploy-testnet-akash-1
```

Verify both images are anonymously pullable via the GitLab JWT token-exchange flow before submitting to Akash.

## Derive master seed for op-1

The patched faucet expects the 32-byte master seed (not the BIP-39 mnemonic). Derive once locally; the value is static and can live in Proton Pass alongside the mnemonic.

```sh
docker run --rm --entrypoint /usr/local/bin/quip-network-node \
    registry.gitlab.com/quip.network/quip-protocol-rs/quip-network-node:v0.2-preview \
    key inspect "<paste-op-1-mnemonic>" --output-type json \
    | jq -r .secretSeed
```

Paste the `0x...` output as `QUIP_FAUCET_HYBRID_MASTER_SEED_HEX` in `deploy.yaml`.

## Smoke test (local)

```sh
docker run --rm -e MOCK_CERTIFIER=1 \
    -e QUIP_HOSTNAME=localhost \
    -e FAUCET_UPSTREAM=host.docker.internal:8087 \
    -p 8443:443 \
    registry.gitlab.com/quip.network/faucet:caddy-deploy-testnet-akash-1
# in another terminal:
curl -k https://localhost:8443/health
```

A full end-to-end smoke (faucet → bootnode RPC → on-chain mint) requires the actual `OPERATOR_1_MNEMONIC` and outbound access to the public bootnode RPC; not normally run from a dev machine — verify on Akash after deploy.

## Deploy to Akash

1. Fill in the 3 placeholders in `deploy.yaml`:
   - `<operator-1-mnemonic>` (Proton Pass)
   - `<certifier-token-faucet>` (Proton Pass — same hostname as v1, token still valid if not revoked)
   - `<caddy-image-tag>` (the tag you just pushed)
2. Submit via Akash console or `provider-services tx deployment create deploy.yaml`.
3. Wait for bids, accept, lease starts.
4. Note the public IP assigned to the `faucet-ip` endpoint.
5. Update DNS: `faucet.testnet.quip.network` A record → new IP (TTL 60).
6. Wait for cert issuance (≤30s after caddy pod boots), then verify:

```sh
echo | openssl s_client -connect faucet.testnet.quip.network:443 \
    -servername faucet.testnet.quip.network -brief 2>&1 \
    | grep -E '(CONNECTION|Verification)'

curl -sS https://faucet.testnet.quip.network/health
# Expect: {"status": "ok"}

# End-to-end drip (requires a fresh ss58 dest)
curl -sS -X POST -H 'content-type: application/json' \
    --data '{"dest":"<ss58>","amount":1000000000000}' \
    https://faucet.testnet.quip.network/request | jq .
# Expect: 200 {"extrinsic_hash":"0x…","block_hash":"0x…","amount":…,"dest":"…"}

# Verify on-chain inclusion via bootnode RPC
curl -sS -X POST -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"system_account_nextIndex\",\"params\":[\"<ss58>\"]}" \
    https://bootnode-1.testnet.quip.network:20049/rpc | jq .

# Rate-limit check (within 60s of the drip above, same dest)
curl -i -X POST -H 'content-type: application/json' \
    --data '{"dest":"<ss58>","amount":1000000000000}' \
    https://faucet.testnet.quip.network/request
# Expect: 429 with retry_after_seconds
```

## Known gotchas

- **Akash provider routing** varies (memory: `statusdb_on_akash`). The v1 deploy's first bidder didn't route the metallb pool externally; we had to close and rebid. If the first provider's IP isn't reachable on 443 within ~5 min of lease start, close the deployment and rebid.
- **Akash credentials block**: don't add `credentials:` with placeholder strings — the provider 401s and does NOT fall back to anonymous (memory: `akash_credentials_no_anonymous_fallback`). Both images are public; omit the block entirely.
- **No persistent storage in this SDL** — we tried `beta2` first and got zero bids; reverted to ephemeral `/certs` so the deploy is schedulable. Caddy pod restarts re-issue the cert from the certifier (1-cert/hour throttle gates repeated cold-starts within that window).
- **Cert-not-revoked assumption**: if the v1 certifier token for `faucet.testnet.quip.network` was revoked during v1 teardown, this deploy can't issue. Mint a fresh token via certifier admin before submitting.
