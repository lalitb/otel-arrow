// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Opt-in real-kernel test. Missing prerequisites skip; code failures do not.

#![allow(clippy::print_stderr)]
#![cfg(target_os = "linux")]

use otel_arrow_dfe_ebpf_profiler::{
    CpuSelection, Profiler, ProfilerBuilder, ProfilerConfig, ProgramConfig, ShardingStrategy,
};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Target(Child);

impl Drop for Target {
    fn drop(&mut self) {
        let _kill = self.0.kill();
        let _wait = self.0.wait();
    }
}

struct RunningProfiler(Profiler);

impl Drop for RunningProfiler {
    fn drop(&mut self) {
        let _cleanup = self.0.shutdown(Instant::now() + Duration::from_secs(30));
    }
}

fn bounded_text(path: &str) -> String {
    let file = File::open(path).expect("prerequisite file must be readable");
    let mut text = String::new();
    let _read = file
        .take(65537)
        .read_to_string(&mut text)
        .expect("prerequisite file must be UTF-8");
    assert!(text.len() <= 65536, "prerequisite file exceeds test bound");
    text
}

fn missing_prerequisite() -> Option<String> {
    if !matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
        return Some(format!(
            "unsupported architecture {}",
            std::env::consts::ARCH
        ));
    }
    let release = bounded_text("/proc/sys/kernel/osrelease");
    let mut parts = release.trim().split('.');
    let major = parts
        .next()
        .expect("kernel major")
        .parse::<u32>()
        .expect("kernel major");
    let minor = parts
        .next()
        .expect("kernel minor")
        .parse::<u32>()
        .expect("kernel minor");
    if (major, minor) < (5, 8) {
        return Some(format!("Linux >=5.8 required, found {}", release.trim()));
    }
    let status = bounded_text("/proc/self/status");
    let caps = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(str::trim)
        .expect("CapEff in proc status");
    let caps = u64::from_str_radix(caps, 16).expect("hex effective capabilities");
    let required = (1_u64 << 38) | (1_u64 << 39);
    if caps & (1_u64 << 21) == 0 && caps & required != required {
        return Some(format!(
            "requires CAP_BPF + CAP_PERFMON or CAP_SYS_ADMIN; CapEff={caps:016x}; perf_event_paranoid={}; unprivileged_bpf_disabled={}",
            bounded_text("/proc/sys/kernel/perf_event_paranoid").trim(),
            bounded_text("/proc/sys/kernel/unprivileged_bpf_disabled").trim(),
        ));
    }
    None
}

fn compile_workload(directory: &Path) -> PathBuf {
    if let Some(executable) = std::env::var_os("OTEL_EBPF_PROFILER_WORKLOAD") {
        let executable = PathBuf::from(executable);
        assert!(
            executable.is_file(),
            "prebuilt frame-pointer workload must be a regular executable file"
        );
        return executable;
    }
    let output = directory.join("native-workload");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ebpf/profiler/tests/workload.c");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let status = Command::new(compiler)
        .args([
            "-O2",
            "-g",
            "-fno-omit-frame-pointer",
            "-fno-optimize-sibling-calls",
            "-Wall",
            "-Werror",
        ])
        .arg(source)
        .arg("-o")
        .arg(&output)
        .status()
        .expect("compile frame-pointer workload");
    assert!(status.success(), "frame-pointer workload must compile");
    output
}

fn open_fd_count() -> usize {
    let mut count = 0;
    for entry in std::fs::read_dir("/proc/self/fd").expect("inspect FDs") {
        let _entry = entry.expect("read FD entry");
        count += 1;
        assert!(count <= 4096, "test FD budget exceeded");
    }
    count
}

