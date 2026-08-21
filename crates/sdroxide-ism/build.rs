//! Builds the vendored rtl_433 and the shim that drives it.
//!
//! Does nothing at all unless the `rtl433` feature is on, so the crate's native
//! decoders still build with no C toolchain and no submodule checked out.
//!
//! `vendor/rtl_433` is a git submodule, used unmodified. Upstream's own
//! `src/CMakeLists.txt` already defines a static `r_433` target holding
//! everything except `main()`, which is exactly the library an embedder wants —
//! so this builds that target rather than curating a source list that would
//! drift every time upstream adds a decoder.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_RTL433").is_none() {
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let rtl433 = manifest.join("../../vendor/rtl_433");

    if !rtl433.join("include/r_flow.h").exists() {
        panic!(
            "vendored rtl_433 sources are missing at {}\n\
             run: git submodule update --init --recursive\n\
             (or build with --no-default-features to leave the rtl_433 decoders out)",
            rtl433.display()
        );
    }
    let rtl433 = rtl433.canonicalize().expect("canonicalize vendor/rtl_433");

    let dst = cmake::Config::new(&rtl433)
        // We hand rtl_433 samples ourselves, so none of its own inputs are
        // wanted. ENABLE_RTLSDR in particular defaults to ON *and is fatal* when
        // librtlsdr is absent, which would make the whole build depend on a
        // library sdroxide deliberately does not use.
        .define("ENABLE_RTLSDR", "OFF")
        .define("ENABLE_SOAPYSDR", "OFF")
        .define("ENABLE_OPENSSL", "OFF")
        .define("BUILD_TESTING", "OFF")
        // Upstream still declares `cmake_minimum_required(VERSION 2.6...3.10)`,
        // which CMake 4 refuses outright.
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        // The decoders run on every IQ block; an unoptimised build of them is
        // not worth having even under `cargo build`.
        .profile("Release")
        .build_target("r_433")
        .build();

    let build = dst.join("build");
    println!("cargo:rustc-link-search=native={}", build.join("src").display());
    println!("cargo:rustc-link-lib=static=r_433");

    if cfg!(target_os = "windows") {
        // mongoose, which r_api.c's network outputs pull in.
        println!("cargo:rustc-link-lib=dylib=ws2_32");
    } else {
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=pthread");
    }

    let includes = [rtl433.join("include"), build.join("include")];

    let mut shim = cc::Build::new();
    shim.file(manifest.join("src/rtl433/shim.c")).opt_level(2).std("c99");
    for inc in &includes {
        shim.include(inc);
    }
    shim.compile("sdroxide_rtl433_shim");

    // Only our own header: rtl_433's would drag mongoose and the pulse detector
    // into Rust, none of which the bridge names.
    let bindings = bindgen::Builder::default()
        .header(manifest.join("src/rtl433/shim.h").to_string_lossy())
        .allowlist_function("sdrx_rtl433_.*")
        .allowlist_type("sdrx_rtl433_.*")
        .allowlist_var("SDRX_RTL433_.*")
        .layout_tests(false)
        .generate()
        .expect("bindgen over shim.h");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out.join("bindings.rs")).expect("write bindings.rs");

    println!("cargo:rerun-if-changed=src/rtl433/shim.c");
    println!("cargo:rerun-if-changed=src/rtl433/shim.h");
    println!("cargo:rerun-if-changed={}", rtl433.join("src").display());
    println!("cargo:rerun-if-changed={}", rtl433.join("include").display());
}
