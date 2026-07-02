# Quip Faucet

Standalone dev faucet for [Quip Network](https://gitlab.com/quip.network)
substrate chains. Listens on HTTP and submits
`Sudo.sudo(FaucetOps.mint)` extrinsics from a single funded account
(typically `//Alice` on a dev chain) to whichever destination is
requested. Per-destination rate-limited.

A concurrent Rust binary (`src/`, `quip-faucet`) built on tokio + jsonrpsee
(multiplexed RPC, no global lock) with **per-account nonce lanes**, so `/request`
mints pipeline concurrently instead of serializing one transaction per block. It
reuses the Quip runtime, crypto, and client crates (`quip-protocol-runtime`,
`quip-transaction-crypto`, `quip-tools`) so the extrinsic wire format can never
drift from the chain. Each request mints through the root-only `FaucetOps` pallet
wrapped in `Sudo.sudo(...)`, so the configured faucet key must be the chain's
sudo/dev key.

Used by [`nodes.quip.network`](https://gitlab.com/quip.network/nodes.quip.network)
as the `faucet` profile in its docker-compose stack.

> **Never run against a production chain.** The startup check refuses to
> bind unless the connected chain reports a known dev name. See the
> `--allow-any-chain` flag below for the (deliberate) override.

> **Funder must be the chain sudo key.** The faucet mints via
> `Sudo.sudo(FaucetOps.mint)`, which the runtime authorizes only for
> `Sudo::Key`. At startup it reads the chain's `Sudo.Key` and **exits with an
> error if the funder doesn't match** — otherwise every mint would be accepted
> into the tx pool and silently dropped, so `/request` would return `200` yet
> never fund anything. `--pool-size 0` disables the `/sign` pool (it returns
> `503`); `/request` is unaffected.

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
| `403` | Destination already funded — free balance exceeds `--max-funded-balance-plancks` (default: one dispense; set 0 to deny any funds). Body includes `free_balance_plancks`. |
| `429` | Rate limited (`retry_after_seconds`). Confirmed-empty accounts use the short `--lenient-rate-limit-seconds`; if the balance query can't run, the strict `--rate-limit-seconds` applies. |
| `503` | `/sign` pool temporarily exhausted (`retry_after_seconds`), or balance check unavailable when `--balance-query-fail-closed`. |
| `502` | Transfer/sign failed; see logs. |

### Balance gate & rate limiting

Every request first checks the destination's on-chain free balance. Accounts
already holding more than `--max-funded-balance-plancks` (default: one dispense)
are denied (`403`); low or empty accounts are only lightly throttled
(`--lenient-rate-limit-seconds`, just long enough to bridge inclusion latency).
Set the lenient window `>=` chain block time. On a balance-query failure the
faucet falls back to the strict window and proceeds (`--balance-query-fail-open`,
the default) or denies (`--balance-query-fail-closed`).

### Examples (curl)

Fund an address. `amount` is in plancks and optional — omit it for the faucet
default:

```bash
curl -X POST http://localhost:8087/request \
    -H 'Content-Type: application/json' \
    -d '{"dest":"<ss58 or 0x-hex>","amount":1000000000000}'
# 200 {"extrinsic_hash":"0x…","amount":1000000000000,"dest":"…"}
```

Check whether an address is already funded. The balance gate runs before any
dispense, so the same endpoint reports a funded account's balance and spends
nothing — note it would *fund* an empty account, so this is a fund-or-report
call, not a pure read-only probe:

```bash
curl -X POST http://localhost:8087/request \
    -H 'Content-Type: application/json' \
    -d '{"dest":"<ss58 or 0x-hex>"}'
# 403 {"error":"destination already funded","free_balance_plancks":998996851040628}
```

For a read-only balance query independent of the faucet, the node also serves
JSON-RPC over HTTP (`state_getStorage` on the `System.Account` key); that needs
the storage key derived for the address, so reach for polkadot.js or a script
rather than curl alone.

### Multiple nodes (failover)

Repeat `--node-url` to add ordered fallbacks. The faucet connects to the first
reachable, verified dev node and, on a connection error, fails over to the next
(sticky — it stays on the healthy one). Timeouts are *not* failed over (a timed-
out tx may already be in a pool; the balance gate backstops the retry). All
nodes are assumed to be replicas of the same chain.

## Run locally

Needs SSH access to the private `quip-protocol-rs` repo (`.cargo/config.toml`
uses the git CLI for auth).

```bash
cargo run --release -- \
    --node-url ws://localhost:9944 \
    --faucet-key //Alice \
    --listen-host 127.0.0.1 \
    --port 8087
```

`quip-faucet --help` lists every flag (rate limit, log level, allow-any-chain
override).

## Run in Docker

The published image (`registry.gitlab.com/quip.network/faucet`, multi-arch
linux/amd64 + linux/arm64) runs the Rust faucet binary. Flags map directly to
its CLI (`--help`):

```bash
docker run --rm -p 8087:8087 \
    registry.gitlab.com/quip.network/faucet:latest \
    --node-url=ws://host.docker.internal:9944 \
    --faucet-key=//Alice \
    --listen-host=0.0.0.0 \
    --port=8087
```

Runtime environment variables (all optional):

- `PUID` / `PGID` — uid/gid the faucet runs as (default `1000`). The
  entrypoint remaps the internal `quip` user at start and drops privileges
  via gosu, matching the quip-network-node image's convention.
- `QUIP_FAUCET_ALLOW_ANY_CHAIN=1` — same as `--allow-any-chain`; `0`,
  `false`, or empty keep the dev-chain guard on. UNSAFE outside controlled
  environments.
- `QUIP_FAUCET_FAUCET_KEY` — same as `--faucet-key`.

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

- **sr25519** — vanilla `MultiSignature` chains.
- **hybrid** — `HybridTxSignature` chains (sr25519 + ML-DSA-44, FIPS 204).

Either way the funder key is derived from its SURI (`//Alice`, a raw seed, or a
mnemonic) via the shared `quip-transaction-crypto`/`quip-tools` crates, and the
pool and base accounts are hard-derived from it — so the signed extrinsic
envelope always matches the chain, with no hardcoded dev-seed table.

## Build, test & CI

Needs SSH access (local) or a CI job token to fetch the private
`quip-protocol-rs` dependency. Unit tests mock the chain, so no node is required.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --locked
```

CI compiles the binary in the substrate toolchain image once per architecture
(`build-binary-amd64`/`-arm64`, each on a native runner); kaniko packages each
prebuilt binary into a slim Debian image (`publish-image-<arch>`), and
`manifest` stitches them into a multi-arch (linux/amd64 + linux/arm64)
manifest list — matching the quip-network-node image.

## License

AGPL-3.0-or-later. See `LICENSE`.
