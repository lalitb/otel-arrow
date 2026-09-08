// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Optional independent C-compiler evidence from external authoritative headers.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::process::Command;

/// Scenario: The pinned upstream source and a C compiler are explicitly supplied.
/// Guarantees: Authoritative C sizes and offsets match the Rust wire contract without BPF privileges.
#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[ignore = "requires UPSTREAM_EBPF_SOURCE at the pinned commit and a C compiler"]
fn authoritative_c_layout_matches_rust() {
    use otel_arrow_dfe_upstream_ebpf_profiler_backend::{
        PINNED_UPSTREAM_COMMIT,
        abi::{TRACE_MAX_SIZE, TRACE_PREFIX_SIZE},
    };

    let source = std::path::PathBuf::from(
        std::env::var_os("UPSTREAM_EBPF_SOURCE").expect("UPSTREAM_EBPF_SOURCE"),
    );
    let revision = Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("read upstream HEAD");
    assert!(revision.status.success());
    assert_eq!(
        String::from_utf8(revision.stdout).expect("revision").trim(),
        PINNED_UPSTREAM_COMMIT
    );

    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let binary = directory.path().join("c-abi-layout");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let output = Command::new(compiler)
        .current_dir(directory.path())
        .args(["-std=gnu17", "-O0", "-I"])
        .arg(source.join("support/ebpf"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/c_abi_layout.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("run C compiler");
    assert!(
        output.status.success(),
        "C fixture compilation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(&binary).output().expect("execute C fixture");
    assert!(output.status.success());
    let actual: Vec<usize> = String::from_utf8(output.stdout)
        .expect("C numeric output")
        .split_whitespace()
        .map(|number| number.parse().expect("numeric size or offset"))
        .collect();
    assert_eq!(
        actual,
        [
            TRACE_MAX_SIZE,
            TRACE_PREFIX_SIZE,
            700,
            702,
            704,
            706,
            712,
            720,
            4,
            16,
            8,
            16,
            16,
            144,
            16
        ],
    );
}
