// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Phase-1 plugin manifest: parsing, hash verification, and signing policy.
//!
//! The manifest carries:
//!   * artifact location and integrity (sha256)
//!   * execution limits
//!   * compatibility declaration
//!   * trust policy
//!
//! It is **not** authoritative for component declarations — those come from
//! the plugin's `descriptor()` export (see `otap-df-plugin-api`).

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

use std::path::{Path, PathBuf};

use otap_df_plugin_api::PluginError;
use serde::{Deserialize, Serialize};

/// Top-level manifest, deserialized from YAML.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginManifest {
    /// API version of the manifest schema itself, e.g. `otap.plugin/v1alpha1`.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Manifest kind. Phase 1 accepts `WasmPlugin` (legacy/experimental) and
    /// `NativePlugin` (the supported zero-copy hot-path).
    pub kind: ManifestKind,
    /// Plugin metadata.
    pub metadata: Metadata,
    /// Runtime backend declaration.
    pub runtime: RuntimeSpec,
    /// Optional version-compat declaration.
    #[serde(default)]
    pub compatibility: Compatibility,
    /// Optional execution limits.
    #[serde(default)]
    pub limits: Limits,
    /// Optional plugin-side telemetry settings (host honors only `log_level`
    /// in phase 1).
    #[serde(default)]
    pub telemetry: TelemetrySettings,
    /// Optional signing policy.
    #[serde(default)]
    pub signing: SigningSpec,
}

/// Manifest kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ManifestKind {
    /// A Wasmtime-component-based plugin (experimental, parked for the
    /// hot path — see docs/dynamic-processor-exporter-plugins-phase1.md).
    WasmPlugin,
    /// A native cdylib plugin (phase-1 supported zero-copy path).
    NativePlugin,
}

/// Plugin metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metadata {
    /// Plugin name (matches descriptor.name).
    pub name: String,
    /// Plugin version (matches descriptor.version).
    pub version: String,
}

/// Runtime backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RuntimeSpec {
    /// Wasmtime-component backend.
    WasmtimeComponent {
        /// Path to the `.wasm` artifact, relative to the manifest file.
        path: PathBuf,
        /// Hex-encoded SHA-256 of the artifact (required, always checked).
        sha256: String,
    },
    /// Native cdylib backend (phase-1 supported zero-copy path).
    NativeCdylib {
        /// Path to the cdylib (`.so` / `.dylib` / `.dll`) artifact,
        /// relative to the manifest file.
        path: PathBuf,
        /// Hex-encoded SHA-256 of the artifact (required, always checked).
        sha256: String,
    },
}

/// Plugin-API compatibility declaration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Compatibility {
    /// Plugin API version range the plugin is compatible with, expressed
    /// as a semver-style constraint. Authoritative compat check is the
    /// descriptor; this is an early reject mechanism.
    #[serde(rename = "pluginApi", default)]
    pub plugin_api: Option<String>,
}

/// Execution limits applied at instantiation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    /// Per-instance Wasm linear memory cap in bytes.
    #[serde(rename = "memoryMaxBytes")]
    pub memory_max_bytes: u64,
    /// Per-call fuel cap (secondary guard, primary is epoch interruption).
    #[serde(rename = "fuelPerCall", default)]
    pub fuel_per_call: Option<u64>,
    /// Per-call wall-clock cap in milliseconds — enforced by Wasmtime
    /// epoch interruption.
    #[serde(rename = "timeoutMs")]
    pub timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        // Conservative phase-1 defaults: 16 MiB memory, 10 ms deadline,
        // no fuel cap (epoch is primary).
        Self {
            memory_max_bytes: 16 * 1024 * 1024,
            fuel_per_call: None,
            timeout_ms: 10,
        }
    }
}

/// Plugin-side telemetry settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TelemetrySettings {
    /// Minimum log level the plugin's telemetry import is permitted to
    /// emit at. Phase 1 only honors logs.
    #[serde(rename = "logLevel", default)]
    pub log_level: Option<String>,
}

