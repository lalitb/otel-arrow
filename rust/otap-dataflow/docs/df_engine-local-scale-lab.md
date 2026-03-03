# df_engine Local Scale Lab

This guide describes a local-only stress lab for `df_engine` focusing on:

- thread-per-core scaling behavior
- event-threshold test runs
- memory growth/leak detection
- throughput and CPU trends
- optional heap/perf profiling hooks

The lab script is:

- `scripts/local/df_engine_scale_lab.sh`

It is intentionally local experimentation tooling (not CI automation).

## What the script does

For each `(core_count, signal_rate)` pair:

1. Generates a dedicated config (`receiver:traffic_generator -> exporter:perf`),
   or uses a user-supplied YAML via `--config`.
2. Starts `df_engine` (release build by default).
3. Samples `/telemetry/metrics?format=json` each second.
4. Tracks:
   - produced events
   - `pipeline.metrics` memory usage
   - `pipeline.metrics` memory alloc/free deltas
   - `pipeline.metrics` CPU utilization
   - process RSS
5. Stops by duration or when `--threshold-events` is reached.
6. Writes per-run artifacts and an aggregate summary CSV.

## Quick start

```bash
scripts/local/df_engine_scale_lab.sh \
  --core-sweep 1,2,4,8 \
  --rate-sweep 50000,100000,200000 \
  --run-seconds 90 \
  --threshold-events 5000000
```

Artifacts are written to:

- `.local/df-engine-scale-lab/<timestamp>/`

Top-level files:

- `summary.csv`: one row per run
- `run-meta.txt`: run parameters

Per-run files (`c<cores>_r<rate>/`):

- `config.yaml`: generated config
- `samples.csv`: sampled time-series
- `raw_metrics.ndjson`: raw admin metrics snapshots
- `summary.json`: run summary
- `engine.stdout.log`, `engine.stderr.log`

## Recommended run profiles

### Custom pipeline config

Test any pipeline topology by supplying your own YAML:

```bash
scripts/local/df_engine_scale_lab.sh \
  --config configs/otlp-otlp.yaml \
  --core-sweep 1,2,4 \
  --run-seconds 60
```

When `--config` is used, the script skips auto-generating the
`traffic_generator -> perf` pipeline and uses the supplied YAML as-is.
Core allocation, receivers, processors, and exporters are all controlled
by the YAML itself. The monitoring/sampling/artifact machinery still
runs normally.

### Short scaling sanity check:

```bash
scripts/local/df_engine_scale_lab.sh \
  --core-sweep 1,2,4 \
  --rate-sweep 20000,50000,100000 \
  --run-seconds 60
```

Leak-focused soak:

```bash
scripts/local/df_engine_scale_lab.sh \
  --core-sweep 4,8 \
  --rate-sweep 100000 \
  --run-seconds 900 \
  --threshold-events 0 \
  --leak-threshold-mb 128
```

Trace-heavy run:

```bash
scripts/local/df_engine_scale_lab.sh \
  --signal-kind traces \
  --core-sweep 2,4,8 \
  --rate-sweep 20000,40000 \
  --run-seconds 120
```

## Heap / perf hooks

Supported profiler modes:

- `--heap-profiler none`
- `--heap-profiler jemalloc`
- `--heap-profiler heaptrack` (requires `heaptrack`)
- `--heap-profiler massif` (requires `valgrind`)

Optional Linux perf stats:

- `--with-perf` (requires `perf`)

NUMA test wrapper:

- `--numa-node N` (requires `numactl`; applies `--cpunodebind` + `--membind`)

## Interpreting leak and scaling signals

Primary columns in `summary.csv`:

- `throughput_events_per_sec`: aggregate produced events/s
- `memory_growth_mb`: net `pipeline.metrics.memory_usage` growth
- `leak_rate_mib_per_min`: memory growth trend speed
- `leak_flag`: true when `memory_growth_mb > --leak-threshold-mb`

Useful checks:

1. Throughput should increase with core count until bottlenecks appear.
2. `memory_growth_mb` should stabilize near zero in steady-state runs.
3. High throughput with rising leak-rate suggests buffering/retention growth.
4. Compare `memory_growth_mb` vs `rss_growth_mb` to separate heap-only vs full-process growth.

## Notes

- Default generator mode is `data_source=static` + `generation_strategy=pre_generated` for stable high-throughput local tests.
- If you need semantic-conventions-based generation, pass `--data-source semantic_conventions` (network access may be required by the generator).
