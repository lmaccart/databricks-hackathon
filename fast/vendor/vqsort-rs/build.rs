use std::{env, path::PathBuf};

fn main() {
    let highway = pkg_config::Config::new()
        .probe("libhwy-contrib")
        .expect("install the pinned Google Highway development package");
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let build_rs_path = manifest_dir.join("build.rs");
    let source_path = manifest_dir.join("src").join("vqsort.cpp");

    println!("cargo:rerun-if-changed={}", build_rs_path.display());
    println!("cargo:rerun-if-changed={}", source_path.display());

    let mut build = cc::Build::new();
    if highway.version.starts_with("1.0.") {
        build.define("HWY_USE_SORTER", None);
    }
    build.cpp(true).file(source_path);
    for include in highway.include_paths {
        build.include(include);
    }
    build.compile("vqsort");
}
