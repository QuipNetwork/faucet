#!/usr/bin/env python3
"""Standalone dev-only faucet for substrate chains.

Listens on an HTTP endpoint and submits `Sudo.sudo(FaucetOps.mint)` from a
single funded account (typically `//Alice` on a dev chain) to whichever
destination is requested. Rate-limits per destination so a misbehaving caller
can't drain the funding key.

Designed to be deployable independently of the rest of `quip-protocol`:
copy this single file plus `substrate-interface`, `aiohttp`, `dilithium-py`,
and `blake3` and it runs. No imports from `shared/` and no other quip
modules.

**Two signing modes**:
  - **sr25519** — vanilla `MultiSignature` chains. The funder URI (default
    `//Alice`) is resolved by substrate-interface and extrinsics go through
    `create_signed_extrinsic`.
  - **hybrid** — chains that use the `HybridTxSignature` envelope
    (sr25519 + ML-DSA-44, FIPS 204 / quip-crypto-primitives). The funder URI
    is resolved against a precomputed `DEV_HYBRID_SEEDS` table because
    substrate's URI-to-master-seed path is non-trivial to replicate; we
    build the extrinsic bytes by hand because substrate-interface 1.8.1
    doesn't know about `HybridTxSignature`, `AuthorizeCall`, etc.

The mode is auto-detected by inspecting chain metadata at startup — no CLI
flag needed.

**Never run against a production chain.** The startup check refuses to bind
unless the connected chain's `System.chain` matches a known dev name. Pass
`--allow-any-chain` or set `QUIP_FAUCET_ALLOW_ANY_CHAIN=1` to override —
only do that in deliberately controlled environments.

Usage:
    python faucet_bot.py --node-url ws://localhost:9944 \\
        --faucet-key //Alice --listen 127.0.0.1 --port 8087

Funding request (faucet mints + broadcasts):
    POST /request  {"dest": "0x<32-byte hex>", "amount": <plancks>}
    Response 200: {"extrinsic_hash": "0x...", "block_hash": "0x...",
                   "amount": N, "dest": "..."}

Sign request (faucet signs a transfer from a pre-funded pool account; the
receiver broadcasts it via author_submitExtrinsic):
    POST /sign  {"dest": "0x<32-byte hex>", "amount": <plancks>}
    Response 200: {"signed_extrinsic": "0x...", "extrinsic_hash": "0x...",
                   "nonce": N, "from": "...", "amount": N, "dest": "...",
                   "mode": "sr25519"|"hybrid"}

Both endpoints query the destination's on-chain balance first: funded accounts
are denied (403); empty accounts are only lightly throttled (Goal 3/4).
    Response 403: destination already funded
    Response 429: rate limited (includes retry_after_seconds)
    Response 503: /sign pool exhausted, or balance check unavailable (fail-closed)
    Response 4xx/5xx: error detail in the JSON body

Pass --node-url more than once to add ordered failover nodes.
"""
from __future__ import annotations

import argparse
import asyncio
import contextlib
import hashlib
import hmac
import logging
import os
import socket
import sys
import time
from dataclasses import dataclass

from aiohttp import web
from dilithium_py.ml_dsa import ML_DSA_44
from scalecodec.utils.ss58 import ss58_decode, ss58_encode
from substrateinterface import Keypair, KeypairType, SubstrateInterface

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

# Dev chain names matched by prefix. `--chain=local3` reports
# "Local Testnet (3 Validators)" so a prefix match keeps the list short.
DEV_CHAIN_PREFIXES = (
    "Development",
    "Local Testnet",
    "quip-local",
)

# One funding per minute per destination address. With the on-chain balance gate
# (see `_check_gate`) this is now the *fallback* window, applied only when the
# balance query can't run; confirmed-empty accounts use the lenient window below.
DEFAULT_RATE_LIMIT_SECONDS = 60

# Lenient per-destination window applied to confirmed zero-balance accounts. Its
# only job is to bridge inclusion latency so a flood of requests can't double-fund
# the same empty account before the prior mint lands and updates the balance. Set
# >= chain block time.
DEFAULT_LENIENT_RATE_LIMIT_SECONDS = 5

# Default funding amount: 1000 UNIT assuming 12-decimal chains (matches
# standard Substrate dev presets). Override per request.
#
# "Planck" is the Substrate/Polkadot term for the smallest divisible balance
# unit (analogous to `wei` on Ethereum or `satoshi` on Bitcoin). 1 UNIT =
# 10^12 plancks on 12-decimal chains.
DEFAULT_AMOUNT_PLANCKS = 1_000_000_000_000_000

# Pre-funded pool backing the `/sign` endpoint (see `_init_pool`). Defaults are
# tuned for a dev chain: a handful of accounts, each funded for many handouts,
# refilled in the background. The pool isolates `/sign` from the funder's nonce
# and auto-doubles (up to `pool_max_size`) when it detects nonce reuse under load.
DEFAULT_POOL_SIZE = 8
DEFAULT_POOL_MAX_SIZE = 64
DEFAULT_POOL_FUND_AMOUNT = 100 * DEFAULT_AMOUNT_PLANCKS
DEFAULT_POOL_LOW_WATERMARK = 10 * DEFAULT_AMOUNT_PLANCKS
DEFAULT_POOL_COOLDOWN_SECONDS = 20.0
DEFAULT_POOL_REPLENISH_INTERVAL_SECONDS = 30.0
DEFAULT_POOL_IDLE_GROW_SECONDS = 30.0

# Domain separation for deterministic pool-account derivation:
# seed_i = blake2b(POOL_DERIVATION_LABEL || funder_secret || index).
POOL_DERIVATION_LABEL = b"quip-faucet-pool-v1"


# Hybrid suite constants — must match `quip-crypto-primitives` byte-for-byte
# (mirrors `shared/hybrid_signer.py`; kept inline so this file stays
# standalone). Pinned by the parity tests in tests/test_hybrid_signer.py.
HYBRID_LABEL = b"hybrid-sr25519-mldsa44-v1\0"  # 26 bytes
HYBRID_VERSION = 0x01
HKDF_SALT = b"hybrid-sig"
HKDF_CLASSICAL_INFO = b"classical"
HKDF_PQ_INFO = b"pq"
MASTER_SEED_LEN = 32
ACCOUNT_ID_DOMAIN = b"quip-account-v1"

SR_PK_LEN = 32
SR_SIG_LEN = 64
ML_PK_LEN = 1312
ML_SIG_LEN = 2420
HYBRID_PK_LEN = SR_PK_LEN + ML_PK_LEN          # 1344
HYBRID_SIG_LEN = SR_SIG_LEN + ML_SIG_LEN       # 2484


# Terminal transaction-pool states returned by `author_submitAndWatchExtrinsic`
# that indicate the extrinsic will never finalize. Without enumerating all of
# them the subscription handler silently waits forever on a `usurped` /
# `retracted` / `finalityTimeout` response. Mirrors
# `shared/substrate_client._HYBRID_TERMINAL_FAILURES` byte-for-byte so the
# inline copy here doesn't drift from the canonical version.
_HYBRID_TERMINAL_FAILURES = frozenset(
    {"dropped", "invalid", "usurped", "retracted", "finalitytimeout"}
)


# Transport/connection errors that justify reconnecting and — with multiple nodes
# configured — failing over to the next node. Deliberately EXCLUDES TimeoutError:
# a timeout may mean the extrinsic already reached a pool, so resubmitting to
# another node could double-fund. A timeout surfaces as a 502 (no retry); the
# on-chain balance gate backstops the caller's retry (the account now has funds).
_TRANSPORT_ERRORS: tuple[type[Exception], ...] = (
    BrokenPipeError,
    ConnectionError,
    OSError,
    EOFError,
)


