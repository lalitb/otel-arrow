// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! End-to-end integration tests that exercise the full plugin host path
//! through a real Wasmtime component.
//!
//! The fixture is a hand-written component-model WAT file
//! (`tests/fixtures/identity_plugin.wat`) compiled at test time via the
//! `wat` crate. This avoids any out-of-tree build tooling (no
//! `cargo component`, no `wit-bindgen`) while still exercising the real
//! Wasmtime backend, the real descriptor loader, the real
//! `validate-config` path, and the real `process` path.
//!
//! Coverage:
//!   * manifest load + sha-256 verification
//!   * descriptor load through Wasmtime
//!   * validate-config success
//!   * runner invocation end-to-end (identity transform)
//!   * the host's `LoadedPlugin` aggregate (validators, runners, fingerprints)
//!
//! Compile-time gated on the `wasmtime-backend` feature.

#![cfg(feature = "wasmtime-backend")]

use std::path::Path;

use otap_df_plugin_host::{PayloadKind, PluginHost, PluginHostConfig, result_class};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const FIXTURE_WAT: &str = include_str!("fixtures/identity_plugin.wat");

/// Compile the WAT fixture into component bytes and stage a manifest +
/// `.wasm` pair in a temp directory. Returns the temp dir (so the
/// caller keeps it alive) and the path to the generated YAML manifest.
fn stage_identity_plugin() -> (TempDir, std::path::PathBuf) {
    let component_bytes = wat::parse_str(FIXTURE_WAT).expect("WAT compiles to a component");
    assert!(component_bytes.len() > 8, "produced empty component");

    let dir = TempDir::new().expect("tempdir");
    let wasm_path = dir.path().join("identity.wasm");
    std::fs::write(&wasm_path, &component_bytes).expect("write wasm fixture");

    let mut hasher = Sha256::new();
    hasher.update(&component_bytes);
    let sha_hex = hex_lower(&hasher.finalize());

    let manifest_yaml = format!(
        "apiVersion: otap.plugin/v1alpha1
kind: WasmPlugin
metadata:
  name: identity
  version: 0.0.1
runtime:
  kind: wasmtime-component
  path: ./identity.wasm
  sha256: {sha_hex}
limits:
  memoryMaxBytes: 16777216
  timeoutMs: 100
",
    );
    let manifest_path = dir.path().join("identity.yaml");
    std::fs::write(&manifest_path, manifest_yaml).expect("write manifest");

    (dir, manifest_path)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn fixture_compiles() {
    let bytes = wat::parse_str(FIXTURE_WAT).expect("WAT must compile");
    // Component header: `\0asm` + version + layer (component layer = 1).
    assert_eq!(&bytes[0..4], b"\0asm");
}

#[test]
fn loads_descriptor_through_wasmtime() {
    let (_dir, manifest_path) = stage_identity_plugin();
    let host = PluginHost::new(PluginHostConfig {
        plugin_dirs: vec![manifest_path.parent().unwrap().to_path_buf()],
        ..PluginHostConfig::default()
    });

    let loaded = host
        .load_one(&manifest_path)
        .expect("load_one succeeds for a valid identity plugin");

    assert_eq!(loaded.descriptor.name, "identity");
    assert_eq!(loaded.descriptor.components.len(), 1);
    let c = &loaded.descriptor.components[0];
    assert_eq!(c.urn, "urn:otel:test:identity");

    // Phase-1 invariants the host enforces: at least one validator and
    // one runner per component.
    assert_eq!(loaded.validators.len(), 1);
    assert_eq!(loaded.runners.len(), 1);
    assert_eq!(loaded.fingerprints.len(), 1);
    assert_eq!(loaded.fingerprints[0].component_urn, c.urn);
}

#[test]
fn validate_config_accepts_arbitrary_json() {
    let (_dir, manifest_path) = stage_identity_plugin();
    let host = PluginHost::new(PluginHostConfig::default());
    let loaded = host.load_one(&manifest_path).unwrap();

    let (_urn, validator) = &loaded.validators[0];
    // Identity plugin always returns Ok; we are exercising the real
    // wasmtime-backed validator, not the Unimplemented stub.
    validator
        .validate(r#"{"any":"config"}"#)
        .expect("validate-config returns Ok");
}

#[test]
fn process_runs_identity_transform_through_wasmtime() {
    let (_dir, manifest_path) = stage_identity_plugin();
    let host = PluginHost::new(PluginHostConfig::default());
    let loaded = host.load_one(&manifest_path).unwrap();

    let (_urn, runner) = &loaded.runners[0];
    let payload: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03];
    let (class, out) = runner
        .process(
            /* signal = logs */ 0,
            PayloadKind::OtlpProtoBytes,
            payload,
            r#"{}"#,
            /* timeout_ms */ 1000,
        )
        .expect("identity transform succeeds");

    assert_eq!(class, result_class::OK);
    assert_eq!(
        out, payload,
        "identity transform must echo input bytes verbatim",
    );
}

#[test]
fn load_all_discovers_identity_plugin_via_directory_scan() {
    let (_dir, manifest_path) = stage_identity_plugin();
    let plugin_dir: &Path = manifest_path.parent().unwrap();

    let host = PluginHost::new(PluginHostConfig {
        plugin_dirs: vec![plugin_dir.to_path_buf()],
        ..PluginHostConfig::default()
    });
    let plugins = host.load_all().expect("directory scan succeeds");
    assert_eq!(plugins.len(), 1, "exactly one manifest in dir");
    assert_eq!(plugins[0].descriptor.name, "identity");
}

#[test]
fn sha_mismatch_is_rejected_for_identity_plugin() {
    let (dir, _good_manifest) = stage_identity_plugin();

    // Write a sibling manifest with a wrong sha; verify the host rejects.
    let bad_manifest_yaml = "apiVersion: otap.plugin/v1alpha1
kind: WasmPlugin
metadata:
  name: identity
  version: 0.0.1
runtime:
  kind: wasmtime-component
  path: ./identity.wasm
  sha256: 0000000000000000000000000000000000000000000000000000000000000000
limits:
  memoryMaxBytes: 16777216
  timeoutMs: 100
";
    let bad_path = dir.path().join("bad-sha.yaml");
    std::fs::write(&bad_path, bad_manifest_yaml).expect("write bad manifest");

    let host = PluginHost::new(PluginHostConfig::default());
    let res = host.load_one(&bad_path);
    let err = match res {
        Ok(_) => panic!("sha mismatch must be rejected"),
        Err(e) => e,
    };
    let msg = format!("{err:?}");
    assert!(
        msg.contains("sha") || msg.contains("Sha") || msg.contains("expected"),
        "expected sha-mismatch error, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Signature policy integration tests (real minisign verification through
// `PluginHost::load_one`).
// ---------------------------------------------------------------------------

/// Sign `artifact_bytes` with a freshly generated unencrypted minisign key,
/// writing the public key to `pk_path` and the signature to `sig_path`.
fn sign_artifact(pk_path: &Path, sig_path: &Path, artifact_bytes_for_sig: &[u8]) {
    let kp = minisign::KeyPair::generate_unencrypted_keypair()
        .expect("generate unencrypted minisign keypair");
    std::fs::write(pk_path, kp.pk.to_box().expect("encode pk box").to_string())
        .expect("write public key");
    let sig_box = minisign::sign(
        None,
        &kp.sk,
        std::io::Cursor::new(artifact_bytes_for_sig),
        None,
        None,
    )
    .expect("sign artifact bytes");
    std::fs::write(sig_path, sig_box.into_string()).expect("write signature");
}

/// Stage the identity plugin alongside a signing block in the manifest.
/// If `sign_with_matching_key` is `true`, a real signature for the staged
/// artifact is written next to it. If `false`, no signature file is
/// produced (so verification will fail).
fn stage_signed_identity_plugin(sign_with_matching_key: bool) -> (TempDir, std::path::PathBuf) {
    let component_bytes = wat::parse_str(FIXTURE_WAT).expect("WAT compiles to a component");
    let dir = TempDir::new().expect("tempdir");
    let wasm_path = dir.path().join("identity.wasm");
    std::fs::write(&wasm_path, &component_bytes).expect("write wasm fixture");

    let mut hasher = Sha256::new();
    hasher.update(&component_bytes);
    let sha_hex = hex_lower(&hasher.finalize());

    let pk_path = dir.path().join("identity.pub");
    let sig_path = dir.path().join("identity.wasm.minisig");
    if sign_with_matching_key {
        sign_artifact(&pk_path, &sig_path, &component_bytes);
    } else {
        // Generate a key + signature for *different* bytes so verification
        // fails deterministically.
        sign_artifact(&pk_path, &sig_path, b"unrelated-bytes");
    }

    let manifest_yaml = format!(
        "apiVersion: otap.plugin/v1alpha1
kind: WasmPlugin
metadata:
  name: identity
  version: 0.0.1
runtime:
  kind: wasmtime-component
  path: ./identity.wasm
  sha256: {sha_hex}
limits:
  memoryMaxBytes: 16777216
  timeoutMs: 100
signing:
  minisignPublicKeyPath: ./identity.pub
"
    );
    let manifest_path = dir.path().join("identity.yaml");
    std::fs::write(&manifest_path, manifest_yaml).expect("write manifest");

    (dir, manifest_path)
}

#[test]
fn host_accepts_plugin_with_valid_signature() {
    let (_dir, manifest_path) = stage_signed_identity_plugin(true);

    // Even with require_signed = true, a properly signed plugin must load.
    let host = PluginHost::new(PluginHostConfig {
        require_signed: true,
        ..PluginHostConfig::default()
    });
    let loaded = host
        .load_one(&manifest_path)
        .expect("signed plugin should load under require_signed=true");
    assert_eq!(loaded.descriptor.name, "identity");
}

#[test]
fn host_rejects_plugin_with_invalid_signature() {
    let (_dir, manifest_path) = stage_signed_identity_plugin(false);

    // Even with require_signed = false, a present-but-invalid signature
    // must still be rejected. We never silently accept a bad signature.
    let host = PluginHost::new(PluginHostConfig {
        require_signed: false,
        ..PluginHostConfig::default()
    });
    let res = host.load_one(&manifest_path);
    let err = match res {
        Ok(_) => panic!("invalid signature must be rejected"),
        Err(e) => e,
    };
    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("signature") || msg.to_lowercase().contains("verification"),
        "expected SignatureVerification error, got: {msg}"
    );
}

#[test]
fn host_rejects_unsigned_plugin_when_signing_required() {
    // Use the unsigned identity plugin (no signing block in manifest).
    let (_dir, manifest_path) = stage_identity_plugin();

    let host = PluginHost::new(PluginHostConfig {
        require_signed: true,
        ..PluginHostConfig::default()
    });
    let res = host.load_one(&manifest_path);
    let err = match res {
        Ok(_) => panic!("unsigned plugin must be rejected when host requires signing"),
        Err(e) => e,
    };
    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("signature"),
        "expected SignatureVerification error, got: {msg}"
    );
}

#[test]
fn host_accepts_unsigned_plugin_when_signing_not_required() {
    let (_dir, manifest_path) = stage_identity_plugin();

    let host = PluginHost::new(PluginHostConfig {
        require_signed: false,
        ..PluginHostConfig::default()
    });
    let _loaded = host
        .load_one(&manifest_path)
        .expect("unsigned plugin allowed when require_signed=false");
}
