"""Balance-gated rate limiting (`_check_gate`): deny funded, lightly throttle empty."""
from __future__ import annotations

import asyncio
import json
import time

from faucet_bot import FaucetConfig, SubstrateFaucet


def _faucet(**cfg):
    cfg.setdefault("node_urls", ["ws://a"])
    return SubstrateFaucet(FaucetConfig(**cfg))


def _resp(resp):
    return resp.status, json.loads(resp.body)


def _set_balance(faucet, value):
    calls = []

    def q(dest):
        calls.append(dest)
        return value

    faucet._query_free_balance = q  # type: ignore[assignment]
    faucet._balance_calls = calls  # type: ignore[attr-defined]


def _raise_balance(faucet):
    def q(dest):
        raise ConnectionError("node down")

    faucet._query_free_balance = q  # type: ignore[assignment]


DEST = "0x" + "11" * 32


def test_gate_denies_funded_dest():
    f = _faucet(max_funded_balance_plancks=0)
    _set_balance(f, 5_000)
    resp = asyncio.run(f._check_gate(DEST))
    status, body = _resp(resp)
    assert status == 403
    assert body["free_balance_plancks"] == 5_000
    assert DEST not in f._in_flight  # reservation released on deny


def test_gate_allows_empty_dest_and_keeps_reservation():
    f = _faucet(max_funded_balance_plancks=0)
    _set_balance(f, 0)
    resp = asyncio.run(f._check_gate(DEST))
    assert resp is None  # allowed
    assert DEST in f._in_flight  # stays reserved for the handler's finally


def test_gate_lenient_window_blocks_second_without_querying():
    f = _faucet(lenient_rate_limit_seconds=30)
    _set_balance(f, 0)
    f._last_funded[DEST] = time.monotonic()  # just funded
    resp = asyncio.run(f._check_gate(DEST))
    status, body = _resp(resp)
    assert status == 429
    assert f._balance_calls == []  # cheap reject, no chain round-trip


def test_gate_in_flight_blocks_concurrent_request():
    f = _faucet(lenient_rate_limit_seconds=0)
    _set_balance(f, 0)
    f._in_flight.add(DEST)  # another request holds it
    resp = asyncio.run(f._check_gate(DEST))
    status, _ = _resp(resp)
    assert status == 429


def test_gate_threshold_boundary_is_inclusive_allow():
    f = _faucet(max_funded_balance_plancks=100)
    _set_balance(f, 100)  # exactly at threshold -> allowed (not strictly greater)
    assert asyncio.run(f._check_gate(DEST)) is None


def test_gate_threshold_boundary_one_over_denies():
    f = _faucet(max_funded_balance_plancks=100)
    _set_balance(f, 101)
    status, _ = _resp(asyncio.run(f._check_gate(DEST)))
    assert status == 403


def test_gate_query_failure_fail_open_allows_when_not_recently_funded():
    f = _faucet(balance_query_fail_open=True, rate_limit_seconds=60)
    _raise_balance(f)
    assert asyncio.run(f._check_gate(DEST)) is None  # degraded-allow


def test_gate_query_failure_fail_open_strict_fallback_throttles():
    f = _faucet(balance_query_fail_open=True, rate_limit_seconds=60, lenient_rate_limit_seconds=0)
    _raise_balance(f)
    f._last_funded[DEST] = time.monotonic()  # within strict 60s window
    status, body = _resp(asyncio.run(f._check_gate(DEST)))
    assert status == 429
    assert "degraded" in body["error"]


def test_gate_query_failure_fail_closed_denies():
    f = _faucet(balance_query_fail_open=False)
    _raise_balance(f)
    status, _ = _resp(asyncio.run(f._check_gate(DEST)))
    assert status == 503
    assert DEST not in f._in_flight
