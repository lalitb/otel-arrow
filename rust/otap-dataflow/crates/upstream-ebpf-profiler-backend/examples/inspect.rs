// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Prints a deterministic compatibility manifest for an external artifact.

use std::io::Write;

use otel_arrow_dfe_upstream_ebpf_profiler_backend::{
    AMD64_COMPATIBILITY_MANIFEST, ARM64_COMPATIBILITY_MANIFEST, CompatibilityManifest,
    CompatibilityValidator, PINNED_UPSTREAM_COMMIT, UpstreamArtifact,
    artifact::ArtifactArchitecture,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: inspect <object> [compatibility-version] [upstream-commit] [--check]")?;
    let version = args
        .next()
        .unwrap_or_else(|| "upstream-06ea040c-amd64-v1".to_owned());
    let commit = args
        .next()
        .unwrap_or_else(|| PINNED_UPSTREAM_COMMIT.to_owned());
    let check = match args.next().as_deref() {
        None => false,
        Some("--check") => true,
        Some(_) => return Err("expected --check after the upstream commit".into()),
    };
    if args.next().is_some() {
        return Err("too many arguments".into());
    }

    let artifact = UpstreamArtifact::open(path)?;
    if check {
        let pinned = match artifact.inventory().architecture {
            ArtifactArchitecture::Amd64 => AMD64_COMPATIBILITY_MANIFEST,
            ArtifactArchitecture::Arm64 => ARM64_COMPATIBILITY_MANIFEST,
            ArtifactArchitecture::Unknown => return Err("unknown artifact architecture".into()),
        };
        let manifest = CompatibilityManifest::from_json(pinned)?;
        if commit != manifest.upstream_commit {
            return Err("the supplied upstream commit does not match the pinned manifest".into());
        }
        CompatibilityValidator::new(manifest)?.validate(&artifact, &version)?;
    }
    let manifest =
        CompatibilityManifest::from_inventory(version, commit, artifact.inventory().clone());
    std::io::stdout()
        .lock()
        .write_all(manifest.to_pretty_json()?.as_bytes())?;
    Ok(())
}
