# OTAP Dataflow Monitoring

Pre-built [Perses](https://perses.dev/) dashboards for monitoring the OTAP
Dataflow engine. Uses the Prometheus-format metrics the engine already exposes
at `/metrics`.

## Quick Start

```bash
# 1. Start the engine (ensure http_admin is enabled)
cargo run -- --config configs/fake-perf.yaml

# 2. Start Prometheus + Perses
cd monitoring
docker compose up -d

# 3. Provision dashboards (one-time, idempotent)
./setup.sh

# 4. Open the dashboard
open http://localhost:8081    # Perses UI → project "otap-dataflow"
open http://localhost:9091    # Prometheus UI (for ad-hoc queries)
```

## What's Included

| File | Purpose |
|------|---------|
| `docker-compose.yml` | Prometheus + Perses stack |
| `setup.sh` | Provisions project, datasource & dashboards into Perses |
| `prometheus/prometheus.yml` | Scrape config targeting the engine admin endpoint |
| `perses/project.json` | Perses project definition |
| `perses/datasource.json` | Prometheus datasource for Perses |
| `perses/otap-overview-dashboard.json` | Main dashboard (engine health, data flow, runtime) |

## Dashboard Panels

### Engine Health
- **RSS Memory** — process-wide resident set size over time
- **Engine CPU** — normalized CPU utilization (engine-level)
- **CPU by Pipeline Group** — per-group CPU breakdown
- **Memory by Pipeline Group** — per-group heap usage

### Data Flow
- **Channel Throughput** — messages/sec through inter-node channels
- **Backpressure** — `send_error_full` rate (indicates bottlenecks)
- **Exporter Throughput** — exported vs failed signals/sec

### Runtime
- **Tokio Active Tasks** — total async tasks across all pipeline runtimes
- **Context Switches** — voluntary/involuntary switch rates
- **Heap Memory** — jemalloc heap usage by pipeline group

## Configuration

### Changing the engine admin address

Edit `prometheus/prometheus.yml` and update the target:

```yaml
static_configs:
  - targets: ["host.docker.internal:8085"]  # change this
```

### Adding more dashboards

Place Perses dashboard JSON files in `perses/`. They are auto-provisioned on
startup via the `PERSES_PROVISIONING_FOLDERS` setting.

## Alternative: Console TUI

For quick local debugging without any infrastructure, use the Python script:

```bash
python3 scripts/engine-metrics.py --url http://127.0.0.1:8085 -i 5
```

See `scripts/engine-metrics.py --help` for filtering and per-core options.
