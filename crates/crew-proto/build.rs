use std::path::Path;

const NIX_PROTOC: &str = "/run/current-system/sw/bin/protoc";

fn main() {
    println!("cargo:rerun-if-changed=proto/usher.proto");
    println!("cargo:rerun-if-changed=proto/ledger.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");
    let mut config = prost_build::Config::new();
    if std::env::var_os("PROTOC").is_none() && Path::new(NIX_PROTOC).exists() {
        config.protoc_executable(NIX_PROTOC);
    }
    config
        .compile_protos(&["proto/usher.proto", "proto/ledger.proto"], &["proto"])
        .expect("compile protos");
}
