use std::path::Path;

const NIX_PROTOC: &str = "/run/current-system/sw/bin/protoc";

fn main() {
    println!("cargo:rerun-if-changed=proto/usher.proto");
    println!("cargo:rerun-if-changed=proto/ledger.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");
    let mut config = prost_build::Config::new();
    // PROTOC wins (prost-build reads it); else protoc on PATH; else the NixOS system profile.
    if std::env::var_os("PROTOC").is_none() && !on_path("protoc") && Path::new(NIX_PROTOC).exists()
    {
        config.protoc_executable(NIX_PROTOC);
    }
    config
        .compile_protos(&["proto/usher.proto", "proto/ledger.proto"], &["proto"])
        .expect("compile protos: install protoc (apt install protobuf-compiler) or set PROTOC");
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file()))
}
