"""Config + CLI parsing behavior (multi-node, balance gate, pool flags)."""
from __future__ import annotations

import pytest

from faucet_bot import DEFAULT_AMOUNT_PLANCKS, FaucetConfig, _build_parser


def test_single_node_url_becomes_one_element_list():
    args = _build_parser().parse_args(["--node-url", "ws://a"])
    assert args.node_urls == ["ws://a"]


def test_repeated_node_url_preserves_order():
    args = _build_parser().parse_args(
        ["--node-url", "ws://a", "--node-url", "ws://b", "--node-url", "ws://c"]
    )
    assert args.node_urls == ["ws://a", "ws://b", "ws://c"]


def test_missing_node_url_is_an_error():
    with pytest.raises(SystemExit):
        _build_parser().parse_args([])


def test_config_rejects_empty_node_urls():
    with pytest.raises(ValueError):
        FaucetConfig(node_urls=[])


def test_balance_gate_flag_defaults():
    args = _build_parser().parse_args(["--node-url", "ws://a"])
    assert args.lenient_rate_limit_seconds == 5.0
    assert args.max_funded_balance_plancks == 0
    assert args.balance_query_fail_open is True


def test_balance_query_fail_closed_flips_default():
    args = _build_parser().parse_args(["--node-url", "ws://a", "--balance-query-fail-closed"])
    assert args.balance_query_fail_open is False


def test_pool_flag_defaults():
    args = _build_parser().parse_args(["--node-url", "ws://a"])
    assert args.pool_size == 8
    assert args.pool_max_size == 64
    assert args.pool_fund_amount == 100 * DEFAULT_AMOUNT_PLANCKS
    assert args.pool_low_watermark == 10 * DEFAULT_AMOUNT_PLANCKS
    assert args.pool_cooldown_seconds == 20.0
    assert args.pool_replenish_interval_seconds == 30.0
    assert args.pool_idle_grow_seconds == 30.0


def test_config_defaults_match_parser():
    cfg = FaucetConfig(node_urls=["ws://a"])
    assert cfg.rate_limit_seconds == 60.0
    assert cfg.lenient_rate_limit_seconds == 5.0
    assert cfg.max_funded_balance_plancks == 0
    assert cfg.balance_query_fail_open is True
    assert cfg.pool_size == 8
    assert cfg.pool_max_size == 64
    assert cfg.pool_fund_amount == 100 * DEFAULT_AMOUNT_PLANCKS
