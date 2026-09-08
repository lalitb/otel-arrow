// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Explicitly environment-gated privileged native smoke test.

#[cfg(all(feature = "native", target_os = "linux"))]
mod linux {
    use std::path::PathBuf;
    use std::time::Duration;

    use otel_arrow_dfe_upstream_ebpf_profiler_backend::native::NativeSession;
    use otel_arrow_dfe_upstream_ebpf_profiler_backend::{
        AMD64_COMPATIBILITY_MANIFEST, BackendConfig, CompatibilityManifest,
    };

    /// Scenario: The environment explicitly enables privileged testing and
    /// supplies the pinned upstream object path.
    /// Guarantees: The native loader initializes maps and tail calls, attaches
    /// one selected CPU, drains a real window, and performs complete shutdown.
    #[test]
    #[ignore = "requires explicitly authorized BPF/perf capabilities and the external artifact"]
    fn privileged_one_cpu_capture() {
        assert_eq!(
            std::env::var("OTEL_ARROW_EBPF_PRIVILEGED_TEST").as_deref(),
            Ok("1"),
            "explicit privileged-test opt-in is required"
        );
        let object_path = std::env::var_os("UPSTREAM_EBPF_OBJECT")
            .map(PathBuf::from)
            .expect("UPSTREAM_EBPF_OBJECT is required");
        let manifest = CompatibilityManifest::from_json(AMD64_COMPATIBILITY_MANIFEST)
            .expect("checked-in manifest");
        let mut config = BackendConfig::new(
            object_path,
            manifest.artifact.sha256,
            manifest.compatibility_version.clone(),
        );
        config.cpus = otel_arrow_dfe_upstream_ebpf_profiler_backend::CpuSelection::List(vec![0]);
        config.reporting_interval = Duration::from_secs(1);
        let mut session = NativeSession::start(config, manifest).expect("native startup");
        let snapshot = session
            .capture_for(Duration::from_secs(1))
            .expect("capture window");
        assert!(
            snapshot.statistics.raw_events > 0,
            "a successful attachment alone is not evidence of sampling"
        );
        assert!(
            snapshot.samples.iter().any(|sample| {
                sample
                    .user_frames
                    .iter()
                    .filter(|frame| {
                        frame.kind
                            == otel_arrow_dfe_upstream_ebpf_profiler_backend::abi::FrameKind::Native
                            && !frame.flags.is_error()
                    })
                    .count()
                    >= 2
            }),
            "at least one meaningful native user stack is required"
        );
        let _final = session.shutdown(Duration::from_secs(1)).expect("shutdown");
        assert!(
            session
                .shutdown(Duration::ZERO)
                .expect("repeated shutdown")
                .is_empty()
        );
    }
}
