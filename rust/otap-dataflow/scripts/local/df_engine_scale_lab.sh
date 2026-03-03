#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

DEFAULT_BINARY="${ROOT_DIR}/target/release/df_engine"
DEFAULT_OUT_BASE="${ROOT_DIR}/.local/df-engine-scale-lab"

BINARY="${DEFAULT_BINARY}"
OUT_DIR=""
CORE_SWEEP="1,2,4"
RATE_SWEEP="10000,50000,100000"
RUN_SECONDS=60
SAMPLE_SECONDS=1
THRESHOLD_EVENTS=0
BATCH_SIZE=1000
SIGNAL_KIND="logs"
DATA_SOURCE="static"
GENERATION_STRATEGY="pre_generated"
LEAK_THRESHOLD_MB=64
HTTP_BASE_PORT=28080
SKIP_BUILD=0
WITH_PERF=0
NUMA_NODE=""
HEAP_PROFILER="none"
USER_CONFIG=""

timestamp_utc() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log() {
  printf '[%s] %s\n' "$(timestamp_utc)" "$*" >&2
}

usage() {
  cat <<'EOF'
Local df_engine scaling + leak lab.

This script runs local-only experiments for thread-per-core pipeline scaling:
- core-count sweeps
- signal-rate sweeps
- threshold-based event generation
- memory/cpu telemetry sampling from /telemetry/metrics (json)
- leak-growth scoring and summary artifacts
- optional heap/perf capture hooks when tools are available

Usage:
  scripts/local/df_engine_scale_lab.sh [options]

Options:
  --binary PATH                  Path to df_engine binary
  --out-dir PATH                 Artifact directory (default: .local/... timestamped)
  --core-sweep CSV               Core counts, e.g. "1,2,4,8"
  --rate-sweep CSV               Signals/sec rates, e.g. "20000,50000,100000"
  --run-seconds N                Max seconds per run (default: 60)
  --sample-seconds N             Metric sample period in seconds (default: 1)
  --threshold-events N           Stop run when produced events >= N (0 disables)
  --batch-size N                 traffic_config.max_batch_size (default: 1000)
  --signal-kind KIND             logs|metrics|traces (default: logs)
  --data-source KIND             static|semantic_conventions (default: static)
  --generation-strategy KIND     pre_generated|fresh|templates (default: pre_generated)
  --leak-threshold-mb N          Leak warning threshold in MB (default: 64)
  --http-base-port N             First admin port (default: 28080)
  --heap-profiler KIND           none|jemalloc|heaptrack|massif (default: none)
  --with-perf                    Try perf stat collection (Linux perf)
  --numa-node N                  Run under numactl --cpunodebind/--membind N
  --skip-build                   Skip cargo build --release
  --config PATH                  Use a custom pipeline YAML instead of auto-generating one.
                                 When set, --signal-kind/--data-source/--generation-strategy
                                 and --batch-size are ignored. Core allocation and rate are
                                 controlled by the YAML itself.
  --help                         Show this help

Examples:
  # Custom pipeline config (e.g. otlp -> fan_out -> noop):
  scripts/local/df_engine_scale_lab.sh \
    --config my-pipeline.yaml \
    --core-sweep 1,2,4 \
    --run-seconds 60
  scripts/local/df_engine_scale_lab.sh \
    --core-sweep 1,2,4,8 \
    --rate-sweep 50000,100000,200000 \
    --run-seconds 90 \
    --threshold-events 5000000

  scripts/local/df_engine_scale_lab.sh \
    --core-sweep 2,4,8 \
    --rate-sweep 200000 \
    --heap-profiler jemalloc \
    --run-seconds 300
EOF
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

validate_positive_int() {
  local name="$1"
  local value="$2"
  if ! [[ "$value" =~ ^[0-9]+$ ]]; then
    echo "Invalid ${name}: ${value}" >&2
    exit 1
  fi
}

contains_csv_value() {
  local csv="$1"
  local needle="$2"
  local item
  IFS=',' read -r -a arr <<<"${csv}"
  for item in "${arr[@]}"; do
    if [[ "$item" == "$needle" ]]; then
      return 0
    fi
  done
  return 1
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --binary)
        BINARY="$2"
        shift 2
        ;;
      --out-dir)
        OUT_DIR="$2"
        shift 2
        ;;
      --core-sweep)
        CORE_SWEEP="$2"
        shift 2
        ;;
      --rate-sweep)
        RATE_SWEEP="$2"
        shift 2
        ;;
      --run-seconds)
        RUN_SECONDS="$2"
        shift 2
        ;;
      --sample-seconds)
        SAMPLE_SECONDS="$2"
        shift 2
        ;;
      --threshold-events)
        THRESHOLD_EVENTS="$2"
        shift 2
        ;;
      --batch-size)
        BATCH_SIZE="$2"
        shift 2
        ;;
      --signal-kind)
        SIGNAL_KIND="$2"
        shift 2
        ;;
      --data-source)
        DATA_SOURCE="$2"
        shift 2
        ;;
      --generation-strategy)
        GENERATION_STRATEGY="$2"
        shift 2
        ;;
      --leak-threshold-mb)
        LEAK_THRESHOLD_MB="$2"
        shift 2
        ;;
      --http-base-port)
        HTTP_BASE_PORT="$2"
        shift 2
        ;;
      --heap-profiler)
        HEAP_PROFILER="$2"
        shift 2
        ;;
      --with-perf)
        WITH_PERF=1
        shift 1
        ;;
      --numa-node)
        NUMA_NODE="$2"
        shift 2
        ;;
      --skip-build)
        SKIP_BUILD=1
        shift 1
        ;;
      --config)
        USER_CONFIG="$2"
        shift 2
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      *)
        echo "Unknown option: $1" >&2
        usage
        exit 1
        ;;
    esac
  done
}

