# Runtime schema

`runtime-121.scale` is SCALE metadata V16 exported from the quip-validator
v0.5.1 release runtime at `b23738ab13cf04ae451518f4dcb3ddaaef2909d2` (spec 121,
transaction 7). SDK: QuipNetwork/polkadot-sdk tag `v0.4.0` (stable2609-1).

BLAKE2-256: `544107a2bf420a32bace36442cc4478cf856ea265258a4457f8c65b710a94250`.

Regenerate with:

```sh
cargo run --locked --example export_metadata -- metadata/runtime-121.scale
```

The service consumes the pinned runtime's Rust types directly. This snapshot
records the corresponding schema for inspection; it is not a dynamic decoder.
CI re-exports and compares it. FaucetOps remains pallet 11, with mint/disable/
set_authority at call indices 0/1/2; the new runtime pallets occupy 19–24.

Initial export used the identical local runtime tree. Once dependencies became
available, the actual pinned package re-exported the same bytes successfully
(`cargo +1.95 run --offline --locked --example export_metadata`, 2026-09-29).
CI repeats the byte-for-byte comparison before merge.

Re-pinned from `bea1395` to the v0.4.0 release commit on 2026-10-14. The
snapshot changed with it: the mempool settlement and payee additions, and the
regenerated weight constants. Two exports from the release pin are
byte-identical.

Re-pinned to v0.5.0-rc2 on 2026-11-13 for the spec-120 relaunch. `sp_core::hashing` is
gone in this SDK, so the hash functions now come from `sp-crypto-hashing`.

Re-pinned to v0.5.1 on 2026-11-17 for runtime 121, enacted on aglais in place (it pauses new XQVM
program admission). The snapshot differs from the 120 one in one byte, `System.Version.spec_version`.
The faucet refuses any other spec, so it has to be rebuilt with every runtime upgrade.
