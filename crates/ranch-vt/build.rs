//! Link against the prebuilt libghostty-vt (pinned ghostty source, built via
//! Zig — see justfile `vt-build`).

use std::env;
use std::path::PathBuf;

fn main() {
    let lib_dir = env::var("RANCH_VT_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Default: <workspace>/vendor/lib
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
                .join("../../vendor/lib")
        });
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
    println!("cargo:rerun-if-env-changed=RANCH_VT_LIB_DIR");
    println!("cargo:rerun-if-changed={}", lib_dir.display());
}
