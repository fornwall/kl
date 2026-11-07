//! Fetches libghostty-vt at a pinned commit of ghostty's `main` branch and
//! builds it as an optimized, minimal static library with zig.
//!
//! Everything lands in `<target>/ghostty/<commit>/`, shared between profiles,
//! so the slow part only runs once per pin. Moving the pin means updating
//! [`GHOSTTY_COMMIT`] to a newer commit on `main`, and checking that
//! [`GHOSTTY_PATCHES`] still apply.

use std::env;
use std::fmt::Display;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

const GHOSTTY_REPO: &str = "https://github.com/ghostty-org/ghostty.git";

/// A commit on ghostty's `main` branch. Never another branch, fork or tag.
const GHOSTTY_COMMIT: &str = "5813c9461d5af78b6307a1254d00b1bc190eaccd";

/// Applied in order on top of the pinned commit, which stays a plain commit on
/// `main`.
///
/// The first puts each function and data item of the static library in its own
/// section, so that the linker's `--gc-sections` drops the parts of the C API kl
/// does not call. Vendored as the net diff of
/// <https://github.com/ghostty-org/ghostty/pull/13708>, which was closed
/// unmerged, so that the build depends on nothing but the pin.
///
/// The second keeps the PNG decoder of the Ghostty app in the library, since most
/// programs send their images as PNG. See UPSTREAM.md.
const GHOSTTY_PATCHES: [&str; 2] = [
    "patches/ghostty-split-static-sections.patch",
    "patches/ghostty-decode-png-in-library.patch",
];

/// Only the parts of libghostty-vt that kl calls. Functions of a left-out
/// feature stay exported but return `GHOSTTY_UNSUPPORTED`.
const VT_FEATURES: &str = "-all,+render-state,+input-encode,+selection,+formatter,+grid-introspection,+snapshot,+search,\
     +kitty-graphics";

fn main() -> Result<(), String> {
    println!("cargo:rerun-if-changed=build.rs");
    for patch in GHOSTTY_PATCHES {
        println!("cargo:rerun-if-changed={patch}");
    }

    let target = context(env::var("TARGET"), "reading TARGET")?;
    let zig_target = zig_target(&target)?;
    let out_dir = PathBuf::from(context(env::var("OUT_DIR"), "reading OUT_DIR")?);
    let host = context(env::var("HOST"), "reading HOST")?;
    let root = target_dir(&out_dir, target != host)?
        .join("ghostty")
        .join(GHOSTTY_COMMIT);
    context(fs::create_dir_all(&root), "creating the ghostty directory")?;

    // cargo and an editor's cargo check can run this script at the same time.
    let lock = context(File::create(root.join("lock")), "creating the lock file")?;
    context(lock.lock(), "locking the ghostty directory")?;

    let manifest_dir = context(env::var("CARGO_MANIFEST_DIR"), "reading CARGO_MANIFEST_DIR")?;
    let patch_paths = GHOSTTY_PATCHES.map(|patch| Path::new(&manifest_dir).join(patch));
    let mut patches = String::new();
    for path in &patch_paths {
        patches.push_str(&context(
            fs::read_to_string(path),
            "reading a ghostty patch",
        )?);
    }

    let src = root.join("src");
    fetch_source(&src)?;
    apply_patches(&src, &patch_paths, &patches)?;
    check_zig_version(&src)?;
    let packages = fetch_packages(&root, &src)?;
    let inputs = format!("{VT_FEATURES}\n{patches}");
    let prefix = build_library(&root, &src, &packages, zig_target, &inputs)?;

    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    Ok(())
}

fn context<T, E: Display>(result: Result<T, E>, what: &str) -> Result<T, String> {
    result.map_err(|error| format!("{what}: {error}"))
}

fn zig_target(target: &str) -> Result<&'static str, String> {
    match target {
        "x86_64-unknown-linux-gnu" => Ok("x86_64-linux-gnu"),
        "x86_64-unknown-linux-musl" => Ok("x86_64-linux-musl"),
        "aarch64-unknown-linux-gnu" => Ok("aarch64-linux-gnu"),
        "aarch64-unknown-linux-musl" => Ok("aarch64-linux-musl"),
        "aarch64-apple-darwin" => Ok("aarch64-macos"),
        other => Err(format!("unsupported target {other}")),
    }
}

/// `OUT_DIR` is `<target>[/<triple>]/<profile>/build/<pkg>/out`, with the triple
/// always there when cross-compiling. Going all the way up to `<target>` shares
/// one fetch of ghostty between profiles and targets.
fn target_dir(out_dir: &Path, cross_compiling: bool) -> Result<PathBuf, String> {
    out_dir
        .ancestors()
        .nth(if cross_compiling { 5 } else { 4 })
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("unexpected OUT_DIR {}", out_dir.display()))
}