/// Signing policy.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SigningSpec {
    /// Path to the trust-root public key (minisign format), relative to
    /// the manifest file or absolute.
    #[serde(rename = "minisignPublicKeyPath", default)]
    pub minisign_public_key_path: Option<PathBuf>,
    /// Optional explicit path to the artifact's minisign signature file.
    /// When unset, defaults to `<artifact>.minisig` next to the artifact
    /// (the upstream minisign tool's default convention).
    #[serde(rename = "minisignSignaturePath", default)]
    pub minisign_signature_path: Option<PathBuf>,
}

/// Result of loading a manifest from disk: the parsed manifest plus the
/// resolved paths derived from it.
#[derive(Clone, Debug)]
pub struct LoadedManifest {
    /// The original manifest path (used for relative-path resolution).
    pub manifest_path: PathBuf,
    /// The deserialized manifest content.
    pub manifest: PluginManifest,
    /// Absolute path to the artifact (resolved against `manifest_path`).
    pub artifact_path: PathBuf,
}

/// Parse a manifest YAML file and resolve artifact paths.
pub fn load_manifest(manifest_path: &Path) -> Result<LoadedManifest, PluginError> {
    let bytes = std::fs::read(manifest_path)?;
    let manifest: PluginManifest =
        serde_yaml::from_slice(&bytes).map_err(|e| PluginError::ManifestParse(format!("{e}")))?;

    // Cross-validate kind vs runtime: WasmPlugin must carry
    // WasmtimeComponent runtime; NativePlugin must carry NativeCdylib.
    match (&manifest.kind, &manifest.runtime) {
        (ManifestKind::WasmPlugin, RuntimeSpec::WasmtimeComponent { .. }) => {}
        (ManifestKind::NativePlugin, RuntimeSpec::NativeCdylib { .. }) => {}
        (k, r) => {
            return Err(PluginError::ManifestParse(format!(
                "manifest kind {k:?} does not match runtime {r:?}"
            )));
        }
    }

    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let artifact_path = match &manifest.runtime {
        RuntimeSpec::WasmtimeComponent { path, .. } | RuntimeSpec::NativeCdylib { path, .. } => {
            if path.is_absolute() {
                path.clone()
            } else {
                manifest_dir.join(path)
            }
        }
    };

    Ok(LoadedManifest {
        manifest_path: manifest_path.to_path_buf(),
        manifest,
        artifact_path,
    })
}

