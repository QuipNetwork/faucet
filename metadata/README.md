# Runtime schema

`runtime-119.scale` is SCALE metadata V16 exported from the quip-validator
v0.4.0 release runtime at `bc4552a2cc6913bd057ecfdf9abc99197393b55c` (spec 119,
transaction 7). SDK revision: `845eb9986c7131056b077d14ad7249f9d65ca111`.

BLAKE2-256: `888454dc7d0fe5ac194e0dac0b72ad2cdb9c93f1e3ce1af6d79323dad1d169c3`.

Regenerate with:

```sh
cargo run --locked --example export_metadata -- metadata/runtime-119.scale
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
