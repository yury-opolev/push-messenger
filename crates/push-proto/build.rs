//! Generates the protobuf types and the gRPC code with the protoc that
//! `protoc-bin-vendored` ships, so building needs no system protoc.

use std::io;

fn main() -> io::Result<()> {
    let protoc = protoc_bin_vendored::protoc_bin_path().map_err(io::Error::other)?;
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc);
    tonic_prost_build::configure().compile_with_config(
        config,
        &[
            "proto/push/v1/frame.proto",
            "proto/push/internal/v1/messaging.proto",
        ],
        &["proto"],
    )
}
