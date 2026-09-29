# `faucet.testnet.quip.network` on Akash (v2)

Deployment artifacts for the post-`v1`-teardown faucet, using:

- **Rust faucet image**: `registry.gitlab.com/quip.network/faucet:latest` — the Rust binary published by the repo's CI. It derives the appointed authority account **natively** from the mnemonic via `HybridPair::from_string`, so there is no patched image and no master-seed derivation step. Pin `:sha-<short>` instead of `:latest` in `deploy.yaml` for a reproducible deploy.
- **Public bootnode RPC** (no internal `quipnode`): `wss://bootnode-1.testnet.quip.network:20049/rpc` is primary, with `bootnode-2` / `bootnode-3` as ordered failover — the Rust faucet connects to the first reachable and fails over on transport errors. Added in [`bootnodes.quip.network`](https://gitlab.com/quip-infra/bootnodes.quip.network) v0.2-preview-2.
- **Tarsnap-backed cert persistence** (caddy image `:caddy-deploy-testnet-akash-3` onwards). `/certs` is still ephemeral, but the entrypoint restores the cert dir from the latest matching Tarsnap archive on every boot and exec's caddy without ever calling the certifier. The 12h renewal loop is the *only* certifier caller; on successful renewal it pushes a fresh archive and prunes to the 10 most recent. Decouples container restarts from certifier rate-limit risk — the structural cause of the 2026-05-28 outage. Same pattern as `pad.quip.network`.

## Cert persistence: recovery story

Container restart (Akash pod kill, provider migration, lease churn): the entrypoint runs `tarsnap --fsck` → `tarsnap --list-archives | grep "^testnet-caddy-faucet-" | sort -r | head -1` → `tarsnap -x -f $LATEST -C /` → cert validity check (`openssl x509 -checkend 86400` + SAN match) → `exec caddy run`. No certifier call. Recovery is bounded by `--fsck` + extract time (single-digit seconds at this archive size). The 12h renewal loop then resumes on its existing schedule.

If the validity check fails (cert expired or hostname mismatch), the entrypoint fatal-exits with a documented message and the operator re-seeds Tarsnap from a laptop using the same procedure as the first cutover. The Caddy container will restart-loop in the meantime — better than serving a stale cert.

Refusing to seed a first archive (operator hasn't run the one-time setup yet) also fatal-exits with a clear error. See the design spec at `quipnetwork/docs/superpowers/specs/2026-05-29-testnet-cert-persistence-design.md` for the full flow and the one-time-setup procedure.

## Files

- [`deploy.yaml`](deploy.yaml) — Akash SDL v2.0 (2 services: faucet + caddy).
- [`Dockerfile.caddy`](Dockerfile.caddy) — builds the caddy + dnsimple-certifier-client image (single image; the faucet image is upstream, unmodified).
- [`Caddyfile.template`](Caddyfile.template) — TLS reverse-proxy config (443 → faucet:8087).
- [`caddy-entrypoint.sh`](caddy-entrypoint.sh) — cert restore-or-fail, then `exec caddy run` with a 12h background renewal loop.
- [`env.example`](env.example) — secrets reference (authority SURI, certifier token).

## Build prep

```sh
cd deploy/akash
cp -r ../../../dnsimple-certifier .   # vendored for build context; .gitignored
```

## Build + push the caddy image

```sh
cd deploy/akash
docker build -f Dockerfile.caddy \
    -t registry.gitlab.com/quip.network/faucet:caddy-deploy-testnet-akash-3 .
docker push registry.gitlab.com/quip.network/faucet:caddy-deploy-testnet-akash-3
```

## Faucet image

No build step — the deploy uses the upstream `registry.gitlab.com/quip.network/faucet:latest` image (the Rust binary, published by the repo's CI on every merge to `main`). Confirm it is anonymously pullable before submitting to Akash. To pin a specific build, replace `:latest` with `:sha-<short>` in `deploy.yaml`.

## Funder key

`QUIP_FAUCET_FAUCET_KEY` is the SURI/mnemonic of the funded H4 account appointed
by Foundation `faucetOps.set_authority(Some(account))`. Startup and refreshes
check Authority, Enabled fuse, remaining budget and spec/transaction versions.
Top-ups submit direct signed mint and require finalized matching FaucetOps.Minted and EmissionController.FaucetMinted receipts.
After governance rotates the key, update the secret and restart; `None` revokes
it. Rotation never restores the fuse/budget and changes derived base/pool accounts.
Confirm identity and `/health` readiness before routing traffic:

```
funder: <ss58>        # must equal FaucetOps.Authority
base wallet: <ss58>
pool ready: N accounts
```

## Smoke test (caddy, local)

```sh
docker run --rm -e MOCK_CERTIFIER=1 \
    -e QUIP_HOSTNAME=localhost \
    -e FAUCET_UPSTREAM=host.docker.internal:8087 \
    -p 8443:443 \
    registry.gitlab.com/quip.network/faucet:caddy-deploy-testnet-akash-3
# in another terminal:
curl -k https://localhost:8443/health
```

A full end-to-end smoke (faucet → bootnode RPC → on-chain mint) requires the appointed authority’s
real key and outbound access to the public bootnode RPC; not normally run from a
dev machine — verify on Akash after deploy via the steps below.

## Deploy to Akash

1. Fill in the placeholders in `deploy.yaml`:
   - `<faucet-authority-suri>`
   - `<certifier-token-faucet>` (Proton Pass — same hostname as v1, token still valid if not revoked)
   - `<testnet-caddy-tarsnap-key-base64>` (Proton Pass: `TARSNAP_KEY_TESTNET_CADDY`, "base64" field)
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
# Expect: {"status":"ok"}

# End-to-end drip (requires a fresh ss58 dest)
curl -sS -X POST -H 'content-type: application/json' \
    --data '{"dest":"<ss58>","amount":1000000000000}' \
    https://faucet.testnet.quip.network/request | jq .
# Expect: 200 {"extrinsic_hash":"0x…","amount":…,"dest":"…"}

# Verify on-chain inclusion via bootnode RPC
curl -sS -X POST -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"system_account_nextIndex\",\"params\":[\"<ss58>\"]}" \
    https://bootnode-1.testnet.quip.network:20049/rpc | jq .

# Re-request the same dest — once the mint is on-chain it has funds, so the
# balance gate denies it (Goal 4).
curl -i -X POST -H 'content-type: application/json' \
    --data '{"dest":"<ss58>","amount":1000000000000}' \
    https://faucet.testnet.quip.network/request
# Expect: 403 {"error":"destination already funded","free_balance_plancks":…}
# (a rapid repeat to a still-empty dest instead returns 429 with retry_after_seconds)
```

## Known gotchas

- **Akash provider routing** varies (memory: `statusdb_on_akash`). The v1 deploy's first bidder didn't route the metallb pool externally; we had to close and rebid. If the first provider's IP isn't reachable on 443 within ~5 min of lease start, close the deployment and rebid.
- **Akash credentials block**: don't add `credentials:` with placeholder strings — the provider 401s and does NOT fall back to anonymous (memory: `akash_credentials_no_anonymous_fallback`). Both images are public; omit the block entirely.
- **No persistent storage in this SDL** — we tried `beta2` first and got zero bids; reverted to ephemeral `/certs` so the deploy is schedulable. Cert persistence is Tarsnap-backed (see above), not Akash volumes.
- **Cert-not-revoked assumption**: if the v1 certifier token for `faucet.testnet.quip.network` was revoked during v1 teardown, this deploy can't issue. Mint a fresh token via certifier admin before submitting.
- **First boot mints a lot**: the faucet directly mints into its base wallet up to its target runway and funds the pool on first boot. Budget and transaction-fee balance must cover this. Watch the startup log for `base wallet low … authority-minting …` and `pool ready`.
