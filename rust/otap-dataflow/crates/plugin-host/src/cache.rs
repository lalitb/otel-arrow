// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Cache key for the precompiled Wasmtime component.
//!
//! Cache key fields (RFC §14):
//!   * `artifact_sha256`            — content addressing
//!   * `wasmtime_version`           — coarse Wasmtime ABI generation
//!   * `engine_config_fingerprint`  — fingerprint of the host's Wasmtime
//!     [`Engine`]/[`Config`] (cranelift opts, module version strategy,
//!     wasmparser version, etc). Computed via
//!     `Engine::precompile_compatibility_hash`.
//!   * `target_triple`              — recompile when target changes
//!   * `plugin_api_version`         — host-side ABI bump invalidates cache

use otap_df_plugin_api::PluginApiVersion;

/// Stable identity of a precompiled component cache entry.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ComponentCacheKey {
    /// Hex-encoded SHA-256 of the source `.wasm` artifact.
    pub artifact_sha256: String,
    /// Wasmtime version the cache entry was produced with.
    pub wasmtime_version: String,
    /// Hex-encoded fingerprint of the host's Wasmtime engine/compiler
    /// configuration. Two hosts using different `Config` settings
    /// (cranelift opt level, module version strategy, etc) will produce
    /// different fingerprints and therefore different cache entries.
    /// `"engine-unwired"` when the wasmtime backend is not compiled in.
    pub engine_config_fingerprint: String,
    /// Target triple the cache entry was produced for.
    pub target_triple: String,
    /// Host plugin-API version.
    pub plugin_api_version: PluginApiVersion,
}

impl ComponentCacheKey {
    /// Derive a filesystem-friendly cache filename for this key.
    #[must_use]
    pub fn filename(&self) -> String {
        format!(
            "{}-{}-{}-{}-{}.cwasm",
            self.artifact_sha256,
            self.wasmtime_version.replace('.', "_"),
            self.engine_config_fingerprint,
            self.target_triple.replace(['-', '.'], "_"),
            self.plugin_api_version
        )
    }
}

/// Best-effort target triple of the running host. Hard-coded with a
/// `cfg`-based compose so this crate can produce stable cache keys
/// without unconditionally depending on wasmtime (the natural source
/// for this string when the `wasmtime-backend` feature is enabled).
#[must_use]
pub fn target_triple() -> &'static str {
    // Compile-time triple from std::env! when available is non-trivial; a
    // conservative compose from cfg flags is sufficient for cache keying.
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", target_os = "linux"),
        all(target_arch = "x86_64", target_os = "macos"),
        all(target_arch = "aarch64", target_os = "macos"),
        all(target_arch = "x86_64", target_os = "windows"),
    )))]
    {
        "unknown-target"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_filename_is_deterministic() {
        let k = ComponentCacheKey {
            artifact_sha256: "a".repeat(64),
            wasmtime_version: "27.0.0".into(),
            engine_config_fingerprint: "deadbeef00000000".into(),
            target_triple: "x86_64-unknown-linux-gnu".into(),
            plugin_api_version: PluginApiVersion::new(0, 1),
        };
        let f = k.filename();
        assert!(f.ends_with(".cwasm"));
        assert!(f.contains("27_0_0"));
        assert!(f.contains("deadbeef00000000"));
    }
}