validate_args() {
  require_cmd curl
  require_cmd jq

  validate_positive_int "run-seconds" "${RUN_SECONDS}"
  validate_positive_int "sample-seconds" "${SAMPLE_SECONDS}"
  validate_positive_int "threshold-events" "${THRESHOLD_EVENTS}"
  validate_positive_int "batch-size" "${BATCH_SIZE}"
  validate_positive_int "http-base-port" "${HTTP_BASE_PORT}"

  if ! contains_csv_value "logs,metrics,traces" "${SIGNAL_KIND}"; then
    echo "signal-kind must be one of: logs, metrics, traces" >&2
    exit 1
  fi

  if ! contains_csv_value "static,semantic_conventions" "${DATA_SOURCE}"; then
    echo "data-source must be one of: static, semantic_conventions" >&2
    exit 1
  fi

  if ! contains_csv_value "pre_generated,fresh,templates" "${GENERATION_STRATEGY}"; then
    echo "generation-strategy must be one of: pre_generated, fresh, templates" >&2
    exit 1
  fi

  if ! contains_csv_value "none,jemalloc,heaptrack,massif" "${HEAP_PROFILER}"; then
    echo "heap-profiler must be one of: none, jemalloc, heaptrack, massif" >&2
    exit 1
  fi

  if [[ -n "${NUMA_NODE}" ]]; then
    validate_positive_int "numa-node" "${NUMA_NODE}"
    if ! command -v numactl >/dev/null 2>&1; then
      echo "numactl is required for --numa-node but is not installed." >&2
      exit 1
    fi
  fi

  if [[ "${HEAP_PROFILER}" == "heaptrack" ]] && ! command -v heaptrack >/dev/null 2>&1; then
    echo "heaptrack is required for --heap-profiler heaptrack." >&2
    exit 1
  fi

  if [[ "${HEAP_PROFILER}" == "massif" ]] && ! command -v valgrind >/dev/null 2>&1; then
    echo "valgrind is required for --heap-profiler massif." >&2
    exit 1
  fi

  if [[ "${WITH_PERF}" -eq 1 ]] && ! command -v perf >/dev/null 2>&1; then
    log "perf not found; disabling --with-perf."
    WITH_PERF=0
  fi

  if [[ -n "${USER_CONFIG}" && ! -f "${USER_CONFIG}" ]]; then
    echo "Config file not found: ${USER_CONFIG}" >&2
    exit 1
  fi
}

weights_for_signal_kind() {
  case "${SIGNAL_KIND}" in
    logs)
      METRIC_WEIGHT=0
      TRACE_WEIGHT=0
      LOG_WEIGHT=100
      ;;
    metrics)
      METRIC_WEIGHT=100
      TRACE_WEIGHT=0
      LOG_WEIGHT=0
      ;;
    traces)
      METRIC_WEIGHT=0
      TRACE_WEIGHT=100
      LOG_WEIGHT=0
      ;;
  esac
}