/// Verify that the artifact at `artifact_path` matches the SHA-256 declared
/// in the manifest.
///
/// Always required (phase-1 alpha policy: SHA required).
pub fn verify_artifact_sha256(loaded: &LoadedManifest) -> Result<(), PluginError> {
    use sha2::{Digest, Sha256};

    let expected_hex = match &loaded.manifest.runtime {
        RuntimeSpec::WasmtimeComponent { sha256, .. }
        | RuntimeSpec::NativeCdylib { sha256, .. } => sha256,
    };
    let bytes = std::fs::read(&loaded.artifact_path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = hasher.finalize();
    let actual_hex = data_encoding::HEXLOWER.encode(&actual);

    if actual_hex != expected_hex.to_lowercase() {
        return Err(PluginError::ArtifactIntegrity {
            details: format!(
                "expected sha256={expected_hex}, computed={actual_hex} for {}",
                loaded.artifact_path.display()
            ),
        });
    }
    Ok(())
}

/// Verify the artifact signature according to the manifest signing policy
/// and the host's `require_signature` flag.
///
/// Behavior:
/// * If the manifest has **no** `minisignPublicKeyPath` configured:
///   - When `require_signature == true`, the plugin is **rejected** with
///     [`PluginError::SignatureVerification`].
///   - Otherwise the plugin is accepted unsigned (phase-1 alpha policy).
/// * If the manifest has a `minisignPublicKeyPath` configured, the
///   artifact signature is **always** verified — regardless of the host
///   `require_signature` flag — using `minisign-verify`. The signature
///   file path defaults to `<artifact>.minisig`, with optional override
///   via `signing.minisignSignaturePath`. Any failure to load the public
///   key, load the signature, or verify the artifact bytes is reported as
///   [`PluginError::SignatureVerification`].
///
/// **Operational policy (RFC §10):** signed-by-default before stable
/// release; phase-1 alpha allows unsigned with a per-host opt-in flag.
pub fn verify_artifact_signature(
    loaded: &LoadedManifest,
    require_signature: bool,
) -> Result<(), PluginError> {
    let Some(pk_path) = loaded.manifest.signing.minisign_public_key_path.as_ref() else {
        if require_signature {
            return Err(PluginError::SignatureVerification {
                details: format!(
                    "manifest {} has no minisign public key configured but host requires signed plugins",
                    loaded.manifest_path.display()
                ),
            });
        }
        return Ok(());
    };

    let manifest_dir = loaded
        .manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let resolve = |p: &Path| -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            manifest_dir.join(p)
        }
    };

    let pk_path_abs = resolve(pk_path);
    let sig_path_abs = match &loaded.manifest.signing.minisign_signature_path {
        Some(p) => resolve(p),
        None => {
            // Default convention: <artifact>.minisig next to the artifact.
            let mut p = loaded.artifact_path.clone().into_os_string();
            p.push(".minisig");
            PathBuf::from(p)
        }
    };

    let public_key = minisign_verify::PublicKey::from_file(&pk_path_abs).map_err(|e| {
        PluginError::SignatureVerification {
            details: format!(
                "failed to load minisign public key {}: {e}",
                pk_path_abs.display()
            ),
        }
    })?;

    let signature = minisign_verify::Signature::from_file(&sig_path_abs).map_err(|e| {
        PluginError::SignatureVerification {
            details: format!(
                "failed to load minisign signature {}: {e}",
                sig_path_abs.display()
            ),
        }
    })?;

    let artifact_bytes = std::fs::read(&loaded.artifact_path)?;
    public_key
        .verify(&artifact_bytes, &signature, /* allow_legacy = */ false)
        .map_err(|e| PluginError::SignatureVerification {
            details: format!(
                "minisign verification failed for {} (key {}, signature {}): {e}",
                loaded.artifact_path.display(),
                pk_path_abs.display(),
                sig_path_abs.display(),
            ),
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("otap-plugin-manifest-test-{}", name));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
    }

    #[test]
    fn parses_minimal_manifest() {
        let m = b"apiVersion: otap.plugin/v1alpha1
kind: WasmPlugin
metadata:
  name: foo
  version: 0.1.0
runtime:
  kind: wasmtime-component
  path: ./foo.wasm
  sha256: 0000000000000000000000000000000000000000000000000000000000000000
limits:
  memoryMaxBytes: 16777216
  timeoutMs: 10
";
        let path = write_temp("parses_minimal_manifest_plugin.yaml", m);
        let loaded = load_manifest(&path).unwrap();
        assert_eq!(loaded.manifest.metadata.name, "foo");
    }

    #[test]
    fn sha_mismatch_rejected() {
        let manifest = b"apiVersion: otap.plugin/v1alpha1
kind: WasmPlugin
metadata:
  name: foo
  version: 0.1.0
runtime:
  kind: wasmtime-component
  path: ./foo.wasm
  sha256: deadbeef
limits:
  memoryMaxBytes: 1024
  timeoutMs: 10
";
        let mp = write_temp("sha_mismatch_plugin.yaml", manifest);
        // Write a fake artifact next to it.
        let _ = write_temp("foo.wasm", b"not-a-real-component");
        // Note: write_temp uses a per-name dir, so paths don't actually
        // colocate. Instead place artifact next to manifest:
        let dir = mp.parent().unwrap();
        std::fs::write(dir.join("foo.wasm"), b"not-a-real-component").unwrap();
        let loaded = load_manifest(&mp).unwrap();
        let err = verify_artifact_sha256(&loaded).unwrap_err();
        assert!(matches!(err, PluginError::ArtifactIntegrity { .. }));
    }

    /// Helpers for the signing tests.
    mod signing_helpers {
        use std::path::{Path, PathBuf};

        /// Generate an unencrypted minisign keypair, sign `artifact_bytes`,
        /// and write the public key box and signature file to `dir`.
        ///
        /// Returns `(public_key_path, signature_path)`.
        pub fn generate_keypair_and_sign(dir: &Path, artifact_bytes: &[u8]) -> (PathBuf, PathBuf) {
            let kp = minisign::KeyPair::generate_unencrypted_keypair()
                .expect("generate unencrypted minisign keypair");

            // Public key box (textual minisign format).
            let pk_box = kp.pk.to_box().expect("encode public key box");
            let pk_path = dir.join("plugin.pub");
            std::fs::write(&pk_path, pk_box.to_string()).expect("write public key");

            // Sign the artifact bytes.
            let sig_box = minisign::sign(
                None,
                &kp.sk,
                std::io::Cursor::new(artifact_bytes),
                /* trusted_comment = */ None,
                /* untrusted_comment = */ None,
            )
            .expect("sign artifact");
            let sig_path = dir.join("plugin.wasm.minisig");
            std::fs::write(&sig_path, sig_box.into_string()).expect("write signature");

            (pk_path, sig_path)
        }

        /// Same as [`generate_keypair_and_sign`] but writes the public key
        /// and signature path the caller specifies, and signs the bytes
        /// you pass — used to construct mismatched-signature scenarios.
        pub fn generate_keypair_sign_to(pk_path: &Path, sig_path: &Path, data_to_sign: &[u8]) {
            let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
            let pk_box = kp.pk.to_box().unwrap();
            std::fs::write(pk_path, pk_box.to_string()).unwrap();
            let sig_box =
                minisign::sign(None, &kp.sk, std::io::Cursor::new(data_to_sign), None, None)
                    .unwrap();
            std::fs::write(sig_path, sig_box.into_string()).unwrap();
        }
    }

    fn artifact_sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        data_encoding::HEXLOWER.encode(&h.finalize())
    }

    fn write_signed_manifest_layout(
        tmp: &Path,
        artifact_bytes: &[u8],
        public_key_in_manifest: bool,
        explicit_sig_path: Option<&str>,
    ) -> PathBuf {
        let artifact_path = tmp.join("plugin.wasm");
        std::fs::write(&artifact_path, artifact_bytes).unwrap();
        let sha = artifact_sha256_hex(artifact_bytes);

        let mut manifest = format!(
            "apiVersion: otap.plugin/v1alpha1
kind: WasmPlugin
metadata:
  name: signed-plugin
  version: 0.1.0
runtime:
  kind: wasmtime-component
  path: ./plugin.wasm
  sha256: {sha}
limits:
  memoryMaxBytes: 16777216
  timeoutMs: 10
"
        );
        if public_key_in_manifest {
            manifest.push_str("signing:\n  minisignPublicKeyPath: ./plugin.pub\n");
            if let Some(s) = explicit_sig_path {
                manifest.push_str(&format!("  minisignSignaturePath: {s}\n"));
            }
        }

        let manifest_path = tmp.join("plugin.yaml");
        std::fs::write(&manifest_path, manifest).unwrap();
        manifest_path
    }

    #[test]
    fn signature_valid_is_accepted_when_key_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = b"valid-artifact-bytes";
        let manifest_path = write_signed_manifest_layout(tmp.path(), artifact, true, None);
        let (_pk, _sig) = signing_helpers::generate_keypair_and_sign(tmp.path(), artifact);

        let loaded = load_manifest(&manifest_path).unwrap();
        verify_artifact_sha256(&loaded).expect("sha matches");
        verify_artifact_signature(&loaded, /* require_signature = */ false)
            .expect("valid signature should be accepted");
        verify_artifact_signature(&loaded, /* require_signature = */ true)
            .expect("valid signature should also satisfy require_signed");
    }

    #[test]
    fn signature_invalid_is_rejected_when_key_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = b"actual-artifact-bytes";
        let manifest_path = write_signed_manifest_layout(tmp.path(), artifact, true, None);
        // Sign DIFFERENT bytes than what's on disk -> verification must fail.
        signing_helpers::generate_keypair_sign_to(
            &tmp.path().join("plugin.pub"),
            &tmp.path().join("plugin.wasm.minisig"),
            b"some-other-bytes",
        );

        let loaded = load_manifest(&manifest_path).unwrap();
        let err = verify_artifact_signature(&loaded, false)
            .expect_err("tampered signature must be rejected");
        assert!(
            matches!(err, PluginError::SignatureVerification { .. }),
            "expected SignatureVerification, got {err:?}"
        );
    }

    #[test]
    fn signature_missing_file_is_rejected_when_key_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = b"unsigned-artifact-bytes";
        let manifest_path = write_signed_manifest_layout(tmp.path(), artifact, true, None);
        // Public key is referenced but no .minisig file exists.
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        std::fs::write(
            tmp.path().join("plugin.pub"),
            kp.pk.to_box().unwrap().to_string(),
        )
        .unwrap();

        let loaded = load_manifest(&manifest_path).unwrap();
        let err = verify_artifact_signature(&loaded, false)
            .expect_err("missing signature file must be rejected");
        assert!(matches!(err, PluginError::SignatureVerification { .. }));
    }

    #[test]
    fn missing_public_key_rejected_when_host_requires_signing() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = b"unsigned-but-policy-requires-signed";
        let manifest_path = write_signed_manifest_layout(tmp.path(), artifact, false, None);
        let loaded = load_manifest(&manifest_path).unwrap();
        let err = verify_artifact_signature(&loaded, /* require_signature = */ true)
            .expect_err("host requires signed; manifest has no key -> reject");
        assert!(matches!(err, PluginError::SignatureVerification { .. }));
    }

    #[test]
    fn missing_public_key_allowed_when_host_does_not_require_signing() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = b"unsigned-allowed-by-host";
        let manifest_path = write_signed_manifest_layout(tmp.path(), artifact, false, None);
        let loaded = load_manifest(&manifest_path).unwrap();
        verify_artifact_signature(&loaded, /* require_signature = */ false)
            .expect("unsigned plugin allowed under permissive policy");
    }

    #[test]
    fn signature_explicit_path_is_honored() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = b"artifact-with-custom-sig-path";
        let manifest_path =
            write_signed_manifest_layout(tmp.path(), artifact, true, Some("./custom.sig"));

        // Generate keypair, sign artifact, and write signature to the
        // *non-default* path so verification only succeeds if the
        // override is honored.
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        std::fs::write(
            tmp.path().join("plugin.pub"),
            kp.pk.to_box().unwrap().to_string(),
        )
        .unwrap();
        let sig_box =
            minisign::sign(None, &kp.sk, std::io::Cursor::new(artifact), None, None).unwrap();
        std::fs::write(tmp.path().join("custom.sig"), sig_box.into_string()).unwrap();

        let loaded = load_manifest(&manifest_path).unwrap();
        verify_artifact_signature(&loaded, false)
            .expect("verification should succeed via overridden signature path");
    }

    #[test]
    fn signature_wrong_public_key_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = b"artifact-with-mismatched-key";
        let manifest_path = write_signed_manifest_layout(tmp.path(), artifact, true, None);

        // Sign artifact with key A, but write key B's public key on disk.
        let signing_kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let sig_box = minisign::sign(
            None,
            &signing_kp.sk,
            std::io::Cursor::new(artifact),
            None,
            None,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("plugin.wasm.minisig"),
            sig_box.into_string(),
        )
        .unwrap();

        let other_kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        std::fs::write(
            tmp.path().join("plugin.pub"),
            other_kp.pk.to_box().unwrap().to_string(),
        )
        .unwrap();

        let loaded = load_manifest(&manifest_path).unwrap();
        let err = verify_artifact_signature(&loaded, false)
            .expect_err("signature with wrong key must be rejected");
        assert!(matches!(err, PluginError::SignatureVerification { .. }));
    }
}