# Dev URI → master seed. Generated by the Rust helper
# `quip-protocol-rs/crates/transaction-crypto/examples/dump_dev_seeds.rs` —
# regenerate if the chain's dev mnemonic / derivation path changes. These
# are the *master* seeds; HKDF expands each into a (sr25519_seed, ml_dsa_seed)
# pair via `derive_component_seeds`.
DEV_HYBRID_SEEDS = {
    "//Alice": bytes.fromhex(
        "e5be9a5092b81bca64be81d212e7f2f9eba183bb7a90954f7b76361f6edb5c0a"
    ),
    "//Bob": bytes.fromhex(
        "398f0c28f98885e046333d4a41c19cee4c37368a9832c6502f6cfd182e2aef89"
    ),
    "//Alice//stash": bytes.fromhex(
        "3c881bc4d45926680c64a7f9315eeda3dd287f8d598f3653d7c107799c5422b3"
    ),
}


logger = logging.getLogger("faucet_bot")


@dataclass
class FaucetConfig:
    node_urls: list[str]  # ordered; index 0 is primary, the rest are failovers
    faucet_key_uri: str = "//Alice"
    listen_host: str = "127.0.0.1"
    listen_port: int = 8087
    # Float so tests + CLI can pass sub-second values without an implicit
    # int truncation. Subtraction at the throttle site already works for
    # both int and float input.
    rate_limit_seconds: float = float(DEFAULT_RATE_LIMIT_SECONDS)
    lenient_rate_limit_seconds: float = float(DEFAULT_LENIENT_RATE_LIMIT_SECONDS)
    # Deny a /request or /sign when the destination's free balance exceeds this
    # (Goal 4). Default 0 => deny if the account holds any free balance.
    max_funded_balance_plancks: int = 0
    # When the on-chain balance query fails: True => fall back to the strict
    # window and proceed (availability); False => deny with 503 (hard guarantee).
    balance_query_fail_open: bool = True
    allow_any_chain: bool = False
    pool_size: int = DEFAULT_POOL_SIZE
    pool_max_size: int = DEFAULT_POOL_MAX_SIZE
    pool_fund_amount: int = DEFAULT_POOL_FUND_AMOUNT
    pool_low_watermark: int = DEFAULT_POOL_LOW_WATERMARK
    pool_cooldown_seconds: float = DEFAULT_POOL_COOLDOWN_SECONDS
    pool_replenish_interval_seconds: float = DEFAULT_POOL_REPLENISH_INTERVAL_SECONDS
    pool_idle_grow_seconds: float = DEFAULT_POOL_IDLE_GROW_SECONDS

    def __post_init__(self) -> None:
        if not self.node_urls:
            raise ValueError("node_urls must be non-empty")


@dataclass
class _PoolAccount:
    """One pre-funded account backing `/sign`.

    `account_id_hex` is the canonical chain AccountId ("0x" + 32-byte hex) used
    both as the `System.Account` key and as the `get_account_nonce` argument. For
    hybrid accounts this is the *derived* id (`signer.account_id_bytes()`), NOT the
    sr25519 public key. Exactly one of `signer` / `keypair` is populated, per mode.
    """

    index: int
    signer: _HybridSigner | None
    keypair: Keypair | None
    account_id_hex: str
    ss58: str
    tracked_free: int = 0
    last_handed_out: float = 0.0
    last_handed_nonce: int | None = None
    in_flight: bool = False


# ---------------------------------------------------------------------------
# Hybrid signer (inlined — see shared/hybrid_signer.py for the canonical
# version used by the rest of quip-miner). Kept minimal here: we only need
# from_master_seed, account_id_bytes, public_bytes, and sign.
# ---------------------------------------------------------------------------


def _hkdf_extract_expand(*, salt: bytes, ikm: bytes, info: bytes, length: int) -> bytes:
    """Single-block (length<=32) HKDF-SHA256 extract + expand (RFC 5869)."""
    if length > 32:
        raise ValueError("single-block HKDF only supports length<=32")
    prk = hmac.new(salt, ikm, hashlib.sha256).digest()
    t = hmac.new(prk, info + bytes([1]), hashlib.sha256).digest()
    return t[:length]


def _derive_component_seeds(master_seed: bytes) -> tuple[bytes, bytes]:
    """Expand a 32-byte master seed into (classical_seed, pq_seed)."""
    if len(master_seed) != MASTER_SEED_LEN:
        raise ValueError(
            f"master_seed must be {MASTER_SEED_LEN} bytes, got {len(master_seed)}"
        )
    classical = _hkdf_extract_expand(
        salt=HKDF_SALT, ikm=master_seed, info=HKDF_CLASSICAL_INFO, length=MASTER_SEED_LEN
    )
    pq = _hkdf_extract_expand(
        salt=HKDF_SALT, ikm=master_seed, info=HKDF_PQ_INFO, length=MASTER_SEED_LEN
    )
    return classical, pq


def _prepare_message(msg: bytes, *, ctx: bytes = b"") -> bytes:
    """M' = version || label || len(ctx) || ctx || msg."""
    if len(ctx) > 255:
        raise ValueError(f"ctx must be at most 255 bytes, got {len(ctx)}")
    return bytes([HYBRID_VERSION]) + HYBRID_LABEL + bytes([len(ctx)]) + ctx + msg


def _derive_account_id(hybrid_public: bytes) -> bytes:
    """AccountId32 = blake2_256(b"quip-account-v1" || HybridPublic)."""
    if len(hybrid_public) != HYBRID_PK_LEN:
        raise ValueError(
            f"hybrid_public must be {HYBRID_PK_LEN} bytes, got {len(hybrid_public)}"
        )
    return hashlib.blake2b(ACCOUNT_ID_DOMAIN + hybrid_public, digest_size=32).digest()


class _HybridSigner:
    """Minimal sr25519 + ML-DSA-44 composite signer for the faucet."""

    def __init__(self, master_seed: bytes) -> None:
        if len(master_seed) != MASTER_SEED_LEN:
            raise ValueError(
                f"master_seed must be {MASTER_SEED_LEN} bytes, got {len(master_seed)}"
            )
        classical_seed, pq_seed = _derive_component_seeds(master_seed)
        self._sr = Keypair.create_from_seed(
            seed_hex=classical_seed.hex(),
            crypto_type=KeypairType.SR25519,
            ss58_format=42,
        )
        self._ml_pk, self._ml_sk = ML_DSA_44.key_derive(pq_seed)

    def public_bytes(self) -> bytes:
        return bytes(self._sr.public_key) + self._ml_pk

    def account_id_bytes(self) -> bytes:
        return _derive_account_id(self.public_bytes())

    def ss58_address(self) -> str:
        return ss58_encode(self.account_id_bytes(), ss58_format=42)

    def sign(self, payload: bytes) -> bytes:
        msg_prime = _prepare_message(payload)
        sr_sig = self._sr.sign(data=msg_prime)
        if isinstance(sr_sig, str):
            sr_sig = bytes.fromhex(sr_sig[2:] if sr_sig.startswith("0x") else sr_sig)
        sr_sig = bytes(sr_sig)
        ml_sig = ML_DSA_44.sign(self._ml_sk, msg_prime)
        if len(sr_sig) != SR_SIG_LEN or len(ml_sig) != ML_SIG_LEN:
            raise RuntimeError(
                f"component signature length mismatch: sr={len(sr_sig)} ml={len(ml_sig)}"
            )
        return sr_sig + ml_sig


# ---------------------------------------------------------------------------
# SCALE compact encoders + manual v4 extrinsic builder (hybrid path)
# ---------------------------------------------------------------------------


def _strip_0x(s: str) -> str:
    return s[2:] if s.startswith("0x") else s


def _is_err_variant(value) -> bool:
    if value is None:
        return False
    if isinstance(value, dict):
        if "Err" in value:
            return True
        if "Ok" in value:
            return False
        return any(_is_err_variant(v) for v in value.values())
    if isinstance(value, (list, tuple)):
        return any(_is_err_variant(v) for v in value)
    if isinstance(value, str):
        lower = value.lower()
        return lower == "err" or lower.startswith("err(")
    return False


