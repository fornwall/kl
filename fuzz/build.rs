//! Links the libghostty-vt that building kl left under `target/ghostty/`. This
//! crate has none of the logic to build it, so kl has to be built first.

use std::env;
use std::fs;
use std::path::Path;

fn main() -> Result<(), String> {
    let build_script = "../build.rs";
    println!("cargo:rerun-if-changed={build_script}");
    let source = fs::read_to_string(build_script).map_err(|error| error.to_string())?;
    let commit = source
        .split_once("GHOSTTY_COMMIT: &str = \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .ok_or("no GHOSTTY_COMMIT in kl's build script")?
        .0;

    let arch = env::var("CARGO_CFG_TARGET_ARCH").map_err(|error| error.to_string())?;
    let os = env::var("CARGO_CFG_TARGET_OS").map_err(|error| error.to_string())?;
    let platform = if os == "macos" { "macos" } else { "linux-gnu" };
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").map_err(|error| error.to_string())?;
    let lib = Path::new(&manifest_dir)
        .join("../target/ghostty")
        .join(commit)
        .join(format!("lib-{arch}-{platform}/lib"));
    if !lib.join("libghostty-vt.a").exists() {
        return Err(format!("no {}: run cargo build first", lib.display()));
    }
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    Ok(())
}
