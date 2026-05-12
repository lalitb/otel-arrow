// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! End-to-end native plugin test.
//!
//! Builds the `drop_logs_by_service` sample cdylib, computes its
//! SHA-256, writes a manifest, and exercises the loader + adapter.
//!
//! Asserts:
//!   1. Manifest discovery + SHA-256 verification work.
//!   2. Descriptor + `validate_config` succeed.
//!   3. Plugin returns `ForwardSame` for non-matching service names —
//!      and the adapter forwards the original `OtapPdata` (proven via
//!      pointer-equality on the inner `Bytes` payload, which is
//!      reference-counted Arc-shared, so equality of the raw data
//!      pointer means no serialization happened).
//!   4. Plugin returns `Drop` for matching service names.
//!   5. Plugin returns `Error` (with message) when configured to.
//!   6. Plugin fingerprint + cache key are populated for use in
//!      live-reconfig comparisons.

use std::path::PathBuf;
use std::process::Command;

use bytes::Bytes;
use otap_df_otap::pdata::OtapPdata;
use otap_df_pdata::OtapPayload;
use otap_df_pdata::OtlpProtoBytes;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceRequest;
use otap_df_pdata::proto::opentelemetry::common::v1::{AnyValue, KeyValue, any_value};
use otap_df_pdata::proto::opentelemetry::logs::v1::ResourceLogs;
use otap_df_pdata::proto::opentelemetry::resource::v1::Resource;
use otap_df_plugin_native_host::runner::{NativeProcessorRunnerImpl, PluginInstanceHandle};
use otap_df_plugin_native_host::{NativePluginHost, NativePluginHostConfig};
use otap_df_plugin_native_nodes::adapter::NativeProcessorAdapter;
use prost::Message;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Pin the fixture's cargo target dir to a path inside the fixture
/// so the test does not interact with ambient `CARGO_TARGET_DIR`,
/// the workspace `target/`, or parallel test runs.
fn fixture_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/drop_logs_by_service/target_for_tests")
}

fn cdylib_path() -> PathBuf {
    let p = fixture_target_dir().join("release");
    if cfg!(target_os = "macos") {
        p.join("libdrop_logs_by_service.dylib")
    } else if cfg!(target_os = "windows") {
        p.join("drop_logs_by_service.dll")
    } else {
        p.join("libdrop_logs_by_service.so")
    }
}

fn ensure_sample_built() -> PathBuf {
    let cdylib = cdylib_path();
    if cdylib.exists() {
        return cdylib;
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let plugin_dir = PathBuf::from(manifest_dir).join("tests/fixtures/drop_logs_by_service");
    let target_dir = fixture_target_dir();
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "--target-dir"])
        .arg(&target_dir)
        // Strip ambient CARGO_TARGET_DIR so it cannot override the
        // explicit `--target-dir` we pass; otherwise CI matrices that
        // set the env var would land the cdylib elsewhere and the
        // subsequent `cdylib.exists()` check would fail.
        .env_remove("CARGO_TARGET_DIR")
        .current_dir(&plugin_dir)
        .status()
        .expect("invoke cargo build");
    assert!(status.success(), "cargo build failed for sample plugin");
    assert!(
        cdylib.exists(),
        "cdylib not produced at {}",
        cdylib.display()
    );
    cdylib
}

fn sha256_hex(path: &PathBuf) -> String {
    let bytes = std::fs::read(path).expect("read cdylib");
    let mut h = Sha256::new();
    h.update(&bytes);
    let digest = h.finalize();
    data_encoding_hex_lower(&digest)
}

// Tiny hex encoder (no extra crate dep just for one call).
fn data_encoding_hex_lower(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0xf) as usize] as char);
    }
    s
}

