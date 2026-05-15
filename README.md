# Quip Faucet

Standalone dev faucet for [Quip Network](https://gitlab.com/quip.network)
substrate chains. Listens on HTTP and submits
`Balances.transfer_keep_alive` extrinsics from a single funded account
(typically `//Alice` on a dev chain) to whichever destination is
requested. Per-destination rate-limited.

The bot is a verbatim deployable copy of `faucet_bot.py` from the
[`quip-protocol`](https://gitlab.com/quip.network/quip-protocol) project,
which authorizes standalone redistribution from its module docstring.

Used by [`nodes.quip.network`](https://gitlab.com/quip.network/nodes.quip.network)
as the `faucet` profile in its docker-compose stack.

> **Never run against a production chain.** The startup check refuses to
> bind unless the connected chain reports a known dev name. See the
> `--allow-any-chain` flag below for the (deliberate) override.

## API

| Method & path | Body | Success |
|---|---|---|
| `POST /request` | `{"dest": "<ss58 or 0x-hex>", "amount": <plancks>}` | `200 {"extrinsic_hash", "block_hash", "amount", "dest"}` |
| `GET /health`   | —    | `200 {"status": "ok"}` |

`429` is returned (with `retry_after_seconds`) when the same destination
requests within `--rate-limit-seconds` (default 60s).
`4xx` / `5xx` errors carry an `error` field.

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

## Signing modes

Auto-detected from chain metadata at startup:

- **sr25519** — vanilla `MultiSignature` chains. URI → keypair via
  substrate-interface.
- **hybrid** — `HybridTxSignature` chains (sr25519 + ML-DSA-44, FIPS 204).
  Funder URI must be one of the precomputed dev seeds (`//Alice`, `//Bob`,
  `//Alice//stash`); extrinsic is built byte-by-byte because
  substrate-interface doesn't know the hybrid envelope.

## Follow-ups

- Port the faucet test suite from `quip-protocol` (current v0 ships with
  none).
- Add a `HEALTHCHECK` directive (the `/health` endpoint is already there;
  just needs a Dockerfile line).

## License

AGPL-3.0-or-later. See `LICENSE`.
