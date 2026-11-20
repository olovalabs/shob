use std::{env, fs, path::PathBuf};

fn main() {
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let manifest = fs::read_to_string(manifest).expect("read Shob manifest");
    let version = manifest
        .lines()
        .skip_while(|line| line.trim() != "[package]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| key.trim() == "version")
        .map(|(_, value)| value.trim().trim_matches('"'))
        .expect("Shob package version");
    println!("cargo:rustc-env=SHOB_APP_VERSION={version}");
}
