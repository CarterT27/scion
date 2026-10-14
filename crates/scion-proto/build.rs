//! Generates the `scion.v1` gRPC code and a file descriptor set from
//! `proto/scion/v1/scion.proto` (PLAN 2.13). Requires `protoc` on PATH.

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(env::var("OUT_DIR")?);
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest.join("../../proto");
    let proto_file = proto_root.join("scion/v1/scion.proto");
    println!("cargo:rerun-if-changed={}", proto_file.display());
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .file_descriptor_set_path(out.join("scion_descriptor.bin"))
        .compile_protos(&[proto_file], &[proto_root])?;
    Ok(())
}
