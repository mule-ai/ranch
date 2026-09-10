//! Link against the prebuilt libghostty-vt (pinned ghostty source, built
//! via Zig — see Makefile `vt-lib` target). Prefers the static archive
//! (`libghostty-vt.a`, no runtime .so needed); falls back to the shared
//! library when only that exists.

use std::env;
use std::path::PathBuf;

fn main() {
    let lib_dir = env::var("RANCH_VT_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Default: <workspace>/vendor/lib
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../vendor/lib")
        });
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if lib_dir.join("libghostty-vt.a").exists() {
        // static archive; Zig-built and self-contained (libc stubs it uses
        // resolve from the final binary's libc)
        println!("cargo:rustc-link-lib=static=ghostty-vt");
        // zig builds the archive with compiler_rt symbols that may need
        // linking after — a second pass lets lld resolve them
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    } else {
        println!("cargo:rustc-link-lib=dylib=ghostty-vt");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
    }
    println!("cargo:rerun-if-env-changed=RANCH_VT_LIB_DIR");
    println!("cargo:rerun-if-changed={}", lib_dir.display());
}
