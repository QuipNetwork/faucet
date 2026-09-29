# Quip Faucet

Standalone dev faucet for [Quip Network](https://gitlab.com/quip.network)
substrate chains, compiled for the pinned Phase 2 runtime (spec 119, transaction 7).
A Foundation-appointed operational key submits direct signed `FaucetOps.mint`
to replenish a dedicated base wallet. The base wallet transfers to `/request`
recipients and pre-funds the `/sign` pool using concurrent nonce lanes.

The configured `--faucet-key` / `QUIP_FAUCET_FAUCET_KEY` must match on-chain
`FaucetOps.Authority` and hold enough balance to pay transaction fees. Startup,
each refresh (two-second interval), and each mint check finalized Authority,
Enabled fuse, remaining emission-controller faucet budget and runtime version.
Requests and pool signing use that cached result without repeating storage RPCs.
The previous success remains ready during refresh, expires after 12 seconds,
and is invalidated immediately when a check fails.
A mismatch, revoked key, closed fuse, exhausted budget or unreadable controls
stops service funding/signing; `/health` reports 503 when checks fail or become
stale. `--pool-size 0` disables `/sign` only.

Used by [`nodes.quip.network`](https://gitlab.com/quip.network/nodes.quip.network)
as the `faucet` profile in its docker-compose stack. The existing environment
variable is retained; operators must change its value to the appointed authority.
The startup chain-name guard requires a known dev chain unless explicitly
configured with `--allow-any-chain` for a controlled testnet.

## API

| Method & path | Body | Success |
|---|---|---|
<<<<<<< HEAD
| `POST /request` | `{"dest": "<ss58, 0x+64-hex account, or 0x+40-hex EVM address>", "amount": <plancks>}` | `200 {"extrinsic_hash", "block_hash", "amount", "dest", "dest_account"}` — faucet mints and broadcasts |
| `POST /sign` | `{"dest": "<ss58, 0x+64-hex account, or 0x+40-hex EVM address>", "amount": <plancks>}` | `200 {"signed_extrinsic", "extrinsic_hash", "nonce", "from", "amount", "dest", "dest_account", "mode"}` — receiver broadcasts |
=======
| `POST /request` | `{"dest": "<ss58 or 0x-hex>", "amount": <plancks>}` | `200 {"extrinsic_hash", "amount", "dest"}` — base wallet transfers and broadcasts |
| `POST /sign` | `{"dest": "<ss58 or 0x-hex>", "amount": <plancks>}` | `200 {"signed_extrinsic", "extrinsic_hash", "nonce", "from", "amount", "dest", "mode"}` — receiver broadcasts |
>>>>>>> fba560e (feat(faucet): mint directly with governed runtime 119 authority)
| `GET /health`   | —    | `200 {"status": "ok"}` |

`amount` is optional and defaults to `--amount` (one dispense). A request may
ask for at most `--max-amount-plancks`, which also defaults to one dispense;
anything larger gets a `400` naming the maximum. Raise the flag to allow
bigger one-off requests.

`dest` accepts an EVM (H160) address as `0x` + 40 hex chars. It is funded
through its pallet-revive mapped native account (`h160 ++ 0xEE*12`), which is
what the Ethereum JSON-RPC sidecar reports as the address's balance. The
resolved native account is returned as `dest_account` in every success
response.

`/sign` returns a `Balances.transfer_keep_alive` signed by a faucet **pool**
account (not the funder), for the receiver to submit via `author_submitExtrinsic`.
It is signed with the runtime's mortal era and is **single-use**: submit it promptly —
it is rejected as stale if that pool account is reused first; just call `/sign`
again for a fresh one. Hybrid-chain responses carry the H4 signature envelope
(sr25519 + FN-DSA-512), so they are larger than vanilla sr25519 transactions.

### Status codes

| Code | Meaning |
|---|---|
| `400` | Invalid JSON / `dest` / `amount`, including an `amount` above `--max-amount-plancks` (default: `--amount`, one dispense). |
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

Needs SSH access to the private `quip-validator` repo (`.cargo/config.toml`
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

This client uses H4 `HybridTxSignature` (sr25519 + FN-DSA-512) and the pinned
runtime's native signed-extension tuple. It does not auto-detect or support
vanilla sr25519 chains. Base/pool accounts are hard-derived from the configured
SURI. Rotating that key changes these derived accounts too; drain or explicitly
retain access to old wallets before retiring the old secret.

Calls, events and storage values use Rust types from the pinned runtime, rather
than generated Subxt bindings. The metadata export example records that same
schema for review; regenerate it whenever the runtime revision changes.

## Build, test & CI

Needs SSH access (local) or a CI job token to fetch the private
`quip-validator` dependency. Unit tests exercise encoded events and control checks without a node.

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo run --locked --example export_metadata -- metadata/runtime-119.scale
cargo build --release --locked
```

CI compiles the binary in the substrate toolchain image once per architecture
(`build-binary-amd64`/`-arm64`, each on a native runner); kaniko packages each
prebuilt binary into a slim Debian image (`publish-image-<arch>`), and
`manifest` stitches them into a multi-arch (linux/amd64 + linux/arm64)
manifest list — matching the quip-network-node image.

## License

AGPL-3.0-or-later. See `LICENSE`.

## Top-up receipts and runtime 119 rollout

Funder top-ups wait up to 120 seconds for finalization. Success requires
`System.ExtrinsicSuccess`, `FaucetOps.Minted`, and the canonical issuance event
`EmissionController.FaucetMinted`, with the requested recipient/amount and exact
submitted extrinsic index. Dispatch errors, missing/malformed/mismatched or
duplicate events fail closed. `FundingFailed` belongs to scheduled accrual in
initialization and is not a faucet mint receipt.

Pre-submission version failures, authoritative pool rejections and `invalid`
status with no inclusion history return errors without locking out the service.
An `invalid` status after `inBlock` or `retracted` is ambiguous: the same
transaction may have landed on the canonical branch. Definite stale-nonce/low-
priority rejections retry up to five submissions with fresh nonce/era context.
Transport failures, subscription loss, dropped/usurped/finalityTimeout status,
timeouts and unverified finalized receipts latch the service unavailable,
including background top-ups. Reconcile the exact transaction before restarting;
an ambiguous receipt never triggers another mint on the next monitor tick.
AlreadyImported and generic server errors are conservatively ambiguous.

Startup, context refreshes and transaction submissions retain exact
`specVersion` / `transactionVersion` guards. Control storage reads use one finalized block
hash, with strict SCALE decoding. Insufficient budget rejects a top-up before
submission; governance changes racing that check are still enforced on chain.
Existing signed pool transactions cannot be recalled by the service.

Foundation rotates/revokes the operational key through
`faucetOps.set_authority(Some(account))` / `None`. Mint call index is 0, disable
is 1, and set_authority is 2. Rotation changes neither the fuse nor budget nor
issued totals. Use a separate funded authority account in Akash, update its
secret after governance execution, then verify service readiness. A paused or
permanently disabled faucet cannot be revived by rotating the key.

Before switching secrets, stop serving requests/signatures and wait for any
handed-out pool transactions to finalize or expire. Derive the new base wallet
from the new authority SURI, then use the **old base wallet signer** to transfer
its remaining balance to that new base wallet. Drain unused old pool balances
with their old derived signers too, retaining enough for fees until complete.
Only then finalize the Foundation authority change, switch the service secret
and restart. Keep the old secret until all transfers are confirmed; changing the
authority alone does not move any base or pool funds.

The downstream compose repository owns its secret value and image pin; update
both to the approved runtime-119 build during rollout. This repository changes
no deployed credentials or chain state. Live mint/rotation/revocation tests and
published CI remain deployment gates until recorded as verified.
