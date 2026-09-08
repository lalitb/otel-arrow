#!/usr/bin/env python3
# Copyright The OpenTelemetry Authors
# SPDX-License-Identifier: Apache-2.0

"""Reproducible opt-in comparisons of already-built profiler executables.

No shell command strings are evaluated. Output contains summaries, not host
stacks. Results are local artifacts and are never uploaded.
"""

import argparse
from collections import deque
import datetime
import json
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import time


WORKSPACE = Path(__file__).resolve().parents[3]
MAX_PROC_BYTES = 1024 * 1024
MAX_THREADS = 512


def command_array(value):
    """Parse explicit argv arrays instead of executing interpolated shell text."""
    try:
        argv = json.loads(value)
    except json.JSONDecodeError as error:
        raise argparse.ArgumentTypeError(str(error)) from error
    if not isinstance(argv, list) or not argv or len(argv) > 128:
        raise argparse.ArgumentTypeError("expected a nonempty JSON argv array (at most 128 entries)")
    if any(not isinstance(arg, str) or len(arg) > 4096 for arg in argv):
        raise argparse.ArgumentTypeError("argv entries must be strings of at most 4096 characters")
    return argv


def positive_duration(value):
    seconds = float(value)
    if not 1 <= seconds <= 3600:
        raise argparse.ArgumentTypeError("duration must be 1 through 3600 seconds")
    return seconds


def read_proc(path, maximum=MAX_PROC_BYTES):
    """Return bounded optional procfs text, distinguishing exit/denied reads."""
    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            value = handle.read(maximum + 1)
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return None, False
    return value[:maximum], len(value) > maximum


def rss_kib(pid):
    value, truncated = read_proc(Path(f"/proc/{pid}/status"), 64 * 1024)
    if value is None:
        return None
    for line in value.splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1])
    if truncated:
        raise RuntimeError("process status exceeded bounded read")
    return None


def cpu_seconds(pid):
    value, truncated = read_proc(Path(f"/proc/{pid}/stat"), 64 * 1024)
    if value is None or truncated:
        return None
    _, separator, fields = value.rpartition(")")
    if not separator:
        return None
    values = fields.split()
    if len(values) < 13:
        return None
    return (int(values[11]) + int(values[12])) / os.sysconf("SC_CLK_TCK")


def migrations(pid):
    result = {}
    limited = False
    try:
        with os.scandir(f"/proc/{pid}/task") as entries:
            for index, entry in enumerate(entries):
                if index >= MAX_THREADS:
                    limited = True
                    break
                value, truncated = read_proc(Path(entry.path) / "sched", 64 * 1024)
                limited |= truncated
                if value is None:
                    continue
                for line in value.splitlines():
                    key, separator, counter = line.partition(":")
                    if separator and key.strip() in ("se.nr_migrations", "nr_migrations"):
                        result[entry.name] = int(counter.strip())
                        break
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return result, True
    return result, limited


def numa_placement(pid):
    value, truncated = read_proc(Path(f"/proc/{pid}/numa_maps"))
    if value is None:
        return None, True
    nodes = {}
    for line in value.splitlines():
        for field in line.split():
            key, separator, pages = field.partition("=")
            if separator and key.startswith("N") and key[1:].isdigit() and pages.isdigit():
                if len(nodes) >= 256 and key not in nodes:
                    truncated = True
                    continue
                nodes[key] = nodes.get(key, 0) + int(pages)
    return nodes, truncated


def summary_from(path):
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        text = handle.read(MAX_PROC_BYTES + 1)
    if len(text) > MAX_PROC_BYTES:
        return None, "stdout exceeds summary parser limit"
    for line in reversed(text.splitlines()):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("schema") == "otel-ebpf-profiler-summary-v1":
            return value, None
    return None, "baseline did not emit the native-profiler summary schema"