fn write_manifest(dir: &std::path::Path, cdylib: &std::path::Path, sha: &str) -> PathBuf {
    let manifest = format!(
        r#"apiVersion: otap.plugin/v1alpha1
kind: NativePlugin
metadata:
  name: drop_logs_by_service
  version: 0.1.0
runtime:
  kind: native-cdylib
  path: {path}
  sha256: {sha}
"#,
        path = cdylib.display(),
        sha = sha,
    );
    let manifest_path = dir.join("plugin.yaml");
    std::fs::write(&manifest_path, manifest).expect("write manifest");
    manifest_path
}

fn make_logs_pdata(service_name: &str) -> OtapPdata {
    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue(service_name.to_string())),
                    }),
                }],
                dropped_attributes_count: 0,
                entity_refs: vec![],
            }),
            scope_logs: vec![],
            schema_url: String::new(),
        }],
    };
    let mut buf = Vec::new();
    req.encode(&mut buf).expect("encode otlp logs");
    let payload: OtapPayload = OtlpProtoBytes::ExportLogsRequest(Bytes::from(buf)).into();
    OtapPdata::new_todo_context(payload)
}

/// Extract the raw data pointer of the inner `Bytes`. Used to assert
/// that ForwardSame really forwards the original buffer rather than a
/// re-encoded copy.
fn bytes_data_ptr(p: &OtapPayload) -> *const u8 {
    match p {
        OtapPayload::OtlpBytes(OtlpProtoBytes::ExportLogsRequest(b))
        | OtapPayload::OtlpBytes(OtlpProtoBytes::ExportMetricsRequest(b))
        | OtapPayload::OtlpBytes(OtlpProtoBytes::ExportTracesRequest(b)) => b.as_ptr(),
        OtapPayload::OtapArrowRecords(_) => std::ptr::null(),
    }
}