prepare_out_dir() {
  if [[ -z "${OUT_DIR}" ]]; then
    local stamp
    stamp="$(date -u +%Y%m%dT%H%M%SZ)"
    OUT_DIR="${DEFAULT_OUT_BASE}/${stamp}"
  fi
  mkdir -p "${OUT_DIR}"
}

build_binary() {
  if [[ "${SKIP_BUILD}" -eq 1 ]]; then
    return
  fi
  log "Building df_engine release binary..."
  (cd "${ROOT_DIR}" && cargo build --release --bin df_engine)
}

wait_for_ready() {
  local port="$1"
  local max_tries=60
  local i
  for ((i=1; i<=max_tries; i++)); do
    if curl -fsS "http://127.0.0.1:${port}/readyz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

shutdown_engine() {
  local port="$1"
  local pid="$2"

  curl -fsS -X POST "http://127.0.0.1:${port}/pipeline-groups/shutdown?wait=true" >/dev/null 2>&1 || true

  local i
  for ((i=0; i<10; i++)); do
    if ! kill -0 "${pid}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done

  kill -TERM "${pid}" >/dev/null 2>&1 || true
  sleep 2
  if kill -0 "${pid}" >/dev/null 2>&1; then
    kill -KILL "${pid}" >/dev/null 2>&1 || true
  fi
}

write_run_config() {
  local path="$1"
  local port="$2"
  local cores="$3"
  local rate="$4"

  local max_signal
  if [[ "${THRESHOLD_EVENTS}" -gt 0 ]]; then
    max_signal="${THRESHOLD_EVENTS}"
  else
    max_signal="null"
  fi

  cat >"${path}" <<EOF
version: otel_dataflow/v1
policies:
  telemetry:
    pipeline_metrics: true
    tokio_metrics: true
    channel_metrics: true
engine:
  http_admin:
    bind_address: "127.0.0.1:${port}"
  telemetry:
    logs:
      level: warn
groups:
  local_scale_lab:
    pipelines:
      main:
        policies:
          resources:
            core_allocation:
              type: core_count
              count: ${cores}
        nodes:
          receiver:
            type: receiver:traffic_generator
            config:
              data_source: ${DATA_SOURCE}
              generation_strategy: ${GENERATION_STRATEGY}
              traffic_config:
                signals_per_second: ${rate}
                max_signal_count: ${max_signal}
                max_batch_size: ${BATCH_SIZE}
                metric_weight: ${METRIC_WEIGHT}
                trace_weight: ${TRACE_WEIGHT}
                log_weight: ${LOG_WEIGHT}
          perf:
            type: exporter:perf
            config:
              frequency: 1000
              self_usage: false
              cpu_usage: false
              mem_usage: false
              disk_usage: false
              io_usage: false
        connections:
          - from: receiver
            to: perf
EOF
}

start_engine() {
  local run_dir="$1"
  local config="$2"
  local port="$3"
  local stdout_log="${run_dir}/engine.stdout.log"
  local stderr_log="${run_dir}/engine.stderr.log"

  local -a cmd=()
  local -a env_cmd=()

  if [[ -n "${NUMA_NODE}" ]]; then
    cmd+=(numactl "--cpunodebind=${NUMA_NODE}" "--membind=${NUMA_NODE}")
  fi

  case "${HEAP_PROFILER}" in
    heaptrack)
      cmd+=(heaptrack "-o" "${run_dir}/heaptrack.out")
      ;;
    massif)
      cmd+=(valgrind "--tool=massif" "--massif-out-file=${run_dir}/massif.out")
      ;;
    jemalloc)
      env_cmd+=(
        env
        "MALLOC_CONF=prof:true,prof_active:true,prof_final:true,lg_prof_sample:19,prof_prefix:${run_dir}/jeprof"
      )
      ;;
  esac

  cmd+=("${BINARY}" "--config" "${config}" "--http-admin-bind" "127.0.0.1:${port}")

  if [[ "${#env_cmd[@]}" -gt 0 ]]; then
    "${env_cmd[@]}" "${cmd[@]}" >"${stdout_log}" 2>"${stderr_log}" &
  else
    "${cmd[@]}" >"${stdout_log}" 2>"${stderr_log}" &
  fi

  echo $!
}