def measure(name, argv, output, duration, warmup):
    directory = output / name
    directory.mkdir()
    rss_samples = deque(maxlen=600)
    peak_observed_rss = 0
    migration_total = 0
    last_migrations = {}
    migration_limited = False
    placement = None
    placement_limited = False
    warm_cpu = None
    warm_elapsed = None
    started = time.monotonic()
    termination = None
    timed_out = False
    next_details = started
    with (directory / "stdout.txt").open("xb") as stdout, (directory / "stderr.txt").open("xb") as stderr:
        child = subprocess.Popen(
            argv, cwd=WORKSPACE, stdout=stdout, stderr=stderr, start_new_session=True
        )
        try:
            while True:
                waited, status, usage = os.wait4(child.pid, os.WNOHANG)
                if waited == child.pid:
                    child.returncode = os.waitstatus_to_exitcode(status)
                    break
                now = time.monotonic()
                rss = rss_kib(child.pid)
                if rss is not None:
                    peak_observed_rss = max(peak_observed_rss, rss)
                    if now - started >= warmup:
                        rss_samples.append(rss)
                if warm_cpu is None and now - started >= warmup:
                    warm_cpu = cpu_seconds(child.pid)
                    if warm_cpu is not None:
                        warm_elapsed = now - started
                if now >= next_details:
                    current, limited = migrations(child.pid)
                    migration_limited |= limited
                    for tid, count in current.items():
                        migration_total += max(0, count - last_migrations.get(tid, 0))
                    last_migrations = current
                    current_placement, limited = numa_placement(child.pid)
                    if current_placement is not None:
                        placement = current_placement
                    placement_limited |= limited
                    next_details = now + 1
                if now - started > duration + 15 and termination is None:
                    # The process group belongs exclusively to this Popen call.
                    os.killpg(child.pid, signal.SIGTERM)
                    termination = now
                    timed_out = True
                elif termination is not None and now - termination > 2:
                    os.killpg(child.pid, signal.SIGKILL)
                time.sleep(0.1)
        finally:
            if child.returncode is None:
                # Never leave a profiler or an explicitly supplied baseline running.
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                _, status, _ = os.wait4(child.pid, 0)
                child.returncode = os.waitstatus_to_exitcode(status)
    elapsed = time.monotonic() - started
    profiler, unavailable = summary_from(directory / "stdout.txt")
    result = {
        "name": name,
        "argv": argv,
        "exit_code": child.returncode,
        "timed_out": timed_out,
        "elapsed_seconds": elapsed,
        "user_cpu_seconds": usage.ru_utime,
        "system_cpu_seconds": usage.ru_stime,
        "cpu_percent": 100 * (usage.ru_utime + usage.ru_stime) / elapsed,
        "steady_cpu_percent": (
            100 * max(0, usage.ru_utime + usage.ru_stime - warm_cpu) / (elapsed - warm_elapsed)
            if warm_cpu is not None and elapsed > warm_elapsed else None
        ),
        "peak_rss_kib": usage.ru_maxrss,
        "peak_observed_rss_kib": peak_observed_rss,
        "steady_rss_kib_median": statistics.median(rss_samples) if rss_samples else None,
        "steady_rss_sample_count": len(rss_samples),
        "voluntary_context_switches": usage.ru_nvcsw,
        "involuntary_context_switches": usage.ru_nivcsw,
        "observed_thread_migrations": migration_total,
        "migration_scan_limited": migration_limited,
        "numa_resident_pages_last": placement,
        "numa_scan_limited": placement_limited,
        "local_remote_memory_access": None,
        "local_remote_memory_access_reason": "requires model-specific perf/uncore counters; page placement is not an access measurement",
        "profiler": profiler,
        "profiler_metrics_unavailable_reason": unavailable,
    }
    if profiler is not None:
        result["samples_per_second"] = profiler["samples_consumed"] / elapsed
    with (directory / "result.json").open("x", encoding="utf-8") as handle:
        json.dump(result, handle, indent=2, sort_keys=True)
        handle.write("\n")
    return result


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration", type=positive_duration, default=30)
    parser.add_argument("--warmup", type=float, default=2)
    parser.add_argument("--samples-per-second", type=int, default=19)
    parser.add_argument("--rust-binary", type=Path, default=WORKSPACE / "target/release/examples/capture")
    parser.add_argument("--object", type=Path)
    parser.add_argument("--output-dir", type=Path, default=WORKSPACE / "target/ebpf-profiler-comparison")
    parser.add_argument("--go-command", type=command_array)
    parser.add_argument("--go-dfe-command", type=command_array)
    parser.add_argument("--lightswitch-command", type=command_array)
    parser.add_argument("--build", action="store_true", help="build Rust example once, outside timed measurements")
    parser.add_argument("--plan-only", action="store_true", help="print argv arrays without starting a profiler")
    parser.add_argument("--acknowledge-host-profiling", action="store_true")
    parsed = parser.parse_args()
    if not 1 <= parsed.samples_per_second <= 10000:
        parser.error("sample rate must be 1 through 10000")
    if not 0 <= parsed.warmup < parsed.duration:
        parser.error("warmup must be nonnegative and shorter than duration")
    if parsed.object is None and not parsed.plan_only:
        parser.error("--object must point to a reproducibly built BPF object and manifest")
    if not parsed.plan_only and not parsed.acknowledge_host_profiling:
        parser.error("explicit --acknowledge-host-profiling is required")
    return parsed


