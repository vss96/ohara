//! Cargo build script for ohara-embed.
//!
//! Sole purpose: emit framework-link directives for ort's CoreML
//! execution provider when both `feature = "coreml"` and the target is
//! macOS. ort-sys compiles the CoreML glue but expects the linker to
//! be told about CoreML.framework + Foundation.framework explicitly.
//! Without these, `ld` reports missing symbols like
//! `onnxruntime::coreml::util::CoreMLVersion()`.

fn main() {
    let coreml_feature = std::env::var("CARGO_FEATURE_COREML").is_ok();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if coreml_feature && target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=CoreML");
        println!("cargo:rustc-link-lib=framework=Foundation");
        // ort-sys's CoreML glue uses Objective-C `@available(...)` runtime
        // checks, which the compiler emits as `__isPlatformVersionAtLeast`
        // calls. That builtin lives in clang's compiler-rt
        // (`libclang_rt.osx.a`); without an explicit link, ld fails with
        // "Undefined symbols: ___isPlatformVersionAtLeast".
        if let Ok(prefix) = std::process::Command::new("xcrun")
            .arg("--show-sdk-path")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        {
            // Walk up from the SDK path to find clang's resource dir.
            // Typical: /Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/clang/<ver>/lib/darwin/
            // Fall back to letting the linker find it via -lclang_rt.osx.
            let _ = prefix; // keep `xcrun` discovery as a probe; static path below is the actual link.
        }
        // Resolve the toolchain's clang_rt.osx via xcrun. Letting cargo's
        // linker (cc) find it automatically is unreliable across Xcode
        // versions; emitting an absolute -L path is the robust fix.
        if let Ok(out) = std::process::Command::new("xcrun")
            .args(["--find", "clang"])
            .output()
        {
            let clang = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Ok(out) = std::process::Command::new(&clang)
                .args(["--print-resource-dir"])
                .output()
            {
                let resource_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !resource_dir.is_empty() {
                    println!("cargo:rustc-link-search=native={resource_dir}/lib/darwin");
                    println!("cargo:rustc-link-lib=static=clang_rt.osx");
                }
            }
        }
    }

    // CUDA / others have their own link conventions and aren't handled
    // here — `ort-sys` does its own CUDA discovery via env vars when its
    // `cuda` feature is enabled.

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_COREML");
}
