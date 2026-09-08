// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Reports a pinned preparation policy without writing or loading an object.

use std::io::Write;

use otel_arrow_dfe_upstream_ebpf_profiler_backend::{PreparedObject, UpstreamArtifact};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments
        .next()
        .ok_or("usage: prepare <pinned-object> [native|analysis]")?;
    let policy = arguments.next();
    if arguments.next().is_some() {
        return Err("too many arguments".into());
    }
    let artifact = UpstreamArtifact::open(path)?;
    let policy = policy
        .as_ref()
        .map(|value| value.to_str().ok_or("preparation policy must be UTF-8"))
        .transpose()?
        .unwrap_or("native");
    let prepared = match policy {
        "native" => PreparedObject::native_probe_maps(&artifact)?,
        "analysis" => PreparedObject::thread_scoped_analysis(&artifact)?,
        _ => return Err("preparation policy must be native or analysis".into()),
    };
    let mut output = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, prepared.report())?;
    output.write_all(b"\n")?;
    Ok(())
}
