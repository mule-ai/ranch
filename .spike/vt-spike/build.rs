// Link against the prebuilt libghostty-vt (built from pinned ghostty source via Zig).
use std::env;
use std::path::PathBuf;

fn main() {
    let lib_dir = match env::var("LIBGHOSTTY_VT_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            // Default: this crate lives in .spike/vt-spike, lib next to it.
            let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
            PathBuf::from(manifest).join("lib")
        }
    };
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,{}",
        lib_dir.display()
    );
    println!("cargo:rerun-if-changed=build.rs");
}
