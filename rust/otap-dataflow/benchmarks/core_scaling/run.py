#!/usr/bin/env python3
# Copyright The OpenTelemetry Authors
# SPDX-License-Identifier: Apache-2.0
"""Linux-only DFE core-scaling experiment; affinity is a correctness gate."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request


def select_cpus(count):
    """Choose distinct physical cores from one visible NUMA node."""
    nodes = {}
    for cpu in sorted(os.sched_getaffinity(0)):
        root = Path(f"/sys/devices/system/cpu/cpu{cpu}")
        node_paths = list(root.glob("node[0-9]*"))
        if len(node_paths) != 1:
            raise RuntimeError(f"CPU {cpu}: NUMA topology unavailable")
        node = node_paths[0].name
        package = (root / "topology/physical_package_id").read_text().strip()
        core = (root / "topology/core_id").read_text().strip()
        nodes.setdefault(node, {}).setdefault((package, core), cpu)
    for node, cores in sorted(nodes.items()):
        if len(cores) >= count:
            return node, list(cores.values())[:count]
    raise RuntimeError(f"Need {count} visible physical cores on one NUMA node")


def configuration(cpus, port, rate):
    pipelines = {}
    for index, cpu in enumerate(cpus):
        pipelines[f"p{index}"] = {
            "policies": {
                "resources": {"core_allocation": {
                    "type": "core_set", "set": [{"start": cpu, "end": cpu}]}},
                "telemetry": {"runtime_metrics": "detailed"},
            },
            "nodes": {
                "source": {"type": "receiver:traffic_generator", "config": {
                    "data_source": "synthetic",
                    "generation_strategy": "pre_generated",
                    "traffic_config": {"signals_per_second": rate,
                                       "production_mode": "open",
                                       "max_batch_size": 1000,
                                       "metric_weight": 0, "trace_weight": 0,
                                       "log_weight": 100}}},
                "router": {"type": "processor:type_router", "config": {},
                           "outputs": ["logs", "metrics", "traces"]},
                "sink": {"type": "exporter:noop", "config": None},
            },
            "connections": [{"from": "source", "to": "router"},
                            {"from": 'router["logs"]', "to": "sink"}],
        }
    return {"version": "otel_dataflow/v1",
            "engine": {"http_admin": {"bind_address": f"127.0.0.1:{port}"},
                       "telemetry": {"reporting_interval": "1s",
                                     "logs": {"level": "warn"}}},
            "groups": {"s": {"pipelines": pipelines}}}


def worker_affinity(pid, cpus):
    """Match Linux's truncated thread names, and verify exact masks."""
    expected = {f"pipeline-s-p{i}-core-{cpu}-gen-0"[:15]: cpu
                for i, cpu in enumerate(cpus)}
    if len(expected) != len(cpus):
        raise RuntimeError("Worker names collide after Linux truncation")
    found = {}
    for task in Path(f"/proc/{pid}/task").iterdir():
        try:
            name = (task / "comm").read_text().strip()
            if name in expected:
                tid = int(task.name)
                mask = os.sched_getaffinity(tid)
                if mask != {expected[name]}:
                    raise RuntimeError(f"Worker {name}/{tid}: affinity {mask}, "
                                       f"expected {{{expected[name]}}}")
                if name in found:
                    raise RuntimeError(f"Duplicate worker {name}")
                found[name] = tid
        except FileNotFoundError:
            continue  # An unrelated auxiliary thread may exit during inspection.
    if found.keys() != expected.keys():
        raise RuntimeError(f"Missing workers: {expected.keys() - found.keys()}")
    return found