/// Runs `command`, returning its stdout. Output is captured because cargo
/// would otherwise parse a child's stdout as build script directives.
fn run(command: &mut Command, what: &str) -> Result<String, String> {
    let output = context(command.output(), what)?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(format!(
        "{what} failed with {}:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn git(src: &Path, args: &[&str], what: &str) -> Result<String, String> {
    run(
        Command::new("git")
            .current_dir(src)
            .args(args)
            // A user's url.insteadOf could turn this anonymous https fetch into
            // one that needs an ssh key.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE"),
        what,
    )
}

/// A shallow fetch of exactly the pinned commit. Git verifies the content
/// against the commit hash, so the pin needs no separate checksum.
fn fetch_source(src: &Path) -> Result<(), String> {
    let fetched = src.join(".kl-fetched");
    if fetched.exists() {
        return Ok(());
    }
    if src.exists() {
        context(fs::remove_dir_all(src), "removing a partial ghostty fetch")?;
    }
    context(
        fs::create_dir_all(src),
        "creating the ghostty source directory",
    )?;

    git(src, &["init", "-q"], "git init")?;
    git(
        src,
        &["fetch", "-q", "--depth", "1", GHOSTTY_REPO, GHOSTTY_COMMIT],
        "fetching ghostty",
    )?;
    git(
        src,
        &["checkout", "-q", "--detach", "FETCH_HEAD"],
        "checking out ghostty",
    )?;
    context(
        File::create(fetched),
        "marking the ghostty fetch as complete",
    )
    .map(drop)
}

/// Starts from a pristine checkout, so that a changed patch is not applied on
/// top of its previous version.
fn apply_patches(src: &Path, paths: &[PathBuf], patches: &str) -> Result<(), String> {
    let stamp = src.join(".kl-patched");
    if fs::read_to_string(&stamp).is_ok_and(|applied| applied == patches) {
        return Ok(());
    }
    git(
        src,
        &["checkout", "-q", "--force", "--detach", GHOSTTY_COMMIT],
        "resetting ghostty to the pin",
    )?;
    for path in paths {
        let path = path.to_str().ok_or("a ghostty patch path is not UTF-8")?;
        git(src, &["apply", path], "applying a ghostty patch")?;
    }
    context(fs::write(stamp, patches), "recording the ghostty patches")
}

fn major_minor(version: &str) -> Option<(&str, &str)> {
    let mut parts = version.split('.');
    Some((parts.next()?, parts.next()?))
}

/// Zig releases break compatibility, so ghostty builds with exactly one
/// major.minor. Catching a mismatch here beats a wall of compile errors.
fn check_zig_version(src: &Path) -> Result<(), String> {
    let zon = context(
        fs::read_to_string(src.join("build.zig.zon")),
        "reading build.zig.zon",
    )?;
    let required = zon
        .lines()
        .find_map(|line| line.trim().strip_prefix(".minimum_zig_version = \""))
        .and_then(|rest| rest.split('"').next())
        .ok_or("build.zig.zon has no minimum_zig_version")?;

    let installed = run(
        Command::new("zig").arg("version"),
        "running zig (see https://ziglang.org/download/)",
    )?;
    let installed = installed.trim();

    if major_minor(installed) == major_minor(required) {
        return Ok(());
    }
    Err(format!(
        "libghostty-vt needs zig {required} (any patch release), found {installed}"
    ))
}

/// Downloads only the zig packages that libghostty-vt uses. A plain `zig build`
/// would instead pull in everything the Ghostty app needs, over 300 MB of it.
fn fetch_packages(root: &Path, src: &Path) -> Result<PathBuf, String> {
    let packages = root.join("packages");
    if packages.exists() {
        return Ok(packages);
    }
    run(
        Command::new("sh")
            .current_dir(src)
            .args([
                "nix/build-support/fetch-zig-cache.sh",
                "deps/libghostty-vt.txt",
            ])
            .env("ZIG_GLOBAL_CACHE_DIR", zig_cache(root)),
        "fetching zig packages",
    )?;
    // Zig 0.17 can crash when a --system build finds package archives in its
    // global cache. Moving them out lets the build reuse the rest of the cache,
    // which saves compiling the build runner a second time.
    context(
        fs::rename(zig_cache(root).join("p"), &packages),
        "moving the zig packages",
    )?;
    Ok(packages)
}

fn zig_cache(root: &Path) -> PathBuf {
    root.join("zig-cache")
}

/// Builds into a temporary prefix and renames it when done, so an interrupted
/// build is never mistaken for a finished one.
fn build_library(
    root: &Path,
    src: &Path,
    packages: &Path,
    zig_target: &str,
    inputs: &str,
) -> Result<PathBuf, String> {
    let prefix = root.join(format!("lib-{zig_target}"));
    let stamp = "build-inputs";
    if fs::read_to_string(prefix.join(stamp)).is_ok_and(|built| built == inputs) {
        return Ok(prefix);
    }

    let partial = root.join(format!("lib-{zig_target}.partial"));
    for stale in [&prefix, &partial] {
        if stale.exists() {
            context(fs::remove_dir_all(stale), "removing an old libghostty-vt")?;
        }
    }

    run(
        Command::new("zig")
            .current_dir(src)
            .arg("build")
            .arg("--system")
            .arg(packages)
            .arg("--prefix")
            .arg(&partial)
            .args([
                "-Demit-lib-vt",
                "-Doptimize=ReleaseFast",
                // Not `native`: the SIMD paths dispatch at runtime, and a binary
                // has to keep working on machines other than the build host.
                "-Dcpu=baseline",
                "-Dstrip=true",
            ])
            .arg(format!("-Dtarget={zig_target}"))
            .arg(format!("-Dvt-features={VT_FEATURES}"))
            .env("ZIG_GLOBAL_CACHE_DIR", zig_cache(root)),
        "building libghostty-vt",
    )?;

    context(
        fs::write(partial.join(stamp), inputs),
        "recording the libghostty-vt build inputs",
    )?;
    context(fs::rename(&partial, &prefix), "installing libghostty-vt")?;
    Ok(prefix)
}
