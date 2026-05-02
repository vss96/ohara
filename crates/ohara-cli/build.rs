//! Inject `OHARA_GIT_SHA` (short) into the build env so `ohara --version`
//! can disambiguate local dev builds from tagged releases. Falls back to
//! "unknown" when not in a git checkout (e.g. building from a source
//! tarball produced by cargo-dist).

fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=OHARA_GIT_SHA={sha}");

    // Re-run when HEAD moves so the SHA stays accurate without
    // requiring a clean rebuild.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    println!("cargo:rerun-if-changed=build.rs");
}
