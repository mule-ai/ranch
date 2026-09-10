//! Propagate the native lib path to the final binary so `ranchd` can find
//! libghostty-vt.so.0 without needing LD_LIBRARY_PATH.

use std::env;
use std::path::PathBuf;

fn main() {
    let lib_dir = env::var("RANCH_VT_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../vendor/lib")
        });
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}
