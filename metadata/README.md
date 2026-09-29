# Runtime schema

`runtime-119.scale` is SCALE metadata V16 exported from the approved Phase 2
runtime at `bea1395381cff989a66bbfd4e4ca06b3c09f937a` (spec 119, transaction 7).
SDK revision: `845eb9986c7131056b077d14ad7249f9d65ca111`.

BLAKE2-256: `bc4bd2fc7d3cb06cf6d23c968cb4b27e5fe98abdeb3c74e729bd2436a5e5afbe`.

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