/// Scenario: An authorized capable host samples a frame-pointer-enabled child.
/// Guarantees: That child's PID owns a nonempty USER stack, counters reconcile,
/// every cleanup path releases the child/profiler, and a second startup succeeds.
#[test]
fn real_kernel_sampling_smoke_test() {
    if std::env::var_os("OTEL_EBPF_PROFILER_SMOKE").as_deref() != Some("1".as_ref()) {
        eprintln!("skipped: set OTEL_EBPF_PROFILER_SMOKE=1 and OTEL_EBPF_PROFILER_OBJECT=<object>");
        return;
    }
    if let Some(reason) = missing_prerequisite() {
        eprintln!("skipped real-kernel smoke: {reason}");
        return;
    }
    let object = PathBuf::from(
        std::env::var_os("OTEL_EBPF_PROFILER_OBJECT")
            .expect("OTEL_EBPF_PROFILER_OBJECT must identify a separately built object"),
    );
    assert!(object.is_file(), "configured eBPF object must exist");
    let cpu = core_affinity::get_core_ids()
        .expect("allowed CPUs")
        .first()
        .expect("allowed CPU")
        .id;
    let cpu = u32::try_from(cpu).expect("CPU ID");
    let directory = tempfile::tempdir_in(".").expect("workload build directory");
    let executable = compile_workload(directory.path());
    let child = Command::new("taskset")
        .args(["--cpu-list", &cpu.to_string()])
        .arg(executable)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("start pinned workload");
    let mut target = Target(child);
    let pid = target.0.id();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        target.0.try_wait().expect("check workload").is_none(),
        "workload must stay alive"
    );
    let baseline_fds = open_fd_count();
    let config = ProfilerConfig {
        cpu_selection: CpuSelection::Include(vec![cpu]),
        sharding: ShardingStrategy::Single,
        reporting_interval: Duration::from_secs(1),
        program: ProgramConfig {
            object_path: Some(object),
            ..ProgramConfig::default()
        },
        ..ProfilerConfig::default()
    };
    let (profiler, snapshots) = ProfilerBuilder::new(config.clone())
        .start()
        .expect("capable host must load and attach; verifier/map/attach errors are failures");
    let profiler = RunningProfiler(profiler);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found_target_user_stack = false;
    while Instant::now() < deadline {
        let snapshot = snapshots
            .recv_timeout(Duration::from_secs(2))
            .expect("capable host must produce a snapshot");
        snapshot.validate().expect("snapshot graph");
        let represented = snapshot
            .samples
            .iter()
            .map(|sample| sample.count)
            .sum::<u64>();
        assert_eq!(represented, snapshot.statistics.samples_in_snapshots);
        found_target_user_stack |= snapshot.samples.iter().any(|sample| {
            let process = &snapshot.processes[sample.process.0 as usize];
            let stack = &snapshot.stacks[sample.user_stack.0 as usize];
            process.pid == pid
                && !stack.kernel
                && !stack.locations.is_empty()
                && stack.process == Some(sample.process)
                && stack
                    .locations
                    .iter()
                    .all(|location| !snapshot.locations[location.0 as usize].kernel)
        });
        if found_target_user_stack {
            break;
        }
    }
    let report = profiler
        .0
        .shutdown(Instant::now() + Duration::from_secs(10))
        .expect("real profiler shutdown");
    assert!(report.cleanup_complete);
    assert!(
        report.failure.is_none(),
        "worker failure: {:?}",
        report.failure
    );
    assert!(
        found_target_user_stack,
        "the frame-pointer CHILD must have a nonempty USER stack"
    );
    let stats = &report.statistics;
    assert!(stats.sampling_periods_attempted > 0);
    assert_eq!(stats.attached_cpus, 1);
    assert_eq!(stats.unavailable_cpus, 0);
    assert_eq!(
        stats.sampling_periods_attempted - stats.idle_samples_skipped,
        stats.raw_events_received + stats.unobserved_samples.expect("native kernel counters")
    );
    assert!(stats.kernel_output_failures <= stats.unobserved_samples.expect("native kernel loss"));
    let again = profiler
        .0
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("idempotent second shutdown");
    assert_eq!(again.statistics, report.statistics);
    assert_eq!(
        open_fd_count(),
        baseline_fds,
        "first shutdown must release FDs"
    );
    let mut kernel_config = config;
    kernel_config.include_kernel_stacks = true;
    let (second, second_snapshots) = ProfilerBuilder::new(kernel_config)
        .start()
        .expect("second startup loads the user-plus-kernel program");
    let second = RunningProfiler(second);
    let snapshot = second_snapshots
        .recv_timeout(Duration::from_secs(3))
        .expect("user-plus-kernel program emits a real window");
    snapshot
        .validate()
        .expect("user-plus-kernel snapshot graph");
    assert!(snapshot.statistics.raw_events_received > 0);
    for sample in &snapshot.samples {
        if let Some(kernel) = sample.kernel_stack {
            assert!(snapshot.stacks[kernel.0 as usize].kernel);
        }
    }
    let report = second
        .0
        .shutdown(Instant::now() + Duration::from_secs(10))
        .expect("second cleanup");
    assert!(report.cleanup_complete);
    assert_eq!(
        open_fd_count(),
        baseline_fds,
        "second shutdown must release FDs"
    );
}