def main():
    args = arguments()
    if platform.system() != "Linux":
        raise SystemExit("Linux is required for procfs comparison measurements")
    binary = str(args.rust_binary.resolve())
    object_path = str(args.object.resolve()) if args.object is not None else "<object>"
    base = [
        binary, "--duration", f"{args.duration}s", "--reporting-interval", "1s",
        "--samples-per-second", str(args.samples_per_second), "--object", object_path,
        "--output", "json",
    ]
    cases = [
        ("rust-single", base + ["--sharding", "single"]),
        ("rust-per-numa", base + ["--sharding", "per-numa"]),
        ("rust-fixed-2", base + ["--sharding", "fixed:2"]),
        ("rust-slow-consumer", base + ["--sharding", "single", "--consumer-delay", f"{min(3, args.duration)}s"]),
    ]
    for name, command in [
        ("go-profiler", args.go_command), ("go-plus-dfe", args.go_dfe_command),
        ("lightswitch", args.lightswitch_command),
    ]:
        if command is not None:
            cases.append((name, command))
    if args.plan_only:
        print(json.dumps(cases, indent=2))
        return
    if args.build:
        subprocess.run(
            ["cargo", "build", "--release", "-p", "otel-arrow-dfe-ebpf-profiler", "--example", "capture"],
            cwd=WORKSPACE, check=True,
        )
    if not args.rust_binary.is_file():
        raise SystemExit("Rust example is absent; build it first or pass --build")
    timestamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = args.output_dir / f"{timestamp}-{os.getpid()}"
    output.mkdir(parents=True)
    results = []
    for name, command in cases:
        result = measure(name, command, output, args.duration, args.warmup)
        results.append(result)
        print(f"{name}: exit={result['exit_code']} peak_rss={result['peak_rss_kib']} KiB cpu={result['cpu_percent']:.2f}%")
        if result["exit_code"] != 0:
            break
    with (output / "comparison.json").open("x", encoding="utf-8") as handle:
        json.dump({
            "schema": "otel-ebpf-profiler-comparison-v1",
            "kernel": platform.release(), "architecture": platform.machine(),
            "duration_seconds": args.duration, "samples_per_second": args.samples_per_second,
            "results": results,
            "notes": [
                "Executables must be prebuilt; compilation is excluded from measurements.",
                "Use an identical frame-pointer workload and explicit sample rate/CPU scope for every baseline.",
                "Baseline-specific profile metrics remain unavailable until that baseline emits an equivalent normalized summary.",
                "Task migrations are sampled and can miss short-lived threads; residency does not measure NUMA accesses.",
                "No runtime or performance superiority is inferred from absent measurements.",
            ],
        }, handle, indent=2, sort_keys=True)
        handle.write("\n")
    print(output)
    if any(result["exit_code"] != 0 for result in results):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
