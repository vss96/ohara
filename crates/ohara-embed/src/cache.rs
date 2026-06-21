//! Per-machine cache location for downloaded embedding models and the
//! CoreML compiled-model cache.
//!
//! fastembed's own default ([`fastembed::get_cache_dir`]) is the
//! working-directory-relative `.fastembed_cache`, so the ~130MB model
//! download *and* the ~30s CoreML compile would be re-paid for every
//! directory `ohara index` is run from. Instead we resolve a stable
//! OS-level cache directory — matching the daemon-registry convention in
//! `ohara-engine` (`~/Library/Caches/ohara` on macOS,
//! `${XDG_CACHE_HOME:-~/.cache}/ohara` elsewhere) — so models download and
//! compile once per machine, not once per working directory.
//!
//! An explicit `FASTEMBED_CACHE_DIR` always wins: it preserves
//! back-compat for anyone already relying on fastembed's env var, and
//! lets a caller pin a per-project cache when they actually want one.

use std::ffi::OsString;
use std::path::PathBuf;

/// Subdirectory under the OS cache root that holds model snapshots and
/// the CoreML compile cache. Kept distinct from the daemon's `daemon/`
/// subtree, which lives under the same `ohara` cache root.
const MODELS_SUBDIR: &str = "models";

/// Resolve the cache directory for embedding models and the CoreML
/// compile cache. Honors `FASTEMBED_CACHE_DIR`; otherwise returns a
/// stable per-machine OS cache location. See the module docs.
pub fn cache_dir() -> PathBuf {
    resolve_cache_dir(
        std::env::var_os("FASTEMBED_CACHE_DIR"),
        cfg!(target_os = "macos"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_CACHE_HOME"),
    )
}

/// Pure resolution, split out from environment access so it can be tested
/// deterministically without mutating process-global env vars.
fn resolve_cache_dir(
    fastembed_override: Option<OsString>,
    is_macos: bool,
    home: Option<OsString>,
    xdg_cache_home: Option<OsString>,
) -> PathBuf {
    // An explicit fastembed cache dir always wins (back-compat + opt-in
    // per-project caches).
    if let Some(dir) = fastembed_override {
        return PathBuf::from(dir);
    }
    // Otherwise the OS cache root, matching `ohara-engine`'s daemon
    // registry convention so every per-machine artifact lives under one
    // `ohara` tree.
    let root = if is_macos {
        home.map(|h| PathBuf::from(h).join("Library/Caches/ohara"))
    } else if let Some(xdg) = xdg_cache_home {
        Some(PathBuf::from(xdg).join("ohara"))
    } else {
        home.map(|h| PathBuf::from(h).join(".cache").join("ohara"))
    };
    // If even HOME can't be resolved, fall back to fastembed's legacy
    // working-directory cache — never worse than the prior behavior.
    root.map(|r| r.join(MODELS_SUBDIR))
        .unwrap_or_else(|| PathBuf::from(".fastembed_cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_fastembed_override_always_wins() {
        let got = resolve_cache_dir(
            Some(OsString::from("/custom/cache")),
            true,
            Some(OsString::from("/Users/x")),
            Some(OsString::from("/xdg")),
        );
        assert_eq!(got, PathBuf::from("/custom/cache"));
    }

    #[test]
    fn macos_uses_library_caches_under_home() {
        let got = resolve_cache_dir(None, true, Some(OsString::from("/Users/x")), None);
        assert_eq!(got, PathBuf::from("/Users/x/Library/Caches/ohara/models"));
    }

    #[test]
    fn non_macos_prefers_xdg_cache_home_over_home() {
        let got = resolve_cache_dir(
            None,
            false,
            Some(OsString::from("/home/x")),
            Some(OsString::from("/xdg")),
        );
        assert_eq!(got, PathBuf::from("/xdg/ohara/models"));
    }

    #[test]
    fn non_macos_falls_back_to_dot_cache_under_home() {
        let got = resolve_cache_dir(None, false, Some(OsString::from("/home/x")), None);
        assert_eq!(got, PathBuf::from("/home/x/.cache/ohara/models"));
    }

    #[test]
    fn without_home_falls_back_to_legacy_cwd_cache() {
        // Never worse than fastembed's prior default.
        let got = resolve_cache_dir(None, true, None, None);
        assert_eq!(got, PathBuf::from(".fastembed_cache"));
    }
}
