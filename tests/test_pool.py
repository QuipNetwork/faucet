"""Pre-funded pool backing /sign: deterministic derivation, allocation, build."""
from __future__ import annotations

import asyncio
import hashlib
import time

from tests._helpers import DEST, make_faucet


def _pooled(n=3, fund=10_000, ed=0, cooldown=20.0):
    f = make_faucet(pool_cooldown_seconds=cooldown)
    f._is_hybrid = False
    f._funder_secret = b"\x01" * 32
    f._existential_deposit = ed
    f._pool = [f._derive_pool_account(i) for i in range(n)]
    for p in f._pool:
        p.tracked_free = fund
    return f


def _sr_faucet(secret=b"\x01" * 32):
    f = make_faucet()
    f._is_hybrid = False
    f._funder_secret = secret
    return f


class _FakeScaleBytes:
    def __init__(self, nonce):
        self._nonce = nonce

    def to_hex(self):
        return "0x" + (b"\xab" + self._nonce.to_bytes(4, "big")).hex()


class _FakeExtrinsic:
    def __init__(self, nonce):
        self.data = _FakeScaleBytes(nonce)


class _FakeIface:
    """Minimal sr25519 signing stand-in for SubstrateInterface."""

    def __init__(self, nonce):
        self._nonce = nonce

    def compose_call(self, **kw):
        return {"call": kw}

    def get_account_nonce(self, account):
        return self._nonce

    def create_signed_extrinsic(self, call, keypair, nonce):
        return _FakeExtrinsic(nonce)


def test_derivation_is_deterministic_across_instances():
    a = _sr_faucet()._derive_pool_account(3)
    b = _sr_faucet()._derive_pool_account(3)
    assert a.account_id_hex == b.account_id_hex
    assert a.account_id_hex.startswith("0x")
    assert len(a.account_id_hex) == 66  # 0x + 32-byte hex


def test_derivation_varies_by_index():
    f = _sr_faucet()
    assert f._derive_pool_account(0).account_id_hex != f._derive_pool_account(1).account_id_hex


def test_derivation_varies_by_funder_secret():
    a = _sr_faucet(b"\x01" * 32)._derive_pool_account(0)
    b = _sr_faucet(b"\x02" * 32)._derive_pool_account(0)
    assert a.account_id_hex != b.account_id_hex


def test_derivation_sr25519_populates_keypair_not_signer():
    acct = _sr_faucet()._derive_pool_account(0)
    assert acct.keypair is not None
    assert acct.signer is None
    assert acct.ss58


def test_derivation_hybrid_populates_signer_not_keypair():
    f = make_faucet()
    f._is_hybrid = True
    f._funder_secret = b"\x09" * 32
    acct = f._derive_pool_account(2)
    assert acct.signer is not None
    assert acct.keypair is None
    assert len(acct.account_id_hex) == 66


def test_build_sign_only_sr25519_returns_hex_hash_nonce():
    f = _sr_faucet()
    f._iface = _FakeIface(nonce=5)
    pool = f._derive_pool_account(0)
    result = f._build_sign_only(pool, DEST, 1000)
    assert result["nonce"] == 5
    assert result["mode"] == "sr25519"
    signed = result["signed_extrinsic"]
    assert signed.startswith("0x")
    expected_hash = "0x" + hashlib.blake2b(bytes.fromhex(signed[2:]), digest_size=32).hexdigest()
    assert result["extrinsic_hash"] == expected_hash


def test_build_sign_only_detects_nonce_reuse_and_grows():
    f = _sr_faucet()
    f._iface = _FakeIface(nonce=5)
    pool = f._derive_pool_account(0)
    f._build_sign_only(pool, DEST, 1000)  # first handout: nonce 5, no prior -> no grow
    assert f._grow_pending is False
    assert pool.last_handed_nonce == 5
    # chain nonce still 5 (prior handout not yet included) -> reuse -> grow
    f._build_sign_only(pool, DEST, 1000)
    assert f._grow_pending is True