extract_metrics_json() {
  local snapshot="$1"
  jq -c '
    def num:
      if type == "number" then .
      elif type == "object" then
        (
          .u64 // .U64 //
          .f64 // .F64 //
          (if has("mmsc") then .mmsc.sum else empty end) //
          (if has("Mmsc") then .Mmsc.sum else empty end) //
          .sum // .Sum // 0
        )
      else 0 end;

    def vals($set_re; $name_re):
      [
        .metric_sets[]
        | select(.name | test($set_re))
        | .metrics[]
        | select(.name | test($name_re))
        | .value
        | num
      ];

    def set_count($set_re):
      [ .metric_sets[] | select(.name | test($set_re)) ] | length;

    {
      produced: (
        vals("(^|\\.)fake_data_generator\\.receiver\\.metrics$"; "^(logs|metrics|spans)[._]produced$")
        | add // 0
      ),
      memory_usage: (
        vals("(^|\\.)pipeline\\.metrics$"; "^memory[._]usage$")
        | add // 0
      ),
      memory_allocated_delta: (
        vals("(^|\\.)pipeline\\.metrics$"; "^memory[._]allocated[._]delta$")
        | add // 0
      ),
      memory_freed_delta: (
        vals("(^|\\.)pipeline\\.metrics$"; "^memory[._]freed[._]delta$")
        | add // 0
      ),
      cpu_utilization_sum: (
        vals("(^|\\.)pipeline\\.metrics$"; "^cpu[._]utilization$")
        | add // 0
      ),
      pipeline_instances: set_count("(^|\\.)pipeline\\.metrics$")
    }
  ' <<<"${snapshot}"
}

