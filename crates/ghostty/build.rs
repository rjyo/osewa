// Links the pinned libghostty-vt static library that scripts/libghostty.sh
// builds into .cache/libghostty/<os>-<arch>, or OSEWA_LIBGHOSTTY when set,
// and compiles the C bridge against its headers.
use std::env;
use std::path::PathBuf;

fn main() {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    // The directory names follow `uname`, as scripts/libghostty.sh makes them.
    // The iOS simulator is its own platform to the linker: a device
    // archive doesn't link there.
    let sim = env::var("CARGO_CFG_TARGET_ABI").is_ok_and(|a| a == "sim");
    let platform = match (os.as_str(), arch.as_str()) {
        ("macos", "aarch64") => "darwin-arm64".to_string(),
        ("macos", a) => format!("darwin-{a}"),
        (o, a) if sim => format!("{o}-{a}-simulator"),
        (o, a) => format!("{o}-{a}"),
    };
    println!("cargo:rerun-if-env-changed=OSEWA_LIBGHOSTTY");
    let dir = match env::var_os("OSEWA_LIBGHOSTTY") {
        Some(d) => PathBuf::from(d),
        None => {
            let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
            root.join(".cache/libghostty").join(&platform)
        }
    };
    let lib = dir.join("lib/libghostty-vt.a");
    if !lib.exists() {
        panic!("{} is missing: run `make libghostty`", lib.display());
    }
    println!("cargo:rerun-if-changed={}", lib.display());
    println!("cargo:rerun-if-changed=csrc/bridge.c");
    println!("cargo:rerun-if-changed=csrc/keys.h");
    cc::Build::new()
        .file("csrc/bridge.c")
        .include(dir.join("include"))
        .define("GHOSTTY_STATIC", None)
        .warnings(false)
        .compile("osewa_ghostty_bridge");
    // Only the archive goes on the search path: next to it sits a dylib
    // the linker would prefer.
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    std::fs::copy(&lib, out.join("libghostty-vt.a")).unwrap();
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    if os == "linux" {
        println!("cargo:rustc-link-lib=m");
    }
}
