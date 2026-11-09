# Core scaling guardrails

This Linux experiment runs independent synthetic log pipelines on distinct
physical cores within one NUMA node. Each pipeline contains a pre-generated
traffic source, a type router, and a no-op sink. Each source offers one million
records per second in open-loop bursts by default. There is no external
network, registry download, or disk data path.

Python 3.11 or newer is required. Build and run from `rust/otap-dataflow`:

```sh
cargo build --release --bin df_engine
python3 benchmarks/core_scaling/run.py \
  --binary target/release/df_engine --output /tmp/dfe-core-scaling
```

The default experiment uses 1, 2, and 4 cores, three repetitions, five seconds
of warm-up, and thirty seconds of measurement. Use `--rate` to change the
offered records per second per core; increasing it also increases pre-generated
data memory. This is a fixed offered-load experiment, not an automatic
maximum-throughput search. If all offered traffic is delivered, throughput is
source-limited; compare CPU cost per record instead. Run on an otherwise idle,
reserved machine. Use `--cores 1 2` if only two physical cores are available.
The runner refuses to substitute SMT siblings or span NUMA nodes. In a VM,
physical-core identity means guest-reported topology; record the host/VM SKU
separately and do not interpret it as proof of host CPU isolation.

The admin port defaults to 18085; reserve it for this job or choose `--port`.
The output directory must not already exist. Engine configuration validation
runs before each experiment. Keep the binary build command, compiler version,
allocator choice, and source diff alongside the generated artifacts when
comparing branches. The recorded Git revision alone does not identify an
uncommitted source tree or independently supplied binary.

## Correctness and measurement boundaries

The runner waits for sink counters, then independently reads each pipeline
worker's actual affinity mask. Every selected worker must have exactly its
configured CPU, and its thread identity must remain stable during measurement.
An affinity mismatch, absent worker, missing counter, nonpositive delivery,
engine failure, or shutdown timeout fails the job. Auxiliary threads are not
subject to the pipeline worker affinity rule.

The benchmark reports delivered sink input items, aggregate records per second,
and process CPU seconds per record. Raw snapshots and per-pipeline deltas are
retained. Counters are collected asynchronously at one-second intervals, so
measurement boundaries are approximate; use long windows for comparisons.
CPU usage includes engine auxiliary work and uses `/proc` clock ticks, so low
CPU consumption has coarse relative precision. Increase the offered load or
measurement duration before interpreting small CPU-cost differences.
Detailed telemetry is enabled in all
runs to obtain item counters, and its overhead is part of this baseline.

This first workload measures local routing/channel throughput and generation
cost. It does not represent decoding, transformation, network I/O, or durability.
It cannot prove the absence of shared state or remote-NUMA accesses.

## CI usage

The existing internal-observability PR workflow runs the harness tests and a
one-core affinity smoke check using its existing release build. This checks
placement without requiring a multicore benchmark runner. Cross-core scaling
is a separate experiment on a reserved runner.

Use a short run for an affinity smoke check:

```sh
python3 benchmarks/core_scaling/run.py \
  --binary target/release/df_engine --output /tmp/dfe-affinity-smoke \
  --cores 1 2 --warmup 0 --seconds 2 --repetitions 1
```

Use the full experiment in a dedicated scheduled benchmark job, saving the
entire output directory as an artifact. Compare the baseline and candidate on
the same machine with the same build profile and workload. Performance results
are informational: this script deliberately has no scaling threshold.
Establish normal variation before introducing a regression gate.

Run the harness contract tests with:

```sh
python3 -m unittest discover -s benchmarks/core_scaling -p 'test_*.py'
```

`perf c2c` profiling is a separate follow-up. Do not mix profiling runs with
throughput baselines or treat an empty contention report as proof of isolation.