def test_build_sign_only_no_grow_when_nonce_advances():
    f = _sr_faucet()
    f._iface = _FakeIface(nonce=5)
    pool = f._derive_pool_account(0)
    f._build_sign_only(pool, DEST, 1000)  # nonce 5
    f._iface._nonce = 6  # prior handout landed
    f._build_sign_only(pool, DEST, 1000)  # nonce 6 > 5 -> healthy
    assert f._grow_pending is False


# --- Allocation policy (the spec for _allocate_pool_account) ---------------


def test_allocate_returns_eligible_account_and_reserves_it():
    f = _pooled()
    acct = asyncio.run(f._allocate_pool_account(1000))
    assert acct is not None
    assert acct.in_flight is True  # reserved so a concurrent request can't reuse it


def test_allocate_rotates_across_calls():
    f = _pooled(n=3, cooldown=0.0)
    i1 = asyncio.run(f._allocate_pool_account(1000)).index
    for p in f._pool:
        p.in_flight = False  # release so eligibility is purely about rotation
    i2 = asyncio.run(f._allocate_pool_account(1000)).index
    assert i1 != i2  # round-robin advanced; didn't re-pick the same account


def test_allocate_skips_in_flight():
    f = _pooled(n=2)
    f._pool[0].in_flight = True
    acct = asyncio.run(f._allocate_pool_account(1000))
    assert acct.index == 1


def test_allocate_skips_low_balance():
    f = _pooled(n=2, fund=500)
    assert asyncio.run(f._allocate_pool_account(1000)) is None  # needs 1000 > 500


def test_allocate_respects_existential_deposit():
    f = _pooled(n=1, fund=1000, ed=1)
    assert asyncio.run(f._allocate_pool_account(1000)) is None  # needs amount + ED


def test_allocate_skips_accounts_in_cooldown():
    f = _pooled(n=1, cooldown=20.0)
    f._pool[0].last_handed_out = time.monotonic()  # just handed out
    assert asyncio.run(f._allocate_pool_account(1000)) is None


def test_allocate_returns_none_when_all_ineligible():
    f = _pooled(n=2)
    for p in f._pool:
        p.in_flight = True
    assert asyncio.run(f._allocate_pool_account(1000)) is None


# --- Adaptive growth + replenishment --------------------------------------

from tests._helpers import set_balance  # noqa: E402


def test_grow_doubles_pool_when_idle_and_pending():
    f = _pooled(n=2, cooldown=0.0)
    f._grow_pending = True
    f._last_sign_activity = 0.0  # long idle
    set_balance(f, 10**18)  # derived accounts already well-funded -> no minting
    asyncio.run(f._maybe_grow_pool())
    assert len(f._pool) == 4
    assert [p.index for p in f._pool] == [0, 1, 2, 3]  # contiguous indices
    assert f._grow_pending is False


def test_grow_skips_when_recently_active():
    f = _pooled(n=2)
    f._grow_pending = True
    f._last_sign_activity = time.monotonic()  # active now -> defer
    asyncio.run(f._maybe_grow_pool())
    assert len(f._pool) == 2
    assert f._grow_pending is True  # still latched for the next idle window


def test_grow_respects_max_size():
    f = _pooled(n=2)
    f.config.pool_max_size = 2
    f._grow_pending = True
    f._last_sign_activity = 0.0
    asyncio.run(f._maybe_grow_pool())
    assert len(f._pool) == 2
    assert f._grow_pending is False  # cleared; cannot grow past the cap


def test_replenish_refills_accounts_below_watermark():
    f = _pooled(n=1, fund=0)
    f.config.pool_low_watermark = 10_000
    set_balance(f, 0)  # reconcile -> 0 (below watermark)
    minted = []
    f._submit_transfer = lambda **kw: minted.append(kw) or {"extrinsic_hash": "0x"}
    asyncio.run(f._replenish_once())
    assert len(minted) == 1
    assert minted[0]["dest"] == f._pool[0].account_id_hex