def cpu_seconds(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def snapshot(port):
    url = (f"http://127.0.0.1:{port}/api/v1/telemetry/metrics"
           "?format=json_compact&keep_all_zeroes=true")
    with urllib.request.urlopen(url, timeout=5) as response:
        return json.load(response)


def unwrap(value):
    return next(iter(value.values())) if isinstance(value, dict) else value


def delivered(snapshot_data, count):
    totals = {f"p{i}": 0 for i in range(count)}
    seen = set()
    for metric in snapshot_data["metric_sets"]:
        attrs = {key: unwrap(value) for key, value in metric["attributes"].items()}
        if (metric["name"] != "node.input" or attrs.get("node.id") != "sink"
                or attrs.get("pipeline.group.id") != "s"):
            continue
        pipeline = attrs.get("pipeline.id")
        if pipeline in totals and "items" in metric["metrics"]:
            totals[pipeline] += metric["metrics"]["items"]
            seen.add(pipeline)
    if seen != totals.keys():
        raise RuntimeError(f"Missing sink item counters: {totals.keys() - seen}")
    return totals


def run(args, cpus, directory):
    directory.mkdir()
    # Fail before launch if another service owns the measurement endpoint.
    with socket.socket() as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        probe.bind(("127.0.0.1", args.port))
    config = directory / "config.yaml"
    config.write_text(json.dumps(configuration(cpus, args.port, args.rate), indent=2) + "\n")
    command = [str(args.binary), "--config", str(config)]
    with (directory / "validation.log").open("w") as validation_log:
        subprocess.run(command + ["--validate-and-exit"], check=True,
                       stdout=validation_log, stderr=subprocess.STDOUT)
    with (directory / "engine.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
            last_error = None
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f"Engine exited: see {directory / 'engine.log'}")
                try:
                    delivered(snapshot(args.port), len(cpus))
                    break
                except (OSError, ValueError, RuntimeError, urllib.error.URLError) as error:
                    last_error = error
                    time.sleep(0.1)  # Poll observable counters, not assumed readiness.
            else:
                raise RuntimeError(f"Sink counters not ready: {last_error}")
            workers = worker_affinity(process.pid, cpus)
            time.sleep(args.warmup)  # Deliberate benchmark warm-up, not synchronization.
            before = snapshot(args.port)
            before_counts = delivered(before, len(cpus))
            start_cpu = cpu_seconds(process.pid)
            start = time.monotonic()
            deadline = start + args.seconds
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError("Engine exited during measurement")
                if worker_affinity(process.pid, cpus) != workers:
                    raise RuntimeError("Worker identity changed during measurement")
                time.sleep(min(0.25, max(0, deadline - time.monotonic())))
            after = snapshot(args.port)
            elapsed = time.monotonic() - start
            used_cpu = cpu_seconds(process.pid) - start_cpu
            after_counts = delivered(after, len(cpus))
            deltas = {key: after_counts[key] - before_counts[key] for key in before_counts}
            if any(value <= 0 for value in deltas.values()):
                raise RuntimeError(f"Nonpositive delivery counter delta: {deltas}")
            result = {"cpus": cpus, "workers": workers, "elapsed_seconds": elapsed,
                      "process_cpu_seconds": used_cpu, "delivered_records": deltas,
                      "records_per_second": sum(deltas.values()) / elapsed,
                      "cpu_seconds_per_record": used_cpu / sum(deltas.values())}
            for name, value in [("before", before), ("after", after), ("result", result)]:
                (directory / f"{name}.json").write_text(json.dumps(value, indent=2) + "\n")
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=75)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise RuntimeError("Engine did not shut down gracefully")
        if process.returncode != 0:
            raise RuntimeError(f"Engine exit status {process.returncode}")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cores", type=int, nargs="+", default=[1, 2, 4])
    parser.add_argument("--rate", type=int, default=1000000,
                        help="offered records/s per core (also sizes pre-generated data)")
    parser.add_argument("--seconds", type=float, default=30)
    parser.add_argument("--warmup", type=float, default=5)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--port", type=int, default=18085)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("Linux /proc and sysfs are required")
    if (min(args.cores) < 1 or args.rate < 1 or args.seconds <= 0 or args.warmup < 0
            or args.repetitions < 1):
        parser.error("cores, rate, seconds and repetitions must be positive; warmup >= 0")
    args.binary = args.binary.resolve(strict=True)
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    node, cpus = select_cpus(max(args.cores))
    with args.binary.open("rb") as binary:
        binary_hash = hashlib.file_digest(binary, "sha256").hexdigest()
    metadata = {"binary_sha256": binary_hash, "uname": list(platform.uname()), "node": node, "cpus": cpus,
                "arguments": {key: str(value) for key, value in vars(args).items()},
                "git_revision": subprocess.check_output(
                    ["git", "rev-parse", "HEAD"], text=True).strip()}
    (args.output / "source.diff").write_text(subprocess.check_output(
        ["git", "diff", "HEAD"], text=True))
    (args.output / "source-status.txt").write_text(subprocess.check_output(
        ["git", "status", "--short"], text=True))
    (args.output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    results = []
    for repetition in range(args.repetitions):
        for count in args.cores:
            result = run(args, cpus[:count], args.output / f"r{repetition}-c{count}")
            results.append(result)
            print(json.dumps(result), flush=True)
    (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