def _compose_sudo_mint_call(iface: SubstrateInterface, *, dest: str, amount: int):
    inner = iface.compose_call(
        call_module="FaucetOps",
        call_function="mint",
        call_params={"who": dest, "amount": amount},
    )
    return iface.compose_call(
        call_module="Sudo",
        call_function="sudo",
        call_params={"call": inner},
    )


def _compose_transfer_call(iface: SubstrateInterface, *, dest: str, amount: int):
    """`Balances.transfer_keep_alive(dest, value)` — signed by a pool account.

    keep_alive (not allow_death) so a pool account never reaps itself below the
    existential deposit and stays reusable.
    """
    return iface.compose_call(
        call_module="Balances",
        call_function="transfer_keep_alive",
        call_params={"dest": {"Id": dest}, "value": amount},
    )


def _encode_compact_u32(n: int) -> bytes:
    if n < 0:
        raise ValueError(f"compact must be non-negative, got {n}")
    if n < 0x40:
        return bytes([n << 2])
    if n < 0x4000:
        return ((n << 2) | 0b01).to_bytes(2, "little")
    if n < 0x4000_0000:
        return ((n << 2) | 0b10).to_bytes(4, "little")
    raise NotImplementedError("compact u32 big-int mode not needed here")


def _encode_compact_u128(n: int) -> bytes:
    if n < 0:
        raise ValueError(f"compact must be non-negative, got {n}")
    if n < 0x40:
        return bytes([n << 2])
    if n < 0x4000:
        return ((n << 2) | 0b01).to_bytes(2, "little")
    if n < 0x4000_0000:
        return ((n << 2) | 0b10).to_bytes(4, "little")
    raw = n.to_bytes((n.bit_length() + 7) // 8, "little")
    # SCALE compact big-int mode caps at 67 bytes: the top 6 bits of the
    # mode byte encode `n_bytes - 4`, so max n_bytes = (0xff >> 2) + 4 = 67.
    if len(raw) > 67:
        raise OverflowError(
            f"compact value needs {len(raw)} bytes, exceeds 67-byte SCALE limit"
        )
    return bytes([((len(raw) - 4) << 2) | 0b11]) + raw


def _fetch_extrinsic_dispatch_error(
    iface: SubstrateInterface, *, block_hash: str, ext_hash: str
) -> str | None:
    """Return a stringified dispatch failure for an included extrinsic.

    The faucet uses `sudo(FaucetOps.mint)`, so success means both:
      - no `System.ExtrinsicFailed` on the outer extrinsic
      - no `Sudo.Sudid { sudo_result: Err(..) }` on the inner root call
    """
    block = iface.get_block(block_hash=block_hash, include_author=False)
    if block is None:
        return None
    target_hash = _strip_0x(ext_hash).lower()
    extrinsics = block.get("extrinsics") or []
    ext_idx: int | None = None
    for idx, ext in enumerate(extrinsics):
        eh = getattr(ext, "extrinsic_hash", None) or (
            ext.get("extrinsic_hash") if isinstance(ext, dict) else None
        )
        if eh and _strip_0x(str(eh)).lower() == target_hash:
            ext_idx = idx
            break
    if ext_idx is None:
        return None
    events = iface.get_events(block_hash=block_hash) or []
    for ev in events:
        v = ev.value if hasattr(ev, "value") else (ev if isinstance(ev, dict) else None)
        if not isinstance(v, dict):
            continue
        phase = v.get("phase") or {}
        if isinstance(phase, dict):
            applied = phase.get("ApplyExtrinsic")
            if applied is None or int(applied) != ext_idx:
                continue
        elif isinstance(phase, (int, str)):
            try:
                if int(phase) != ext_idx:
                    continue
            except (ValueError, TypeError):
                continue
        else:
            continue
        event = v.get("event") or v
        module_id = event.get("module_id") or event.get("pallet")
        event_id = event.get("event_id") or event.get("variant") or event.get("name")
        attrs = event.get("attributes") or event.get("fields") or {}
        if module_id == "System" and event_id == "ExtrinsicFailed":
            return f"Module(ExtrinsicFailed, attrs={attrs!r})"
        if module_id == "Sudo" and event_id == "Sudid":
            if isinstance(attrs, dict):
                sudo_result = attrs.get("sudo_result")
            elif isinstance(attrs, (list, tuple)) and attrs:
                sudo_result = attrs[0]
            else:
                sudo_result = attrs
            if _is_err_variant(sudo_result):
                return f"Module(Sudid, attrs={attrs!r})"
    return None


def _build_hybrid_signed_extrinsic(
    *,
    iface: SubstrateInterface,
    signer: _HybridSigner,
    call,
) -> tuple[bytes, str]:
    """Construct a hybrid-signed v4 extrinsic byte-by-byte.

    Mirrors `shared/substrate_client._build_hybrid_signed_extrinsic`. See
    that function's docstring for the full layout reference.
    """
    raw_call = call.data.data if hasattr(call.data, "data") else call.data
    if hasattr(raw_call, "tobytes"):
        call_bytes = bytes(raw_call)
    elif isinstance(raw_call, str):
        call_bytes = bytes.fromhex(_strip_0x(raw_call))
    else:
        call_bytes = bytes(raw_call)

    account = signer.account_id_bytes()
    nonce = iface.get_account_nonce(account_address="0x" + account.hex())
    genesis_hex = iface.get_block_hash(block_id=0)
    rv = iface.rpc_request("state_getRuntimeVersion", [])["result"]
    spec_version = int(rv["specVersion"])
    tx_version = int(rv["transactionVersion"])
    genesis_bytes = bytes.fromhex(_strip_0x(genesis_hex))

    extra = (
        b""                                  # AuthorizeCall
        + b""                                # CheckNonZeroSender
        + b""                                # CheckSpecVersion
        + b""                                # CheckTxVersion
        + b""                                # CheckGenesis
        + b"\x00"                            # CheckMortality: Era::immortal
        + _encode_compact_u32(int(nonce))    # CheckNonce
        + b""                                # CheckWeight
        + _encode_compact_u128(0)            # ChargeTransactionPayment tip=0
        + b"\x00"                            # CheckMetadataHash: Mode::Disabled
        + b""                                # WeightReclaim
    )
    additional = (
        b""                                  # AuthorizeCall
        + b""                                # CheckNonZeroSender
        + spec_version.to_bytes(4, "little") # CheckSpecVersion
        + tx_version.to_bytes(4, "little")   # CheckTxVersion
        + genesis_bytes                      # CheckGenesis
        + genesis_bytes                      # CheckMortality (immortal -> genesis)
        + b""                                # CheckNonce
        + b""                                # CheckWeight
        + b""                                # ChargeTransactionPayment
        + b"\x00"                            # CheckMetadataHash: Option::None
        + b""                                # WeightReclaim
    )

    payload = call_bytes + extra + additional
    payload_to_sign = (
        hashlib.blake2b(payload, digest_size=32).digest()
        if len(payload) > 256
        else payload
    )
    signature_bytes = signer.sign(payload_to_sign)
    hybrid_sig_scale = signer.public_bytes() + signature_bytes

    body = (
        bytes([0x84])                        # v4 | 0x80 signed flag
        + b"\x00"                            # MultiAddress::Id discriminator
        + account
        + hybrid_sig_scale
        + extra
        + call_bytes
    )
    full_extrinsic = _encode_compact_u32(len(body)) + body
    ext_hash = "0x" + hashlib.blake2b(full_extrinsic, digest_size=32).digest().hex()
    return full_extrinsic, ext_hash


def _chain_uses_hybrid_signature(iface: SubstrateInterface) -> bool:
    """Inspect runtime metadata for `HybridTxSignature` in the type table.

    Returns True if the chain's extrinsic signature type is
    `quip_transaction_crypto::HybridTxSignature`; False for vanilla
    `MultiSignature` chains. Substrate's runtime metadata is versioned
    (V14, V15, ...); the type table is reached under whatever version
    key the chain returned. Iterate over the available versions rather
    than hard-coding "V14" so a chain upgraded to V15 doesn't silently
    fall back to sr25519 mode.

    Raises `RuntimeError` if metadata can't be introspected at all —
    silently defaulting to sr25519 on a hybrid chain produces opaque
    submission failures downstream and is the worst place to fail soft.
    """
    md = iface.get_metadata()
    # md.value is a tuple `(magic_bytes, {version_key: {...}})` for V14+
    # metadata. Tolerate either shape (`(magic, table)` or a dict).
    table = md.value[1] if isinstance(md.value, (list, tuple)) else md.value
    if not isinstance(table, dict):
        raise RuntimeError(
            f"unexpected metadata shape from chain: {type(table).__name__}"
        )
    types_list = None
    for version_key, payload in table.items():
        if not isinstance(payload, dict):
            continue
        types_section = payload.get("types")
        if isinstance(types_section, dict):
            candidate = types_section.get("types")
            if isinstance(candidate, list):
                types_list = candidate
                logger.info("metadata version detected: %s", version_key)
                break
    if types_list is None:
        raise RuntimeError(
            "could not locate `types.types` in chain metadata; "
            f"keys present: {sorted(table.keys())!r}. The chain may be "
            "running an unsupported metadata version."
        )
    for t in types_list:
        path = t["type"].get("path") or []
        if "HybridTxSignature" in path:
            return True
    return False


# ---------------------------------------------------------------------------
# Faucet service
# ---------------------------------------------------------------------------


class SubstrateFaucet:
    """HTTP service that signs `Sudo.sudo(FaucetOps.mint)` extrinsics."""

    def __init__(self, config: FaucetConfig) -> None:
        self.config = config
        self._iface: SubstrateInterface | None = None
        # Index into config.node_urls of the currently-connected node. Sticky:
        # once failover lands on a healthy node we stay there across requests.
        self._active_idx: int = 0
        # One of these is populated after `start()` per the detected chain
        # mode. The vanilla `Keypair` path is what substrate-interface 1.8.1
        # expects; the `_HybridSigner` is the post-Phase 7 composite.
        self._sr_keypair: Keypair | None = None
        self._hybrid_signer: _HybridSigner | None = None
        self._is_hybrid = False
        self._funder_address: str = ""  # for logging / display
        # Raw funder secret seed bytes, captured in `_init_funder`; the pool
        # accounts are derived deterministically from it (see `_derive_pool_account`).
        self._funder_secret: bytes = b""
        self._existential_deposit: int = 0
        self._last_funded: dict[str, float] = {}
        # Destinations currently mid-request (gate reservation). Prevents two
        # concurrent requests for the same empty account from both funding it.
        self._in_flight: set[str] = set()
        self._rate_limit_lock = asyncio.Lock()
        # Pre-funded pool backing `/sign` (see `_init_pool`).
        self._pool: list[_PoolAccount] = []
        self._pool_lock = asyncio.Lock()
        self._rr_cursor: int = 0
        # Latched when `/sign` observes nonce reuse; the replenish loop doubles the
        # pool during the next idle window.
        self._grow_pending: bool = False
        self._last_sign_activity: float = 0.0
        self._replenish_task: asyncio.Task[None] | None = None
        # `SubstrateInterface` keeps a single ws connection and isn't safe
        # against concurrent calls. The faucet serializes every chain
        # interaction (compose / sign / submit) behind this lock. For a dev
        # faucet the loss of parallelism is fine, and it prevents the
        # Broken-pipe and torn-JSON failure modes that show up under load.
        self._chain_lock = asyncio.Lock()
        self._runner: web.AppRunner | None = None
        self._loop: asyncio.AbstractEventLoop | None = None

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._loop = asyncio.get_running_loop()
        await self._connect_first_reachable()
        self._is_hybrid = await self._run(
            lambda: _chain_uses_hybrid_signature(self._ifc)
        )
        self._init_funder()
        # Fund the pool before binding so /sign never serves an empty pool.
        await self._init_pool()

        app = web.Application()
        app.router.add_post("/request", self._handle_faucet)
        app.router.add_post("/sign", self._handle_sign)
        app.router.add_get("/health", self._handle_health)

        self._runner = web.AppRunner(app)
        await self._runner.setup()
        site = web.TCPSite(
            self._runner,
            host=self.config.listen_host,
            port=self.config.listen_port,
        )
        await site.start()
        self._replenish_task = asyncio.create_task(self._replenish_loop())
        logger.info(
            "faucet listening: http://%s:%d mode=%s funder=%s pool=%d",
            self.config.listen_host,
            self.config.listen_port,
            "hybrid" if self._is_hybrid else "sr25519",
            self._funder_address,
            len(self._pool),
        )

    async def stop(self) -> None:
        if self._replenish_task is not None:
            self._replenish_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._replenish_task
            self._replenish_task = None
        if self._runner is not None:
            await self._runner.cleanup()
            self._runner = None
        if self._iface is not None:
            iface = self._iface
            self._iface = None
            def _close() -> None:
                with contextlib.suppress(AttributeError):  # older substrate-interface
                    iface.close()
            await self._run(_close)

    @property
    def _ifc(self) -> SubstrateInterface:
        """The connected chain client; raises if used before start()/after stop()."""
        if self._iface is None:
            raise RuntimeError("chain client not connected")
        return self._iface

    def _read_existential_deposit(self) -> int:
        const = self._ifc.get_constant("Balances", "ExistentialDeposit")
        if const is None or const.value is None:
            return 0
        return int(const.value)

    def _init_funder(self) -> None:
        """Resolve the configured funder URI to the right signer type."""
        uri = self.config.faucet_key_uri
        if self._is_hybrid:
            if uri not in DEV_HYBRID_SEEDS:
                raise RuntimeError(
                    f"hybrid chain requires a known dev URI for the funder "
                    f"(got {uri!r}); known: {sorted(DEV_HYBRID_SEEDS)}. "
                    "Substrate URI derivation can't reproduce the master seed "
                    "for arbitrary URIs — generate keys with the dump_dev_seeds "
                    "Rust example and add them to DEV_HYBRID_SEEDS to extend."
                )
            self._funder_secret = DEV_HYBRID_SEEDS[uri]
            self._hybrid_signer = _HybridSigner(self._funder_secret)
            self._funder_address = self._hybrid_signer.ss58_address()
        else:
            self._sr_keypair = Keypair.create_from_uri(
                uri, crypto_type=KeypairType.SR25519
            )
            # Stable secret bytes for deterministic pool derivation. private_key is
            # always populated for a URI-derived keypair; fall back to the seed.
            secret = getattr(self._sr_keypair, "private_key", None)
            if not secret:
                seed_hex = getattr(self._sr_keypair, "seed_hex", None)
                secret = bytes.fromhex(_strip_0x(seed_hex)) if seed_hex else b""
            self._funder_secret = bytes(secret)
            self._funder_address = self._sr_keypair.ss58_address

    # ------------------------------------------------------------------
    # Pre-funded pool backing /sign (Goal 1)
    # ------------------------------------------------------------------

    def _derive_pool_account(self, index: int) -> _PoolAccount:
        """Deterministically derive pool account `index` from the funder secret.

        seed_i = blake2b(POOL_DERIVATION_LABEL || funder_secret || index). Same
        accounts every boot (no key file, no stranding); growth just derives higher
        indices. Mode follows the funder: hybrid → `_HybridSigner`, else sr25519.
        """
        seed = hashlib.blake2b(
            POOL_DERIVATION_LABEL + self._funder_secret + index.to_bytes(4, "big"),
            digest_size=32,
        ).digest()
        if self._is_hybrid:
            signer = _HybridSigner(seed)
            return _PoolAccount(
                index=index,
                signer=signer,
                keypair=None,
                account_id_hex="0x" + signer.account_id_bytes().hex(),
                ss58=signer.ss58_address(),
            )
        keypair = Keypair.create_from_seed(
            seed_hex=seed.hex(), crypto_type=KeypairType.SR25519, ss58_format=42
        )
        return _PoolAccount(
            index=index,
            signer=None,
            keypair=keypair,
            account_id_hex="0x" + bytes(keypair.public_key).hex(),
            ss58=keypair.ss58_address,
        )

    def _build_sign_only(self, pool: _PoolAccount, dest: str, amount: int) -> dict:
        """(executor thread) Sign `Balances.transfer_keep_alive` from `pool`, no
        broadcast. Caller holds _chain_lock. Returns the submittable hex, hash,
        the funder-pool nonce baked in, and the mode.

        Re-fetches the pool account's on-chain nonce fresh each time, so there is
        never a local counter and never a gapped/stalling nonce: if the prior
        handout landed the nonce advanced; if not, we re-issue the same nonce and
        the older handout simply goes stale. A non-advancing nonce means the buffer
        wrapped before the receiver submitted — latch grow_pending.
        """

        def _do():
            call = _compose_transfer_call(self._ifc, dest=dest, amount=amount)
            nonce = int(self._ifc.get_account_nonce(pool.account_id_hex))
            if self._is_hybrid:
                assert pool.signer is not None  # hybrid mode invariant
                ext_bytes, ext_hash = _build_hybrid_signed_extrinsic(
                    iface=self._ifc, signer=pool.signer, call=call
                )
                signed_hex = "0x" + ext_bytes.hex()
            else:
                assert pool.keypair is not None  # sr25519 mode invariant
                extrinsic = self._ifc.create_signed_extrinsic(
                    call=call, keypair=pool.keypair, nonce=nonce
                )
                signed_hex = extrinsic.data.to_hex()
                ext_hash = "0x" + hashlib.blake2b(
                    bytes.fromhex(_strip_0x(signed_hex)), digest_size=32
                ).hexdigest()
            return {
                "signed_extrinsic": signed_hex,
                "extrinsic_hash": ext_hash,
                "nonce": nonce,
                "mode": "hybrid" if self._is_hybrid else "sr25519",
            }

        result = self._call_with_failover(_do, "sign")
        nonce = result["nonce"]
        if pool.last_handed_nonce is not None and nonce <= pool.last_handed_nonce:
            self._grow_pending = True
            logger.info(
                "pool account[%d] nonce reuse (nonce=%d <= last=%d); growth pending",
                pool.index,
                nonce,
                pool.last_handed_nonce,
            )
        pool.last_handed_nonce = nonce
        return result

    async def _allocate_pool_account(self, amount: int) -> _PoolAccount | None:
        """Pick a pool account to sign a `transfer_keep_alive` of `amount`.

        This is the heart of how `/sign` shares its buffer. Returns an eligible
        account with `in_flight` set (reserved), or ``None`` if none is eligible
        (the handler turns ``None`` into a 503 + retry hint).

        Eligibility for an account: NOT `in_flight`; `tracked_free >= amount +
        self._existential_deposit` (keep_alive must leave it above ED); and past
        its reuse cooldown, i.e. `now - last_handed_out >= pool_cooldown_seconds`.
        The cooldown is what gives a receiver time to broadcast before that
        account's nonce could be reused.

        Rotation: scan round-robin from `self._rr_cursor` so load (and therefore
        nonce reuse) spreads evenly across the buffer; on a match, reserve it and
        advance the cursor past it. All reads/writes of the pool happen under
        `self._pool_lock`.
        """
        needed = amount + self._existential_deposit
        now = time.monotonic()
        async with self._pool_lock:
            n = len(self._pool)
            for offset in range(n):
                cand = self._pool[(self._rr_cursor + offset) % n]
                if cand.in_flight:
                    continue
                if cand.tracked_free < needed:
                    continue
                if now - cand.last_handed_out < self.config.pool_cooldown_seconds:
                    continue
                cand.in_flight = True
                self._rr_cursor = (self._rr_cursor + offset + 1) % n
                return cand
            return None

    async def _fund_pool_account(self, acct: _PoolAccount) -> None:
        """Mint `pool_fund_amount` to a pool account from the sudo funder."""
        await self._run_chain(
            lambda: self._submit_transfer(
                dest=acct.account_id_hex, amount=self.config.pool_fund_amount
            )
        )
        acct.tracked_free += self.config.pool_fund_amount

    async def _ensure_pool_account(self, index: int) -> _PoolAccount:
        """Derive pool account `index`, reconcile its balance, fund if low, add it."""
        acct = self._derive_pool_account(index)
        acct.tracked_free = await self._run_chain(
            lambda: self._query_free_balance(acct.account_id_hex)
        )
        if acct.tracked_free < self.config.pool_low_watermark:
            await self._fund_pool_account(acct)
        async with self._pool_lock:
            self._pool.append(acct)
        return acct

    async def _init_pool(self) -> None:
        """Derive + fund the pool, idempotent against chain state.

        Base indices 0..pool_size-1 are always present. Then adopt any prior
        growth: a doubled pool leaves a contiguous funded prefix of higher
        indices, so probe upward and stop at the first unfunded index.
        """
        self._existential_deposit = await self._run_chain(self._read_existential_deposit)
        for index in range(self.config.pool_size):
            await self._ensure_pool_account(index)

        index = self.config.pool_size
        while index < self.config.pool_max_size:
            acct = self._derive_pool_account(index)
            free = await self._run_chain(
                lambda a=acct: self._query_free_balance(a.account_id_hex)
            )
            if free <= self._existential_deposit:
                break  # first unfunded index ends the previously-grown prefix
            acct.tracked_free = free
            if free < self.config.pool_low_watermark:
                await self._fund_pool_account(acct)
            async with self._pool_lock:
                self._pool.append(acct)
            index += 1

        logger.info(
            "faucet pool ready: %d accounts (mode=%s)",
            len(self._pool),
            "hybrid" if self._is_hybrid else "sr25519",
        )

    async def _maybe_grow_pool(self) -> None:
        """Double the pool if growth is pending and the faucet is idle."""
        if not self._grow_pending:
            return
        if time.monotonic() - self._last_sign_activity < self.config.pool_idle_grow_seconds:
            return  # not idle yet — defer; the flag stays latched
        current = len(self._pool)
        if current >= self.config.pool_max_size:
            self._grow_pending = False
            return
        new_size = min(current * 2, self.config.pool_max_size)
        for index in range(current, new_size):
            await self._ensure_pool_account(index)
        self._grow_pending = False
        logger.info("pool grown %d -> %d accounts", current, len(self._pool))

    async def _replenish_once(self) -> None:
        """One reconcile + refill + maybe-grow cycle."""
        for acct in list(self._pool):
            free = await self._run_chain(
                lambda a=acct: self._query_free_balance(a.account_id_hex)
            )
            async with self._pool_lock:
                acct.tracked_free = free
        for acct in list(self._pool):
            if acct.tracked_free < self.config.pool_low_watermark:
                await self._fund_pool_account(acct)
        await self._maybe_grow_pool()

    async def _replenish_loop(self) -> None:
        """Background: reconcile balances, refill low accounts, grow when idle."""
        while True:
            try:
                await asyncio.sleep(self.config.pool_replenish_interval_seconds)
                await self._replenish_once()
            except asyncio.CancelledError:
                raise
            except Exception:  # noqa: BLE001 — one bad cycle must not kill the loop
                logger.exception("pool replenish cycle failed")

    # ------------------------------------------------------------------
    # Handlers
    # ------------------------------------------------------------------

    async def _handle_health(self, _request: web.Request) -> web.Response:
        return web.json_response({"status": "ok"})

    async def _parse_and_validate(self, request: web.Request):
        """Parse + validate a funding request body.

        Returns ``(dest, amount, normalized_dest)`` on success, or a
        ``web.Response`` (400) to return directly. Shared by /request and /sign so
        the two endpoints can't drift. `normalized_dest` is rejected before any
        chain work so garbage can't key the rate-limit table or burn an RPC.
        """
        try:
            body = await request.json()
        except ValueError:
            return web.json_response({"error": "invalid json"}, status=400)

        dest = body.get("dest")
        if not isinstance(dest, str) or not dest:
            return web.json_response(
                {"error": "missing or invalid 'dest' (ss58 or 0x-hex AccountId)"},
                status=400,
            )

        amount = body.get("amount", DEFAULT_AMOUNT_PLANCKS)
        if not isinstance(amount, int) or isinstance(amount, bool) or amount <= 0:
            return web.json_response(
                {"error": "missing or invalid 'amount' (positive integer plancks)"},
                status=400,
            )

        normalized_dest = _normalize_dest(dest)
        if normalized_dest is None:
            return web.json_response(
                {"error": "invalid 'dest': not a valid SS58 address or 0x-hex AccountId"},
                status=400,
            )
        return dest, amount, normalized_dest

    async def _handle_faucet(self, request: web.Request) -> web.Response:
        parsed = await self._parse_and_validate(request)
        if isinstance(parsed, web.Response):
            return parsed
        dest, amount, normalized_dest = parsed

        gate = await self._check_gate(normalized_dest)
        if gate is not None:
            return gate

        logger.info(
            "funding %s with %d plancks from %s (mode=%s)",
            normalized_dest,
            amount,
            self._funder_address,
            "hybrid" if self._is_hybrid else "sr25519",
        )
        try:
            try:
                receipt = await self._run_chain(
                    lambda: self._submit_transfer(dest=dest, amount=amount)
                )
            except Exception:  # noqa: BLE001 — stable code; internals go to logs
                # Don't commit the lenient slot — the transfer never happened, so
                # retries should be allowed immediately. Don't echo `str(exc)`
                # either; it leaks substrate-interface internals without telling the
                # caller anything actionable. Operators diagnose via the logs.
                logger.exception(
                    "faucet transfer failed: dest=%s amount=%d", dest, amount
                )
                return web.json_response(
                    {"error": "transfer failed; see faucet logs"}, status=502
                )
            # Commit the lenient slot only after the transfer succeeded.
            await self._commit_lenient_slot(normalized_dest)
            return web.json_response(
                {
                    "extrinsic_hash": receipt.get("extrinsic_hash", ""),
                    "block_hash": receipt.get("block_hash"),
                    "amount": amount,
                    "dest": dest,
                }
            )
        finally:
            await self._discard_in_flight(normalized_dest)

    def _pool_retry_hint(self) -> float:
        """Seconds until a pool account is likely eligible again."""
        if not self._pool:
            return round(self.config.pool_replenish_interval_seconds, 1)
        now = time.monotonic()
        remaining = [
            self.config.pool_cooldown_seconds - (now - p.last_handed_out)
            for p in self._pool
        ]
        positive = [r for r in remaining if r > 0]
        if positive:
            return round(min(positive), 1)
        # Nothing is in cooldown -> they're balance-starved; next refill tick.
        return round(self.config.pool_replenish_interval_seconds, 1)

    async def _handle_sign(self, request: web.Request) -> web.Response:
        parsed = await self._parse_and_validate(request)
        if isinstance(parsed, web.Response):
            return parsed
        dest, amount, normalized_dest = parsed

        gate = await self._check_gate(normalized_dest)
        if gate is not None:
            return gate

        try:
            pool = await self._allocate_pool_account(amount)
            if pool is None:
                return web.json_response(
                    {
                        "error": "faucet pool temporarily exhausted",
                        "retry_after_seconds": self._pool_retry_hint(),
                    },
                    status=503,
                )
            try:
                result = await self._run_chain(
                    lambda: self._build_sign_only(pool, dest, amount)
                )
            except Exception:  # noqa: BLE001 — stable code; internals go to logs
                async with self._pool_lock:
                    pool.in_flight = False  # release, no balance change
                logger.exception("faucet /sign failed: dest=%s amount=%d", dest, amount)
                return web.json_response(
                    {"error": "sign failed; see faucet logs"}, status=502
                )
            # Optimistic commit: handing out a broadcastable tx is equivalent to
            # funding, so charge the lenient slot and decrement the pool balance now.
            await self._commit_lenient_slot(normalized_dest)
            async with self._pool_lock:
                pool.last_handed_out = time.monotonic()
                pool.tracked_free -= amount
                pool.in_flight = False
                self._last_sign_activity = time.monotonic()
            return web.json_response(
                {
                    "signed_extrinsic": result["signed_extrinsic"],
                    "extrinsic_hash": result["extrinsic_hash"],
                    "nonce": result["nonce"],
                    "from": pool.ss58,
                    "amount": amount,
                    "dest": dest,
                    "mode": result["mode"],
                }
            )
        finally:
            await self._discard_in_flight(normalized_dest)

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _submit_transfer(self, *, dest: str, amount: int):
        # Idle websocket connections go stale; the first send surfaces as one of
        # `_TRANSPORT_ERRORS`. `_call_with_failover` reconnects (and, with multiple
        # nodes, fails over) and retries. Dispatch failures raised below as
        # RuntimeError are deterministic and are NOT retried.
        def _do():
            if self._is_hybrid:
                return self._submit_hybrid_transfer(dest=dest, amount=amount)
            return self._submit_sr25519_transfer(dest=dest, amount=amount)

        return self._call_with_failover(_do, "submit_transfer")

    def _submit_sr25519_transfer(self, *, dest: str, amount: int) -> dict:
        assert self._sr_keypair is not None  # sr25519 mode invariant
        call = _compose_sudo_mint_call(self._ifc, dest=dest, amount=amount)
        extrinsic = self._ifc.create_signed_extrinsic(
            call=call, keypair=self._sr_keypair
        )
        receipt = self._ifc.submit_extrinsic(
            extrinsic, wait_for_inclusion=True
        )
        ext_hash = str(getattr(receipt, "extrinsic_hash", "") or "")
        block_hash = str(getattr(receipt, "block_hash", "") or "")
        if not block_hash:
            raise RuntimeError(
                "sr25519 extrinsic missing block hash "
                f"(ext_hash={ext_hash})"
            )
        error = _fetch_extrinsic_dispatch_error(
            self._ifc, block_hash=block_hash, ext_hash=ext_hash
        )
        if error:
            raise RuntimeError(f"sr25519 extrinsic dispatch failed: {error}")
        return {"extrinsic_hash": ext_hash, "block_hash": block_hash}

    def _submit_hybrid_transfer(self, *, dest: str, amount: int) -> dict:
        """Build + submit a hybrid-signed faucet mint, wait for in-block.

        Returns a dict with `extrinsic_hash` and `block_hash`. Raises
        `RuntimeError` for any terminal pool status (`dropped` /
        `invalid` / `usurped` / `retracted` / `finalityTimeout`) so
        the HTTP caller gets a useful 502 instead of a silent hang.
        """
        assert self._hybrid_signer is not None  # hybrid mode invariant
        call = _compose_sudo_mint_call(self._ifc, dest=dest, amount=amount)
        ext_bytes, ext_hash = _build_hybrid_signed_extrinsic(
            iface=self._ifc,
            signer=self._hybrid_signer,
            call=call,
        )
        ext_hex = "0x" + ext_bytes.hex()

        def _result_handler(message, update_nr, subscription_id):  # noqa: ARG001
            params = message.get("params") or {}
            res = params.get("result")
            if isinstance(res, dict):
                lower = {k.lower(): v for k, v in res.items()}
                if "inblock" in lower:
                    self._ifc.rpc_request(
                        "author_unwatchExtrinsic", [subscription_id]
                    )
                    return {
                        "extrinsic_hash": ext_hash,
                        "block_hash": lower["inblock"],
                    }
                if "finalized" in lower:
                    self._ifc.rpc_request(
                        "author_unwatchExtrinsic", [subscription_id]
                    )
                    return {
                        "extrinsic_hash": ext_hash,
                        "block_hash": lower["finalized"],
                    }
            elif isinstance(res, str) and res.lower() in _HYBRID_TERMINAL_FAILURES:
                self._ifc.rpc_request(
                    "author_unwatchExtrinsic", [subscription_id]
                )
                raise RuntimeError(f"hybrid extrinsic rejected: {res}")
            return None  # non-terminal status — keep waiting

        response = self._ifc.rpc_request(
            "author_submitAndWatchExtrinsic",
            [ext_hex],
            result_handler=_result_handler,
        )
        # `rpc_request` returns whatever `_result_handler` returned. A None
        # response means the subscription closed before any terminal status
        # fired — without raising, the caller sees `block_hash=None` and
        # treats it as success. Fail fast instead.
        if not isinstance(response, dict) or not response.get("block_hash"):
            raise RuntimeError(
                "hybrid extrinsic subscription closed before inclusion "
                f"(ext_hash={ext_hash})"
            )
        block_hash = response["block_hash"]
        # author_submitAndWatchExtrinsic only reports inclusion — a
        # `System.ExtrinsicFailed` event in the same block still means the
        # chain rejected the dispatch. Fetch events at the block hash and
        # surface any matching failure.
        error = _fetch_extrinsic_dispatch_error(
            self._ifc, block_hash=block_hash, ext_hash=ext_hash
        )
        if error:
            raise RuntimeError(f"hybrid extrinsic dispatch failed: {error}")
        return {"extrinsic_hash": ext_hash, "block_hash": block_hash}

    # ------------------------------------------------------------------
    # Balance gate (Goals 3 & 4)
    # ------------------------------------------------------------------

    async def _run_chain(self, fn):
        """Run a blocking chain call in the executor while holding _chain_lock."""
        async with self._chain_lock:
            return await self._run(fn)

    def _query_free_balance(self, account_hex: str) -> int:
        """(executor thread) Free balance of `account_hex` in plancks, 0 if absent.

        Caller holds _chain_lock (via _run_chain). Works for sr25519 and hybrid
        alike: both key System.Account by AccountId32. `account_hex` is the
        "0x"+64-hex form, accepted directly by substrate-interface's AccountId
        encoder (no SS58 round-trip needed).
        """

        def _do():
            result = self._ifc.query("System", "Account", [account_hex])
            if result is None or result.value is None:
                return 0
            return int(result.value["data"]["free"])

        return self._call_with_failover(_do, "query_balance")

    async def _commit_lenient_slot(self, normalized_dest: str) -> None:
        async with self._rate_limit_lock:
            self._last_funded[normalized_dest] = time.monotonic()

    async def _discard_in_flight(self, normalized_dest: str) -> None:
        async with self._rate_limit_lock:
            self._in_flight.discard(normalized_dest)

    async def _check_gate(self, normalized_dest: str) -> web.Response | None:
        """Shared anti-abuse gate for /request and /sign.

        Returns an error response to reject, or None to allow — in which case the
        dest is left RESERVED in _in_flight and the caller's `finally` MUST discard
        it. Denies already-funded accounts (Goal 4); applies a lenient window to
        confirmed-empty accounts (Goal 3); on a balance-query failure, falls back to
        the strict window (fail-open) or denies with 503 (fail-closed).
        """
        now = time.monotonic()
        async with self._rate_limit_lock:
            last = self._last_funded.get(normalized_dest, 0.0)
            wait = self.config.lenient_rate_limit_seconds - (now - last)
            if wait > 0:
                return web.json_response(
                    {"error": "rate limited", "retry_after_seconds": round(wait, 1)},
                    status=429,
                )
            if normalized_dest in self._in_flight:
                return web.json_response(
                    {"error": "request already in flight for this dest"},
                    status=429,
                )
            self._in_flight.add(normalized_dest)  # reserve

        try:
            free = await self._run_chain(
                lambda: self._query_free_balance(normalized_dest)
            )
        except Exception:  # noqa: BLE001 — balance read failed; diagnose via logs
            logger.exception("balance query failed for %s", normalized_dest)
            if not self.config.balance_query_fail_open:
                await self._discard_in_flight(normalized_dest)
                return web.json_response(
                    {"error": "balance check unavailable"}, status=503
                )
            # fail-open: fall back to the strict window and proceed.
            async with self._rate_limit_lock:
                last = self._last_funded.get(normalized_dest, 0.0)
                wait = self.config.rate_limit_seconds - (now - last)
                if wait > 0:
                    self._in_flight.discard(normalized_dest)
                    return web.json_response(
                        {
                            "error": "rate limited (degraded: balance check unavailable)",
                            "retry_after_seconds": round(wait, 1),
                        },
                        status=429,
                    )
            return None  # allowed, still reserved

        if free > self.config.max_funded_balance_plancks:
            await self._discard_in_flight(normalized_dest)
            return web.json_response(
                {
                    "error": "destination already funded",
                    "free_balance_plancks": free,
                    "threshold_plancks": self.config.max_funded_balance_plancks,
                    "dest": normalized_dest,
                },
                status=403,
            )
        return None  # allowed, reserved

    def _open_iface(self, idx: int) -> None:
        """(executor thread) Close the current iface and open node_urls[idx]."""
        old = self._iface
        if old is not None:
            with contextlib.suppress(Exception):  # best-effort tear-down
                old.close()
        self._iface = SubstrateInterface(url=self.config.node_urls[idx])
        self._active_idx = idx

    async def _connect_first_reachable(self) -> None:
        """Connect to the first node that is reachable AND a valid dev chain."""
        last_exc: Exception | None = None
        for idx in range(len(self.config.node_urls)):
            url = self.config.node_urls[idx]
            try:
                await self._run(lambda i=idx: self._open_iface(i))
                await self._verify_dev_chain()
                logger.info("faucet connected to node[%d]: %s", idx, url)
                return
            except Exception as exc:  # noqa: BLE001 — skip unreachable/non-dev nodes
                last_exc = exc
                logger.warning(
                    "node[%d] %s unusable at startup (%s: %s); trying next",
                    idx,
                    url,
                    type(exc).__name__,
                    exc,
                )
        raise RuntimeError(
            f"no usable substrate node among {self.config.node_urls!r}; "
            f"last error: {last_exc}"
        )

    def _call_with_failover(self, fn, op_name: str):
        """(executor thread) Run fn() with sticky multi-node failover.

        Caller MUST hold ``self._chain_lock`` — this mutates ``self._iface`` /
        ``self._active_idx`` and calls non-thread-safe ``SubstrateInterface``
        methods. On a transport error, advance to the next node (wrapping),
        reconnect, re-verify the dev chain, and retry. ``attempts =
        max(len(node_urls), 2)`` preserves the single-node reconnect-and-retry-once
        stale-socket behavior. Deterministic failures (RuntimeError, dispatch
        errors) and timeouts are NOT failed over — they re-raise immediately.
        """
        n = len(self.config.node_urls)
        attempts = max(n, 2)
        last_exc: Exception | None = None
        for attempt in range(1, attempts + 1):
            try:
                return fn()
            except _TRANSPORT_ERRORS as exc:
                # TimeoutError subclasses OSError (Py3.10+), so it lands here even
                # though it's not in _TRANSPORT_ERRORS. Re-raise it explicitly: a
                # timeout may mean the extrinsic already reached a pool, and failing
                # over would risk a double-fund.
                if isinstance(exc, TimeoutError):
                    raise
                last_exc = exc
                if attempt == attempts:
                    raise
                next_idx = (self._active_idx + 1) % n
                logger.warning(
                    "%s: transport error on node[%d] %s (%s: %s); "
                    "failing over to node[%d]",
                    op_name,
                    self._active_idx,
                    self.config.node_urls[self._active_idx],
                    type(exc).__name__,
                    exc,
                    next_idx,
                )
                self._open_iface(next_idx)
                self._verify_dev_chain_sync()
        raise RuntimeError(f"unreachable: {op_name} failover exhausted") from last_exc

    def _verify_dev_chain_sync(self) -> None:
        """In-thread dev-chain check used on failover reconnect (no executor)."""
        if self.config.allow_any_chain or os.environ.get(
            "QUIP_FAUCET_ALLOW_ANY_CHAIN"
        ) == "1":
            return
        chain_name = self._ifc.chain
        if not any(chain_name.startswith(p) for p in DEV_CHAIN_PREFIXES):
            raise RuntimeError(
                f"refusing to use failover node reporting chain {chain_name!r}; "
                "not a known dev chain"
            )

    async def _verify_dev_chain(self) -> None:
        if self.config.allow_any_chain or os.environ.get(
            "QUIP_FAUCET_ALLOW_ANY_CHAIN"
        ) == "1":
            logger.warning(
                "faucet running against non-dev chain because allow_any_chain=true; "
                "this is unsafe outside controlled environments"
            )
            return

        chain_name = await self._run(lambda: self._ifc.chain)
        if not any(chain_name.startswith(p) for p in DEV_CHAIN_PREFIXES):
            raise RuntimeError(
                f"refusing to run faucet against chain {chain_name!r}; pass "
                "--allow-any-chain only if you really mean to fund accounts "
                "on a non-dev chain"
            )
        logger.info("faucet verified dev chain: %s", chain_name)

    async def _run(self, fn):
        loop = self._loop or asyncio.get_running_loop()
        return await loop.run_in_executor(None, fn)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _normalize_dest(dest: str) -> str | None:
    """Canonicalize dest as 0x-prefixed lowercase hex for rate-limit keys.

    Accepts both `0x...` hex and SS58-encoded addresses; both decode to the
    same 32-byte AccountId, so they share one rate-limit slot. Without SS58
    canonicalization a caller could alternate representations to bypass the
    per-destination throttle.

    Returns `None` for malformed inputs so callers can reject the request
    before keying the rate-limit table on garbage. The previous behaviour
    (pass-through on failure) gave every distinct malformed string its own
    rate-limit slot — trivial throttle bypass.
    """
    if dest.startswith("0x") or dest.startswith("0X"):
        hex_body = dest[2:]
        # 32-byte AccountId32 = 64 hex chars. Reject anything off-shape.
        if len(hex_body) != 64:
            return None
        try:
            bytes.fromhex(hex_body)
        except ValueError:
            return None
        return "0x" + hex_body.lower()
    try:
        pubkey_hex = ss58_decode(dest)
    except (ValueError, IndexError, TypeError):
        # Malformed SS58 raises one of these depending on the failure mode.
        return None
    return "0x" + pubkey_hex.lower()


def _is_port_free(host: str, port: int) -> bool:
    try:
        with socket.socket() as s:
            s.bind((host, port))
            return True
    except OSError:
        return False


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="faucet_bot",
        description="Standalone dev faucet for substrate chains.",
    )
    parser.add_argument(
        "--node-url",
        dest="node_urls",
        action="append",
        metavar="URL",
        required=True,
        help="Substrate node WebSocket URL (e.g. ws://localhost:9944). Repeat to "
        "add ordered fallback nodes tried on connection failure.",
    )
    parser.add_argument(
        "--faucet-key",
        default="//Alice",
        help="Substrate URI for the funded sender account (default: //Alice). "
        "On hybrid chains must be one of //Alice, //Bob, //Alice//stash.",
    )
    parser.add_argument(
        "--listen",
        default="127.0.0.1",
        help="Bind address (default: 127.0.0.1)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=8087,
        help="Bind port (default: 8087)",
    )
    parser.add_argument(
        "--rate-limit-seconds",
        type=float,
        default=float(DEFAULT_RATE_LIMIT_SECONDS),
        help="Strict fallback window per destination (used when the balance "
        "query can't run). Default: %(default)s",
    )
    parser.add_argument(
        "--lenient-rate-limit-seconds",
        type=float,
        default=float(DEFAULT_LENIENT_RATE_LIMIT_SECONDS),
        help="Lenient window for confirmed zero-balance destinations; set >= "
        "block time. Default: %(default)s",
    )
    parser.add_argument(
        "--max-funded-balance-plancks",
        type=int,
        default=0,
        help="Deny requests when the destination's free balance exceeds this "
        "(default 0 => deny if it holds any funds)",
    )
    parser.add_argument(
        "--balance-query-fail-open",
        action="store_true",
        default=True,
        help="On a balance-query failure, fall back to the strict window and "
        "proceed (default). Use --balance-query-fail-closed to deny instead.",
    )
    parser.add_argument(
        "--balance-query-fail-closed",
        dest="balance_query_fail_open",
        action="store_false",
        help="On a balance-query failure, deny with 503 (hard no-double-fund).",
    )
    parser.add_argument(
        "--pool-size",
        type=int,
        default=DEFAULT_POOL_SIZE,
        help="Number of pre-funded pool accounts backing /sign. Default: %(default)s",
    )
    parser.add_argument(
        "--pool-max-size",
        type=int,
        default=DEFAULT_POOL_MAX_SIZE,
        help="Cap on adaptive pool growth (doubling on nonce reuse). "
        "Default: %(default)s",
    )
    parser.add_argument(
        "--pool-fund-amount",
        type=int,
        default=DEFAULT_POOL_FUND_AMOUNT,
        help="Plancks minted to each pool account when funding/replenishing. "
        "Default: %(default)s",
    )
    parser.add_argument(
        "--pool-low-watermark",
        type=int,
        default=DEFAULT_POOL_LOW_WATERMARK,
        help="Refill a pool account when its tracked balance drops below this. "
        "Default: %(default)s",
    )
    parser.add_argument(
        "--pool-cooldown-seconds",
        type=float,
        default=DEFAULT_POOL_COOLDOWN_SECONDS,
        help="Per-account reuse cooldown; set >= block time. Default: %(default)s",
    )
    parser.add_argument(
        "--pool-replenish-interval",
        dest="pool_replenish_interval_seconds",
        type=float,
        default=DEFAULT_POOL_REPLENISH_INTERVAL_SECONDS,
        help="Seconds between pool reconcile/refill/grow cycles. Default: %(default)s",
    )
    parser.add_argument(
        "--pool-idle-grow-seconds",
        type=float,
        default=DEFAULT_POOL_IDLE_GROW_SECONDS,
        help="Idle period (no /sign activity) required before doubling the pool. "
        "Default: %(default)s",
    )
    parser.add_argument(
        "--allow-any-chain",
        action="store_true",
        help="Allow running against non-dev chains. UNSAFE.",
    )
    parser.add_argument(
        "--log-level",
        default="INFO",
        choices=("DEBUG", "INFO", "WARNING", "ERROR"),
        help="Log verbosity (default: INFO)",
    )
    return parser