#[test]
fn end_to_end_native_plugin_loads_descriptor_and_returns_verbs() {
    let cdylib = ensure_sample_built();
    let sha = sha256_hex(&cdylib);
    let tmp = tempfile::tempdir().expect("tempdir");
    let _manifest_path = write_manifest(tmp.path(), &cdylib, &sha);

    // (1) Manifest discovery + SHA-256 + descriptor verification.
    let host = NativePluginHost::new(NativePluginHostConfig {
        plugin_dirs: vec![tmp.path().to_path_buf()],
        require_signed: false,
    });
    let plugins = host.load_all().expect("load_all");
    assert_eq!(plugins.len(), 1, "expected one native plugin loaded");
    let plugin = &plugins[0];
    assert_eq!(plugin.descriptor.name, "drop_logs_by_service");
    assert_eq!(plugin.descriptor.version, "0.1.0");
    assert_eq!(plugin.descriptor.components.len(), 1);
    let component = &plugin.descriptor.components[0];
    assert_eq!(component.urn, "urn:example:processor:drop_logs_by_service");
    assert_eq!(plugin.cache_key.artifact_sha256, sha.to_lowercase());
    assert_eq!(
        plugin.fingerprints.len(),
        1,
        "fingerprint per component participates in live-reconfig Replace"
    );
    assert_eq!(plugin.fingerprints[0].artifact_sha256, sha.to_lowercase());

    // (2) validate_config — accept good config, reject bad config.
    let good_cfg = r#"{"drop_when_service_name_eq": "debug-service"}"#;
    let bad_cfg = r#"{"unrelated": true}"#;
    let validator = plugin.validator(&component.urn).expect("validator wired");
    validator.validate(good_cfg).expect("good config accepted");
    let err = validator
        .validate(bad_cfg)
        .expect_err("bad config rejected");
    assert!(
        err.contains("invalid config") || err.contains("invalid"),
        "rejection message: {err}"
    );

    // (3) Build a per-node runner via the same pathway the registry
    // bridge uses, and exercise ForwardSame / Drop / Error verbs.
    let inst_drop_path =
        PluginInstanceHandle::new(Arc::clone(&plugin.library), &component.urn, good_cfg)
            .expect("instance_new ok");
    let runner_drop = Arc::new(NativeProcessorRunnerImpl::new(inst_drop_path));
    let adapter_drop = NativeProcessorAdapter {
        component_urn: Arc::from(component.urn.as_str()),
        fingerprint: plugin.fingerprints[0].clone(),
        cache_key: plugin.cache_key.clone(),
        config_json: good_cfg.to_string(),
        runner: runner_drop,
        node_id: otap_df_engine::testing::node::test_node("drop-node"),
    };

    // (3a) Non-matching service.name → ForwardSame.
    let pdata = make_logs_pdata("normal-service");
    let original_ptr = bytes_data_ptr(pdata.payload_view());
    let out = adapter_drop
        .dispatch(pdata)
        .expect("dispatch ForwardSame ok");
    let forwarded = out.expect("ForwardSame should produce Some");
    let forwarded_ptr = bytes_data_ptr(forwarded.payload_view());
    assert_eq!(
        original_ptr, forwarded_ptr,
        "ForwardSame must forward the original Bytes buffer (zero-copy: same data pointer)"
    );

    // (3b) Matching service.name → Drop.
    let pdata = make_logs_pdata("debug-service");
    let out = adapter_drop.dispatch(pdata).expect("dispatch Drop ok");
    assert!(
        out.is_none(),
        "Drop verb must yield None (no message emitted)"
    );

    // (3c) Build a second instance with `error_on_match=true` so a
    // matching pdata returns the Error verb.
    let err_cfg = r#"{"drop_when_service_name_eq": "debug-service", "error_on_match": true}"#;
    let inst_err = PluginInstanceHandle::new(Arc::clone(&plugin.library), &component.urn, err_cfg)
        .expect("instance_new ok (error config)");
    let adapter_err = NativeProcessorAdapter {
        component_urn: Arc::from(component.urn.as_str()),
        fingerprint: plugin.fingerprints[0].clone(),
        cache_key: plugin.cache_key.clone(),
        config_json: err_cfg.to_string(),
        runner: Arc::new(NativeProcessorRunnerImpl::new(inst_err)),
        node_id: otap_df_engine::testing::node::test_node("error-node"),
    };
    let pdata = make_logs_pdata("debug-service");
    let err = adapter_err
        .dispatch(pdata)
        .expect_err("Error verb must surface as ProcessorError");
    let err_msg = format!("{err}");
    assert!(
        err_msg.contains("error_on_match=true") || err_msg.contains("debug-service"),
        "Error verb message lost: {err_msg}"
    );

    // (3d) Non-matching pdata under the error-config still ForwardSame.
    let pdata = make_logs_pdata("normal-service");
    let out = adapter_err.dispatch(pdata).expect("dispatch ok");
    assert!(out.is_some(), "non-matching pdata must still forward");
}

#[test]
fn registry_bridge_produces_phase1_processor_entry() {
    let cdylib = ensure_sample_built();
    let sha = sha256_hex(&cdylib);
    let tmp = tempfile::tempdir().expect("tempdir");
    let _ = write_manifest(tmp.path(), &cdylib, &sha);
    let host = NativePluginHost::new(NativePluginHostConfig {
        plugin_dirs: vec![tmp.path().to_path_buf()],
        require_signed: false,
    });
    let plugins = host.load_all().expect("load_all");
    let registry =
        otap_df_plugin_native_nodes::build_native_registry(&plugins).expect("build registry");
    // Should contain exactly one processor URN; no exporters.
    let proc_urns: Vec<_> = registry.processors().map(|e| e.urn.to_string()).collect();
    assert_eq!(
        proc_urns,
        vec!["urn:example:processor:drop_logs_by_service".to_string()]
    );
    // Fingerprint participates in live-reconfig identity.
    let entry = registry
        .processor("urn:example:processor:drop_logs_by_service")
        .expect("entry lookup");
    assert_eq!(entry.fingerprint.artifact_sha256, sha.to_lowercase());
    assert!(entry.factory.is_some(), "native factory wired");
}