run_single_case() {
  local case_idx="$1"
  local cores="$2"
  local rate="$3"
  local port="$4"

  local run_id="c${cores}_r${rate}"
  local run_dir="${OUT_DIR}/${run_id}"
  mkdir -p "${run_dir}"

  local config="${run_dir}/config.yaml"
  if [[ -n "${USER_CONFIG}" ]]; then
    cp "${USER_CONFIG}" "${config}"
  else
    write_run_config "${config}" "${port}" "${cores}" "${rate}"
  fi

  log "Starting run ${run_id} on http port ${port}"
  local engine_pid
  engine_pid="$(start_engine "${run_dir}" "${config}" "${port}")"
  echo "${engine_pid}" >"${run_dir}/engine.pid"

  local perf_pid=""
  if [[ "${WITH_PERF}" -eq 1 ]]; then
    if perf stat -p "${engine_pid}" -o "${run_dir}/perf.stat.txt" --interval-print 1000 >/dev/null 2>&1 & then
      perf_pid=$!
      echo "${perf_pid}" >"${run_dir}/perf.pid"
    else
      log "perf failed for ${run_id}; continuing without perf."
    fi
  fi

  if ! wait_for_ready "${port}"; then
    log "Run ${run_id} failed: admin endpoint did not become ready."
    shutdown_engine "${port}" "${engine_pid}"
    if [[ -n "${perf_pid}" ]]; then
      kill "${perf_pid}" >/dev/null 2>&1 || true
    fi
    return 1
  fi

  local csv="${run_dir}/samples.csv"
  local raw="${run_dir}/raw_metrics.ndjson"
  printf '%s\n' "epoch_s,elapsed_s,produced_total,produced_rate_per_s,memory_usage_bytes,memory_allocated_delta_bytes,memory_freed_delta_bytes,cpu_utilization_sum,cpu_utilization_avg,pipeline_instances,rss_bytes" >"${csv}"

  local start_epoch
  start_epoch="$(date +%s)"
  local now_epoch=0
  local elapsed=0
  local prev_epoch=0
  local prev_produced=0
  local first=1

  local first_epoch=0
  local first_produced=0
  local first_mem=0
  local first_rss=0

  local last_epoch=0
  local last_produced=0
  local last_mem=0
  local last_rss=0

  local stop_reason="duration_limit"

  while true; do
    now_epoch="$(date +%s)"
    elapsed=$((now_epoch - start_epoch))

    if ! kill -0 "${engine_pid}" >/dev/null 2>&1; then
      stop_reason="engine_exit"
      break
    fi

    local snapshot
    if ! snapshot="$(curl -fsS "http://127.0.0.1:${port}/telemetry/metrics?format=json&reset=false&keep_all_zeroes=false")"; then
      log "Metrics scrape failed for ${run_id}; retrying."
      sleep "${SAMPLE_SECONDS}"
      continue
    fi

    local compact
    compact="$(jq -c . <<<"${snapshot}")"
    printf '{"epoch_s":%s,"snapshot":%s}\n' "${now_epoch}" "${compact}" >>"${raw}"

    local metrics
    metrics="$(extract_metrics_json "${snapshot}")"

    local produced mem_usage mem_alloc_delta mem_freed_delta cpu_util_sum pipeline_instances
    produced="$(jq -r '.produced' <<<"${metrics}")"
    mem_usage="$(jq -r '.memory_usage' <<<"${metrics}")"
    mem_alloc_delta="$(jq -r '.memory_allocated_delta' <<<"${metrics}")"
    mem_freed_delta="$(jq -r '.memory_freed_delta' <<<"${metrics}")"
    cpu_util_sum="$(jq -r '.cpu_utilization_sum' <<<"${metrics}")"
    pipeline_instances="$(jq -r '.pipeline_instances' <<<"${metrics}")"

    local rss_kb rss_bytes
    rss_kb="$(ps -o rss= -p "${engine_pid}" 2>/dev/null | tr -d '[:space:]' || true)"
    if [[ -z "${rss_kb}" ]]; then
      rss_kb=0
    fi
    rss_bytes=$((rss_kb * 1024))

    local produced_rate=0
    if [[ "${first}" -eq 0 ]]; then
      local dt=$((now_epoch - prev_epoch))
      if [[ "${dt}" -gt 0 ]]; then
        local dprod=$((produced - prev_produced))
        produced_rate="$(awk -v d="${dprod}" -v t="${dt}" 'BEGIN { printf "%.6f", d / t }')"
      fi
    fi

    local cpu_util_avg=0
    if [[ "${pipeline_instances}" -gt 0 ]]; then
      cpu_util_avg="$(awk -v s="${cpu_util_sum}" -v n="${pipeline_instances}" 'BEGIN { printf "%.6f", s / n }')"
    fi

    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
      "${now_epoch}" "${elapsed}" "${produced}" "${produced_rate}" \
      "${mem_usage}" "${mem_alloc_delta}" "${mem_freed_delta}" "${cpu_util_sum}" \
      "${cpu_util_avg}" "${pipeline_instances}" "${rss_bytes}" >>"${csv}"

    if [[ "${first}" -eq 1 ]]; then
      first=0
      first_epoch="${now_epoch}"
      first_produced="${produced}"
      first_mem="${mem_usage}"
      first_rss="${rss_bytes}"
    fi
    prev_epoch="${now_epoch}"
    prev_produced="${produced}"

    last_epoch="${now_epoch}"
    last_produced="${produced}"
    last_mem="${mem_usage}"
    last_rss="${rss_bytes}"

    if [[ "${THRESHOLD_EVENTS}" -gt 0 ]] && [[ "${produced}" -ge "${THRESHOLD_EVENTS}" ]]; then
      stop_reason="threshold_reached"
      break
    fi
    if [[ "${elapsed}" -ge "${RUN_SECONDS}" ]]; then
      stop_reason="duration_limit"
      break
    fi

    sleep "${SAMPLE_SECONDS}"
  done

  shutdown_engine "${port}" "${engine_pid}"
  if [[ -n "${perf_pid}" ]]; then
    kill "${perf_pid}" >/dev/null 2>&1 || true
  fi

  if [[ "${first_epoch}" -eq 0 ]]; then
    log "Run ${run_id} produced no usable samples."
    return 1
  fi

  local observed_seconds=$((last_epoch - first_epoch))
  if [[ "${observed_seconds}" -le 0 ]]; then
    observed_seconds=1
  fi

  local produced_delta=$((last_produced - first_produced))
  local mem_growth=$((last_mem - first_mem))
  local rss_growth=$((last_rss - first_rss))

  local throughput_eps mem_growth_mb rss_growth_mb leak_rate_mib_per_min leak_flag
  throughput_eps="$(awk -v d="${produced_delta}" -v t="${observed_seconds}" 'BEGIN { printf "%.6f", d / t }')"
  mem_growth_mb="$(awk -v b="${mem_growth}" 'BEGIN { printf "%.6f", b / 1048576 }')"
  rss_growth_mb="$(awk -v b="${rss_growth}" 'BEGIN { printf "%.6f", b / 1048576 }')"
  leak_rate_mib_per_min="$(awk -v b="${mem_growth}" -v t="${observed_seconds}" 'BEGIN { printf "%.6f", (b / 1048576) / (t / 60.0) }')"
  leak_flag="$(awk -v g="${mem_growth_mb}" -v th="${LEAK_THRESHOLD_MB}" 'BEGIN { if (g > th) print "true"; else print "false" }')"

  local summary_json="${run_dir}/summary.json"
  cat >"${summary_json}" <<EOF
{
  "run_id": "${run_id}",
  "cores": ${cores},
  "rate_signals_per_sec": ${rate},
  "target_events": ${THRESHOLD_EVENTS},
  "observed_seconds": ${observed_seconds},
  "produced_total_start": ${first_produced},
  "produced_total_end": ${last_produced},
  "produced_events": ${produced_delta},
  "throughput_events_per_sec": ${throughput_eps},
  "memory_usage_start_bytes": ${first_mem},
  "memory_usage_end_bytes": ${last_mem},
  "memory_growth_bytes": ${mem_growth},
  "memory_growth_mb": ${mem_growth_mb},
  "rss_start_bytes": ${first_rss},
  "rss_end_bytes": ${last_rss},
  "rss_growth_bytes": ${rss_growth},
  "rss_growth_mb": ${rss_growth_mb},
  "leak_rate_mib_per_min": ${leak_rate_mib_per_min},
  "leak_threshold_mb": ${LEAK_THRESHOLD_MB},
  "leak_flag": ${leak_flag},
  "stop_reason": "${stop_reason}",
  "run_dir": "${run_dir}"
}
EOF

  printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "${run_id}" "${cores}" "${rate}" "${THRESHOLD_EVENTS}" "${observed_seconds}" \
    "${produced_delta}" "${throughput_eps}" "${mem_growth_mb}" "${rss_growth_mb}" \
    "${leak_rate_mib_per_min}" "${leak_flag}" "${stop_reason}" "${run_dir}" \
    >>"${OUT_DIR}/summary.csv"

  log "Completed ${run_id}: throughput=${throughput_eps} evt/s, mem_growth=${mem_growth_mb} MB, leak=${leak_flag}"
}

