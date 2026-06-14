"""`_handle_sign`: gate -> allocate pool account -> sign (no broadcast) -> commit."""
from __future__ import annotations

import asyncio

from tests._helpers import DEST, FakeRequest, make_faucet, resp_json, set_balance


def _sign_faucet(balance=0):
    f = make_faucet()
    f._is_hybrid = False
    f._funder_secret = b"\x01" * 32
    set_balance(f, balance)
    return f


def _alloc_returning(pool):
    async def alloc(amount):
        if pool is not None:
            pool.in_flight = True
        return pool

    return alloc


def test_sign_denied_by_gate_when_funded_skips_allocation():
    f = _sign_faucet(balance=5_000)
    calls = []

    async def alloc(amount):
        calls.append(amount)
        return None

    f._allocate_pool_account = alloc
    status, _ = resp_json(asyncio.run(f._handle_sign(FakeRequest({"dest": DEST}))))
    assert status == 403
    assert calls == []  # gate ran before allocation


def test_sign_pool_exhausted_returns_503_and_releases_gate():
    f = _sign_faucet(balance=0)
    f._allocate_pool_account = _alloc_returning(None)
    status, body = resp_json(asyncio.run(f._handle_sign(FakeRequest({"dest": DEST}))))
    assert status == 503
    assert "retry_after_seconds" in body
    assert DEST not in f._in_flight


def test_sign_success_returns_signed_tx_and_commits():
    f = _sign_faucet(balance=0)
    pool = f._derive_pool_account(0)
    pool.tracked_free = 10_000
    f._allocate_pool_account = _alloc_returning(pool)
    f._build_sign_only = lambda p, dest, amount: {
        "signed_extrinsic": "0xdead",
        "extrinsic_hash": "0xhash",
        "nonce": 7,
        "mode": "sr25519",
    }
    req = FakeRequest({"dest": DEST, "amount": 1000})
    status, body = resp_json(asyncio.run(f._handle_sign(req)))
    assert status == 200
    assert body["signed_extrinsic"] == "0xdead"
    assert body["from"] == pool.ss58
    assert body["nonce"] == 7
    assert body["mode"] == "sr25519"
    assert pool.in_flight is False  # released
    assert pool.tracked_free == 9_000  # optimistic decrement
    assert DEST in f._last_funded  # optimistic lenient commit
    assert DEST not in f._in_flight  # gate reservation released


def test_sign_build_failure_releases_and_does_not_commit():
    f = _sign_faucet(balance=0)
    pool = f._derive_pool_account(0)
    pool.tracked_free = 10_000
    f._allocate_pool_account = _alloc_returning(pool)

    def boom(p, dest, amount):
        raise RuntimeError("sign boom")

    f._build_sign_only = boom
    status, _ = resp_json(asyncio.run(f._handle_sign(FakeRequest({"dest": DEST}))))
    assert status == 502
    assert pool.in_flight is False
    assert pool.tracked_free == 10_000  # not decremented on failure
    assert DEST not in f._last_funded
    assert DEST not in f._in_flight