#[test]
fn fingerprint_change_triggers_replace_via_planning_equality() {
    // This test pins the *comparison primitive* the live-control
    // planner relies on. It does NOT exercise
    // `collect_plugin_fingerprints` (`crates/controller/src/live_control/planning.rs`)
    // end-to-end and does NOT drive the planner to emit
    // `RolloutAction::Replace`.
    //
    // Specifically, it proves:
    //   (1) native plugins populate `fingerprint.artifact_sha256` on
    //       the `DynamicProcessorEntry` from the manifest sha;
    //   (2) the `DynamicNodeFingerprint` equality the planner builds
    //       its vectors from compares unequal when the sha changes
    //       (and when `plugin_version` changes).
    //
    // Full `RolloutAction::Replace` integration is a follow-up; the
    // pre-existing `crates/controller` `plugin_fingerprint_tests`
    // already exercise the planner against the same
    // `DynamicComponentRegistry` shape native plugins populate.

    let cdylib = ensure_sample_built();
    let real_sha = sha256_hex(&cdylib);
    let tmp = tempfile::tempdir().expect("tempdir");
    let _ = write_manifest(tmp.path(), &cdylib, &real_sha);

    let host = NativePluginHost::new(NativePluginHostConfig {
        plugin_dirs: vec![tmp.path().to_path_buf()],
        require_signed: false,
    });
    let plugins = host.load_all().expect("load_all");
    let registry_old =
        otap_df_plugin_native_nodes::build_native_registry(&plugins).expect("registry old");

    // Snapshot the URN's fingerprint as the planner would.
    let urn = "urn:example:processor:drop_logs_by_service";
    let entry_old = registry_old
        .processor(urn)
        .expect("native processor entry present");
    let fp_old = entry_old.fingerprint.clone();
    assert_eq!(fp_old.artifact_sha256, real_sha.to_lowercase());

    // Synthesize a registry that mimics the same plugin loaded from a
    // *different* artifact (hence a different sha) by rebuilding the
    // entry with a mutated `DynamicNodeFingerprint`. This is exactly
    // what would happen at runtime if `--plugin-dir` were repointed at
    // an upgraded artifact and the registry were rebuilt at rollout
    // request time.
    use otap_df_engine::runtime_registry::{
        ComponentUrn, ConfigValidator, DynamicComponentRegistry, DynamicNodeFingerprint,
        DynamicProcessorEntry,
    };
    use otap_df_engine::wiring_contract::WiringContract;

    let mut fp_new = fp_old.clone();
    fp_new.artifact_sha256 = "0".repeat(64);
    let mut registry_new = DynamicComponentRegistry::<OtapPdata>::empty();
    registry_new
        .register_processor(DynamicProcessorEntry {
            urn: ComponentUrn::from(urn),
            validator: ConfigValidator::Static(|_| Ok(())),
            fingerprint: fp_new.clone(),
            wiring_contract: WiringContract::UNRESTRICTED,
            factory: entry_old.factory.clone(),
        })
        .expect("register synthetic");

    // The planner compares per-URN fingerprints via equality. Mirror
    // that comparison here.
    let entry_new = registry_new.processor(urn).expect("synthetic entry");
    let planner_fps_old: Vec<&DynamicNodeFingerprint> = vec![&entry_old.fingerprint];
    let planner_fps_new: Vec<&DynamicNodeFingerprint> = vec![&entry_new.fingerprint];
    assert_ne!(
        planner_fps_old, planner_fps_new,
        "DynamicNodeFingerprint vectors must differ when artifact sha256 changes \
         — this is the equality check that drives RolloutAction::Replace"
    );

    // And mutating only the plugin_version field also breaks equality
    // (the planner treats any field change as a rollout-relevant change).
    let mut fp_ver = fp_old.clone();
    fp_ver.plugin_version = "9.9.9".to_string();
    assert_ne!(
        fp_old, fp_ver,
        "plugin_version is part of fingerprint identity"
    );
}