main() {
  parse_args "$@"
  validate_args
  weights_for_signal_kind
  prepare_out_dir

  if [[ ! -x "${BINARY}" && "${SKIP_BUILD}" -eq 1 ]]; then
    echo "Binary not found or not executable: ${BINARY}" >&2
    exit 1
  fi

  build_binary

  if [[ ! -x "${BINARY}" ]]; then
    echo "Binary not found or not executable after build: ${BINARY}" >&2
    exit 1
  fi

  log "Artifacts directory: ${OUT_DIR}"
  cat >"${OUT_DIR}/run-meta.txt" <<EOF
timestamp_utc=$(timestamp_utc)
binary=${BINARY}
core_sweep=${CORE_SWEEP}
rate_sweep=${RATE_SWEEP}
run_seconds=${RUN_SECONDS}
sample_seconds=${SAMPLE_SECONDS}
threshold_events=${THRESHOLD_EVENTS}
batch_size=${BATCH_SIZE}
signal_kind=${SIGNAL_KIND}
data_source=${DATA_SOURCE}
generation_strategy=${GENERATION_STRATEGY}
leak_threshold_mb=${LEAK_THRESHOLD_MB}
http_base_port=${HTTP_BASE_PORT}
heap_profiler=${HEAP_PROFILER}
with_perf=${WITH_PERF}
numa_node=${NUMA_NODE}
EOF

  printf '%s\n' "run_id,cores,rate_signals_per_sec,target_events,observed_seconds,produced_events,throughput_events_per_sec,memory_growth_mb,rss_growth_mb,leak_rate_mib_per_min,leak_flag,stop_reason,run_dir" >"${OUT_DIR}/summary.csv"

  local -a core_arr rate_arr
  IFS=',' read -r -a core_arr <<<"${CORE_SWEEP}"
  IFS=',' read -r -a rate_arr <<<"${RATE_SWEEP}"

  local case_idx=0
  local cores rate
  for cores in "${core_arr[@]}"; do
    validate_positive_int "core_sweep entry" "${cores}"
    for rate in "${rate_arr[@]}"; do
      validate_positive_int "rate_sweep entry" "${rate}"
      local port=$((HTTP_BASE_PORT + case_idx))
      run_single_case "${case_idx}" "${cores}" "${rate}" "${port}"
      case_idx=$((case_idx + 1))
    done
  done

  log "All runs complete. Summary: ${OUT_DIR}/summary.csv"
}

main "$@"
