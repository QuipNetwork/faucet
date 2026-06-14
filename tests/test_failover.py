"""Multi-node failover loop (`_call_with_failover`) and its safety boundaries."""
from __future__ import annotations

import pytest

import faucet_bot
from faucet_bot import FaucetConfig, SubstrateFaucet


def _faucet(urls):
    """Construct a faucet without any chain I/O, with failover seams stubbed.

    `_open_iface` and `_verify_dev_chain_sync` are replaced with recorders so the
    failover logic can be exercised without a node.
    """
    f = SubstrateFaucet(FaucetConfig(node_urls=list(urls)))
    f.opened = []  # type: ignore[attr-defined]

    def fake_open(idx):
        f.opened.append(idx)
        f._active_idx = idx

    f._open_iface = fake_open  # type: ignore[assignment]
    f._verify_dev_chain_sync = lambda: None  # type: ignore[assignment]
    return f


def _raises_then(exc, times, result="ok"):
    """A callable that raises `exc` the first `times` calls, then returns result."""
    state = {"n": 0}

    def fn():
        if state["n"] < times:
            state["n"] += 1
            raise exc
        return result

    return fn


def test_timeout_is_not_a_failover_trigger():
    assert TimeoutError not in faucet_bot._TRANSPORT_ERRORS


def test_single_node_reconnects_and_retries_once():
    f = _faucet(["ws://a"])
    result = f._call_with_failover(_raises_then(ConnectionError(), 1), "op")
    assert result == "ok"
    assert f.opened == [0]  # reconnected to the same single node
    assert f._active_idx == 0


def test_single_node_exhausts_after_two_attempts():
    f = _faucet(["ws://a"])
    with pytest.raises(OSError):
        f._call_with_failover(_raises_then(OSError(), 2), "op")


def test_two_node_failover_advances_to_next_node():
    f = _faucet(["ws://a", "ws://b"])
    result = f._call_with_failover(_raises_then(ConnectionError(), 1), "op")
    assert result == "ok"
    assert f.opened == [1]
    assert f._active_idx == 1


def test_failover_is_sticky_across_calls():
    f = _faucet(["ws://a", "ws://b"])
    f._call_with_failover(_raises_then(ConnectionError(), 1), "op")  # -> idx 1
    f.opened.clear()
    result = f._call_with_failover(lambda: "ok2", "op")  # succeeds immediately
    assert result == "ok2"
    assert f.opened == []  # no reconnect
    assert f._active_idx == 1  # stayed on the healthy node


def test_failover_wraps_around():
    f = _faucet(["ws://a", "ws://b", "ws://c"])
    f._active_idx = 2
    f._call_with_failover(_raises_then(ConnectionError(), 1), "op")
    assert f.opened == [0]  # (2 + 1) % 3 == 0
    assert f._active_idx == 0


def test_all_nodes_down_raises_after_trying_each():
    f = _faucet(["ws://a", "ws://b"])
    with pytest.raises(EOFError):
        f._call_with_failover(_raises_then(EOFError(), 5), "op")
    assert f.opened == [1]  # 2 attempts: one failover before the final re-raise


def test_dispatch_failure_is_not_failed_over():
    f = _faucet(["ws://a", "ws://b"])
    with pytest.raises(RuntimeError):
        f._call_with_failover(_raises_then(RuntimeError("dispatch"), 1), "op")
    assert f.opened == []  # deterministic failure -> never reconnects


def test_timeout_is_not_failed_over():
    f = _faucet(["ws://a", "ws://b"])
    with pytest.raises(TimeoutError):
        f._call_with_failover(_raises_then(TimeoutError(), 1), "op")
    assert f.opened == []
