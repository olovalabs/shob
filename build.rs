use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=KATHAMO_AGENT_BROWSER_BIN");
    let destination = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("agent-browser.bin");
    if let Some(source) = env::var_os("KATHAMO_AGENT_BROWSER_BIN") {
        println!("cargo:rerun-if-changed={}", PathBuf::from(&source).display());
        fs::copy(source, destination).expect("could not embed agent-browser executable");
    } else {
        fs::write(destination, []).expect("could not create agent-browser placeholder");
    }
}
