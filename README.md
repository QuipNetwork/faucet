# Quip Faucet

Standalone dev faucet for [Quip Network](https://gitlab.com/quip.network)
substrate chains. Listens on HTTP and submits
`Sudo.sudo(FaucetOps.mint)` extrinsics from a single funded account
(typically `//Alice` on a dev chain) to whichever destination is
requested. Per-destination rate-limited.

The bot is a deployable copy of `faucet_bot.py` from the
[`quip-protocol`](https://gitlab.com/quip.network/quip-protocol) project,
adapted to mint through the root-only `FaucetOps` pallet by wrapping each
request in `Sudo.sudo(...)`. The configured faucet key therefore needs to be
the chain's sudo/dev key.

Used by [`nodes.quip.network`](https://gitlab.com/quip.network/nodes.quip.network)
as the `faucet` profile in its docker-compose stack.

> **Never run against a production chain.** The startup check refuses to
> bind unless the connected chain reports a known dev name. See the
> `--allow-any-chain` flag below for the (deliberate) override.

## API

| Method & path | Body | Success |
|---|---|---|
| `POST /request` | `{"dest": "<ss58 or 0x-hex>", "amount": <plancks>}` | `200 {"extrinsic_hash", "block_hash", "amount", "dest"}` — faucet mints and broadcasts |
| `POST /sign` | `{"dest": "<ss58 or 0x-hex>", "amount": <plancks>}` | `200 {"signed_extrinsic", "extrinsic_hash", "nonce", "from", "amount", "dest", "mode"}` — receiver broadcasts |
| `GET /health`   | —    | `200 {"status": "ok"}` |

`/sign` returns a `Balances.transfer_keep_alive` signed by a faucet **pool**
account (not the funder), for the receiver to submit via `author_submitExtrinsic`.
It is signed with an immortal era and is **single-use**: submit it promptly —
it is rejected as stale if that pool account is reused first; just call `/sign`
again for a fresh one. Hybrid-chain responses are ~8 KB (ML-DSA-44 signature).

### Status codes

| Code | Meaning |
|---|---|
| `400` | Invalid JSON / `dest` / `amount`. |
| `403` | Destination already funded — free balance exceeds `--max-funded-balance-plancks` (default 0, i.e. any funds). Body includes `free_balance_plancks`. |
| `429` | Rate limited (`retry_after_seconds`). Confirmed-empty accounts use the short `--lenient-rate-limit-seconds`; if the balance query can't run, the strict `--rate-limit-seconds` applies. |
| `503` | `/sign` pool temporarily exhausted (`retry_after_seconds`), or balance check unavailable when `--balance-query-fail-closed`. |
| `502` | Transfer/sign failed; see logs. |

### Balance gate & rate limiting

Every request first checks the destination's on-chain free balance. Funded
accounts are denied (`403`); empty accounts are only lightly throttled
(`--lenient-rate-limit-seconds`, just long enough to bridge inclusion latency).
Set the lenient window `>=` chain block time. On a balance-query failure the
faucet falls back to the strict window and proceeds (`--balance-query-fail-open`,
the default) or denies (`--balance-query-fail-closed`).

### Multiple nodes (failover)

Repeat `--node-url` to add ordered fallbacks. The faucet connects to the first
reachable, verified dev node and, on a connection error, fails over to the next
(sticky — it stays on the healthy one). Timeouts are *not* failed over (a timed-
out tx may already be in a pool; the balance gate backstops the retry). All
nodes are assumed to be replicas of the same chain.

## Run locally

```bash
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt

python faucet_bot.py \
    --node-url ws://localhost:9944 \
    --faucet-key //Alice \
    --listen 127.0.0.1 \
    --port 8087
```

`python faucet_bot.py --help` lists every flag (rate limit, log level,
allow-any-chain override).

## Run in Docker

```bash
docker run --rm -p 8087:8087 \
    registry.gitlab.com/quip.network/faucet:latest \
    --node-url=ws://host.docker.internal:9944 \
    --faucet-key=//Alice \
    --listen=0.0.0.0 \
    --port=8087
```

For the full stack (validator + faucet behind Caddy), see
[`nodes.quip.network`](https://gitlab.com/quip.network/nodes.quip.network)
and run `docker compose --profile validator-cpu --profile faucet up -d`.

## `/sign` pool

`/sign` is backed by a pool of pre-funded accounts so handed-out transactions
never collide with the funder's nonce. Pool accounts are **derived
deterministically** from the funder secret (`blake2b(label ‖ secret ‖ index)`),
so they are the same across restarts — no key file, no stranded funds. At
startup the faucet re-derives them, reconciles balances on chain, and mints only
the shortfall. A background loop refills accounts below `--pool-low-watermark`.

Each `/sign` rotates to the next eligible pool account (round-robin, past its
`--pool-cooldown-seconds` reuse window) and re-fetches its on-chain nonce fresh,
so there is never a nonce gap. When the buffer wraps before a receiver submits
(observed as nonce reuse), the faucet **doubles the pool** during the next idle
window, up to `--pool-max-size`. Tune the pool with `--pool-size`,
`--pool-fund-amount`, `--pool-cooldown-seconds`, and `--pool-replenish-interval`.

## Signing modes

Auto-detected from chain metadata at startup:

- **sr25519** — vanilla `MultiSignature` chains. URI → keypair via
  substrate-interface.
- **hybrid** — `HybridTxSignature` chains (sr25519 + ML-DSA-44, FIPS 204).
  Funder URI must be one of the precomputed dev seeds (`//Alice`, `//Bob`,
  `//Alice//stash`); extrinsic is built byte-by-byte because
  substrate-interface doesn't know the hybrid envelope.

## Tests

```bash
uv venv && source .venv/bin/activate
uv pip install -r requirements-dev.txt
pytest          # unit tests (mock the chain; no node required)
ruff check . && ty check faucet_bot.py
```

## License

AGPL-3.0-or-later. See `LICENSE`.
