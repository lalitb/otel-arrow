// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Reports artifact compatibility, host prerequisites, and memory estimates without BPF syscalls.

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use otel_arrow_dfe_upstream_ebpf_profiler_backend::{
        AMD64_COMPATIBILITY_MANIFEST, ARM64_COMPATIBILITY_MANIFEST, BackendConfig,
        CompatibilityManifest, CompatibilityValidator, MemoryEstimate, UpstreamArtifact,
        artifact::ArtifactArchitecture, linux::SystemProbe,
    };
    use std::io::Write;

    let mut arguments = std::env::args_os().skip(1);
    let path = arguments.next().ok_or("usage: preflight <object>")?;
    if arguments.next().is_some() {
        return Err("too many arguments".into());
    }
    let artifact = UpstreamArtifact::open(&path)?;
    let manifest = CompatibilityManifest::from_json(match artifact.inventory().architecture {
        ArtifactArchitecture::Amd64 => AMD64_COMPATIBILITY_MANIFEST,
        ArtifactArchitecture::Arm64 => ARM64_COMPATIBILITY_MANIFEST,
        ArtifactArchitecture::Unknown => return Err("unknown artifact architecture".into()),
    })?;
    let config = BackendConfig::new(
        path.into(),
        manifest.artifact.sha256,
        &manifest.compatibility_version,
    );
    config.validate()?;
    CompatibilityValidator::new(manifest)?
        .validate(&artifact, &config.expected_compatibility_version)?;
    let system = SystemProbe::collect(&config.procfs_root, &config.kernel_btf_path)?;
    let estimate = MemoryEstimate::for_config(&config, system.topology.cpu_slots());
    let report = serde_json::json!({
        "system": system,
        "limits": config.limits,
        "memory_estimate": estimate,
        "note": "Planning estimates, not a physical-memory or RSS guarantee. No BPF syscall was issued.",
    });
    let mut output = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &report)?;
    output.write_all(b"\n")?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("host preflight requires Linux; artifact inspection remains non-privileged".into())
}
