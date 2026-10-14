//! cargo run --locked --example export_metadata -- metadata/runtime-119.scale
use anyhow::{Context, Result};
use quip_protocol_runtime::{Runtime, RuntimeGenesisConfig};
use sp_runtime::BuildStorage;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("provide an output .scale path")?;
    let storage = RuntimeGenesisConfig::default()
        .build_storage()
        .map_err(anyhow::Error::msg)?;
    let metadata = sp_io::TestExternalities::new(storage).execute_with(|| {
        Runtime::metadata_at_version(16)
            .expect("pinned runtime supports metadata V16")
            .to_vec()
    });
    std::fs::write(path, &metadata)?;
    println!(
        "metadata V16 blake2_256: 0x{}",
        hex::encode(sp_core::hashing::blake2_256(&metadata))
    );
    Ok(())
}
