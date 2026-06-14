"""`_handle_faucet` wiring: gate runs before submit, slot commit + reservation release."""
from __future__ import annotations

import asyncio

from tests._helpers import DEST, FakeRequest, make_faucet, resp_json, set_balance


def test_denied_by_gate_does_not_submit():
    f = make_faucet(max_funded_balance_plancks=0)
    set_balance(f, 5_000)  # already funded -> gate denies
    submitted = []
    f._submit_transfer = lambda **kw: submitted.append(kw)
    status, _ = resp_json(asyncio.run(f._handle_faucet(FakeRequest({"dest": DEST}))))
    assert status == 403
    assert submitted == []  # never reached the chain


def test_success_commits_slot_and_releases_inflight():
    f = make_faucet()
    set_balance(f, 0)  # empty -> allowed
    f._submit_transfer = lambda **kw: {"extrinsic_hash": "0xabc", "block_hash": "0xblk"}
    req = FakeRequest({"dest": DEST, "amount": 1000})
    status, body = resp_json(asyncio.run(f._handle_faucet(req)))
    assert status == 200
    assert body["extrinsic_hash"] == "0xabc"
    assert body["dest"] == DEST
    assert DEST in f._last_funded  # committed
    assert DEST not in f._in_flight  # released


def test_submit_failure_releases_inflight_without_committing():
    f = make_faucet()
    set_balance(f, 0)

    def boom(**kw):
        raise RuntimeError("dispatch failed")

    f._submit_transfer = boom
    status, _ = resp_json(asyncio.run(f._handle_faucet(FakeRequest({"dest": DEST}))))
    assert status == 502
    assert DEST not in f._last_funded  # slot NOT burned on failure
    assert DEST not in f._in_flight  # released


def test_invalid_json_400():
    f = make_faucet()
    status, _ = resp_json(asyncio.run(f._handle_faucet(FakeRequest(None, raise_json=True))))
    assert status == 400


def test_missing_dest_400():
    f = make_faucet()
    status, _ = resp_json(asyncio.run(f._handle_faucet(FakeRequest({}))))
    assert status == 400


def test_bad_amount_400():
    f = make_faucet()
    status, _ = resp_json(asyncio.run(f._handle_faucet(FakeRequest({"dest": DEST, "amount": -5}))))
    assert status == 400


def test_garbage_dest_400():
    f = make_faucet()
    status, _ = resp_json(asyncio.run(f._handle_faucet(FakeRequest({"dest": "garbage"}))))
    assert status == 400
