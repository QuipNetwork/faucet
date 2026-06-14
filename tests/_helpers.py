"""Shared test helpers: build a faucet with no chain I/O, fake requests."""
from __future__ import annotations

import json

from faucet_bot import FaucetConfig, SubstrateFaucet

DEST = "0x" + "11" * 32
DEST2 = "0x" + "22" * 32


def make_faucet(**cfg) -> SubstrateFaucet:
    cfg.setdefault("node_urls", ["ws://a"])
    return SubstrateFaucet(FaucetConfig(**cfg))


def resp_json(resp):
    return resp.status, json.loads(resp.body)


def set_balance(faucet, value):
    """Stub the on-chain balance query to return `value`, recording calls."""
    calls = []

    def q(dest):
        calls.append(dest)
        return value

    faucet._query_free_balance = q
    faucet._balance_calls = calls
    return calls


class FakeRequest:
    """Minimal stand-in for aiohttp.web.Request — only `.json()` is used."""

    def __init__(self, body, raise_json=False):
        self._body = body
        self._raise = raise_json

    async def json(self):
        if self._raise:
            raise ValueError("bad json")
        return self._body
