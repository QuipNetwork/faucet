"""Characterization tests for existing pure helpers in faucet_bot.

These lock current behavior of `_normalize_dest` before the surrounding code is
refactored; if a later change alters normalization, these fail loudly.
"""
from __future__ import annotations

import faucet_bot

# Well-known dev vector: //Alice (sr25519), SS58 format 42.
ALICE_SS58 = "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY"
ALICE_HEX = "0xd43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d"


def test_normalize_dest_accepts_lowercase_hex():
    assert faucet_bot._normalize_dest(ALICE_HEX) == ALICE_HEX


def test_normalize_dest_lowercases_uppercase_hex():
    assert faucet_bot._normalize_dest(ALICE_HEX.upper().replace("0X", "0x")) == ALICE_HEX


def test_normalize_dest_decodes_ss58_to_hex():
    assert faucet_bot._normalize_dest(ALICE_SS58) == ALICE_HEX


def test_normalize_dest_ss58_and_hex_share_one_key():
    # The whole point of normalization: both forms collapse to one rate-limit key.
    assert faucet_bot._normalize_dest(ALICE_SS58) == faucet_bot._normalize_dest(ALICE_HEX)


def test_normalize_dest_rejects_wrong_length_hex():
    assert faucet_bot._normalize_dest("0xdeadbeef") is None


def test_normalize_dest_rejects_non_hex_body():
    assert faucet_bot._normalize_dest("0x" + "z" * 64) is None


def test_normalize_dest_rejects_garbage_ss58():
    assert faucet_bot._normalize_dest("not-an-address") is None