def _setup_logging(level: str) -> None:
    logging.basicConfig(
        level=level,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        stream=sys.stderr,
    )


async def _run(config: FaucetConfig) -> None:
    faucet = SubstrateFaucet(config)
    await faucet.start()
    try:
        # Block forever; aiohttp's TCPSite has already bound the port and
        # the runner serves requests in the background.
        await asyncio.Event().wait()
    finally:
        await faucet.stop()


def main(argv: list | None = None) -> int:
    args = _build_parser().parse_args(argv)
    _setup_logging(args.log_level)

    if not _is_port_free(args.listen, args.port):
        logger.error(
            "port %s:%d already in use; pick a different --port",
            args.listen,
            args.port,
        )
        return 2

    config = FaucetConfig(
        node_urls=args.node_urls,
        faucet_key_uri=args.faucet_key,
        listen_host=args.listen,
        listen_port=args.port,
        rate_limit_seconds=args.rate_limit_seconds,
        lenient_rate_limit_seconds=args.lenient_rate_limit_seconds,
        max_funded_balance_plancks=args.max_funded_balance_plancks,
        balance_query_fail_open=args.balance_query_fail_open,
        allow_any_chain=args.allow_any_chain,
        pool_size=args.pool_size,
        pool_max_size=args.pool_max_size,
        pool_fund_amount=args.pool_fund_amount,
        pool_low_watermark=args.pool_low_watermark,
        pool_cooldown_seconds=args.pool_cooldown_seconds,
        pool_replenish_interval_seconds=args.pool_replenish_interval_seconds,
        pool_idle_grow_seconds=args.pool_idle_grow_seconds,
    )
    try:
        asyncio.run(_run(config))
    except KeyboardInterrupt:
        logger.info("faucet stopped")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
