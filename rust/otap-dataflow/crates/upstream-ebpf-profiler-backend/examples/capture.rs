// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Captures one native-only bounded reporting window.

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use otel_arrow_dfe_upstream_ebpf_profiler_backend::native::NativeSession;
    use otel_arrow_dfe_upstream_ebpf_profiler_backend::{
        AMD64_COMPATIBILITY_MANIFEST, BackendConfig, CompatibilityManifest, CpuSelection,
        PidNamespaceMode,
    };

    struct CaptureArguments {
        object_path: PathBuf,
        duration: Duration,
        cpus: Vec<u32>,
        namespace: PidNamespaceMode,
    }

    fn parse_arguments(
        mut args: impl Iterator<Item = String>,
    ) -> Result<CaptureArguments, Box<dyn std::error::Error>> {
        let object_path = PathBuf::from(
            args.next()
                .ok_or("usage: capture <object> [duration-seconds] [cpu-ids] [host|current]")?,
        );
        let duration = Duration::from_secs(
            args.next()
                .unwrap_or_else(|| "10".to_owned())
                .parse::<u64>()?,
        );
        let cpu_ids = args.next().unwrap_or_else(|| "0".to_owned());
        let mut cpus = Vec::new();
        for cpu in cpu_ids.split(',') {
            if cpus.len() == 16 {
                return Err("capture example supports at most 16 CPUs".into());
            }
            cpus.push(cpu.parse::<u32>()?);
        }
        let namespace = match args.next().as_deref() {
            None | Some("host") => PidNamespaceMode::Host,
            Some("current") => PidNamespaceMode::Current,
            Some(_) => return Err("PID namespace must be host or current".into()),
        };
        if args.next().is_some() {
            return Err("too many arguments".into());
        }
        Ok(CaptureArguments {
            object_path,
            duration,
            cpus,
            namespace,
        })
    }

    pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
        let arguments = parse_arguments(std::env::args().skip(1))?;
        let diagnostics = match std::env::var("OTEL_ARROW_EBPF_CAPTURE_DIAGNOSTICS") {
            Ok(value) if value == "1" => true,
            Ok(value) if value == "0" => false,
            Err(std::env::VarError::NotPresent) => false,
            _ => return Err("OTEL_ARROW_EBPF_CAPTURE_DIAGNOSTICS must be 0 or 1".into()),
        };
        let manifest = CompatibilityManifest::from_json(AMD64_COMPATIBILITY_MANIFEST)?;
        let mut config = BackendConfig::new(
            arguments.object_path,
            manifest.artifact.sha256,
            manifest.compatibility_version.clone(),
        );
        config.cpus = CpuSelection::List(arguments.cpus);
        config.pid_namespace = arguments.namespace;
        match std::env::var("OTEL_ARROW_EBPF_EVENT_POLL_MS") {
            Ok(value) => config.event_poll_interval = Duration::from_millis(value.parse::<u64>()?),
            Err(std::env::VarError::NotPresent) => {}
            Err(error) => return Err(error.into()),
        }
        let event_poll_interval_ms = config.event_poll_interval.as_millis();
        let started = Instant::now();
        let mut session = NativeSession::start(config, manifest)?;
        let startup_ns = started.elapsed().as_nanos();
        let selected_cpus = session.selected_cpus().to_vec();
        let capturing = Instant::now();
        let snapshot = session.capture_for(arguments.duration)?;
        let capture_ns = capturing.elapsed().as_nanos();
        let stopping = Instant::now();
        let remaining = session.shutdown(Duration::from_secs(2))?;
        let shutdown_ns = stopping.elapsed().as_nanos();
        if diagnostics {
            let report = serde_json::json!({
                "startup_ns": startup_ns,
                "capture_ns": capture_ns,
                "shutdown_ns": shutdown_ns,
                "selected_cpus": selected_cpus,
                "pid_namespace": arguments.namespace,
                "event_poll_interval_ms": event_poll_interval_ms,
                "memory_estimate": session.memory_estimate(),
                "kernel_stack_layout": session.kernel_stack_layout(),
                "error_details": session.error_details(),
            });
            let mut stderr = std::io::stderr().lock();
            serde_json::to_writer(&mut stderr, &report)?;
            stderr.write_all(b"\n")?;
        }
        let mut stdout = std::io::stdout().lock();
        for snapshot in std::iter::once(snapshot).chain(remaining) {
            serde_json::to_writer(&mut stdout, &snapshot)?;
            stdout.write_all(b"\n")?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Scenario: The documented capture command requests ten seconds on CPU zero.
        /// Guarantees: Duration and CPU are parsed in the advertised order without loading BPF.
        #[test]
        fn duration_precedes_cpu() {
            let arguments =
                parse_arguments(["/artifact.o", "10", "0"].into_iter().map(str::to_owned))
                    .expect("capture arguments");
            assert_eq!(arguments.duration, Duration::from_secs(10));
            assert_eq!(arguments.cpus, [0]);
            assert_eq!(arguments.namespace, PidNamespaceMode::Host);
        }

        /// Scenario: The capture command provides only an artifact path or an extra argument.
        /// Guarantees: Defaults are ten seconds on CPU zero and surplus input is rejected.
        #[test]
        fn defaults_and_surplus_arguments_are_checked() {
            let arguments = parse_arguments(["/artifact.o"].into_iter().map(str::to_owned))
                .expect("default arguments");
            assert_eq!(arguments.duration, Duration::from_secs(10));
            assert_eq!(arguments.cpus, [0]);
            assert_eq!(arguments.namespace, PidNamespaceMode::Host);
            assert!(
                parse_arguments(
                    ["/artifact.o", "10", "0", "extra"]
                        .into_iter()
                        .map(str::to_owned),
                )
                .is_err()
            );
        }

        /// Scenario: A capture explicitly selects its current container PID namespace.
        /// Guarantees: The parsed mode requests namespace filtering instead of silently using host PIDs.
        #[test]
        fn current_namespace_is_explicit() {
            let arguments = parse_arguments(
                ["/artifact.o", "3", "0", "current"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .expect("container arguments");
            assert_eq!(
                (arguments.duration, arguments.namespace),
                (Duration::from_secs(3), PidNamespaceMode::Current)
            );
            assert_eq!(arguments.cpus, [0]);
        }

        /// Scenario: A capture selects two CPUs using the comma-separated CLI syntax.
        /// Guarantees: Both CPU IDs reach typed configuration and malformed lists are rejected.
        #[test]
        fn multiple_cpus_are_parsed() {
            let arguments = parse_arguments(
                ["/artifact.o", "3", "0,1", "current"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .expect("multi-CPU arguments");
            assert_eq!(arguments.cpus, [0, 1]);
            assert!(
                parse_arguments(["/artifact.o", "3", "0,,1"].into_iter().map(str::to_owned),)
                    .is_err()
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("native eBPF capture requires Linux".into())
}
