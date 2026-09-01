#!/usr/bin/env bash

set -euo pipefail

readonly PROFILER_IMAGE_DEFAULT="otel/opentelemetry-collector-ebpf-profiler:0.159.0@sha256:90d6b6536ce0283d706f7e7b6c45f534c65b140ff6ec456c19385e50a7d12b8e"
readonly WORKLOAD_IMAGE_DEFAULT="alpine:3.24@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"
readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly DATAFLOW_DIR="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly PROFILER_CONFIG="${SCRIPT_DIR}/ebpf-profiler-config.yaml"
readonly DATAFLOW_CONFIG="${DATAFLOW_DIR}/configs/profiles-ebpf-smoke.yaml"

readonly PROFILER_IMAGE="${OTEL_ARROW_EBPF_PROFILER_IMAGE:-${PROFILER_IMAGE_DEFAULT}}"
readonly WORKLOAD_IMAGE="${OTEL_ARROW_EBPF_WORKLOAD_IMAGE:-${WORKLOAD_IMAGE_DEFAULT}}"
readonly DURATION_SECONDS="${OTEL_ARROW_EBPF_DURATION_SECONDS:-15}"
readonly INGEST_PORT="${OTEL_ARROW_EBPF_INGEST_PORT:-14317}"
readonly SINK_PORT="${OTEL_ARROW_EBPF_SINK_PORT:-14318}"
readonly ADMIN_PORT="${OTEL_ARROW_EBPF_ADMIN_PORT:-18080}"
readonly CONTAINER_NAME="otel-arrow-ebpf-profiler-${$}"
readonly WORKLOAD_CONTAINER_NAME="otel-arrow-ebpf-workload-${$}"
readonly WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/otel-arrow-profiles-ebpf.XXXXXX")"
readonly BUFFER_PATH="${WORK_DIR}/buffer"
readonly DATAFLOW_LOG="${WORK_DIR}/df-engine.log"
readonly PROFILER_LOG="${WORK_DIR}/profiler.log"
readonly METRICS_JSON="${WORK_DIR}/metrics.json"
readonly STATIC_PROFILE_WORKLOAD="${WORK_DIR}/profile_workload"

df_engine_pid=""
profiler_pid=""
workload_pid=""

cleanup() {
  local exit_code=$?
  trap - EXIT

  if [[ -n "${workload_pid}" ]] && kill -0 "${workload_pid}" 2>/dev/null; then
    kill "${workload_pid}" 2>/dev/null || true
    wait "${workload_pid}" 2>/dev/null || true
  fi
  if docker inspect "${CONTAINER_NAME}" >/dev/null 2>&1; then
    docker stop --time 5 "${CONTAINER_NAME}" >/dev/null 2>&1 || true
  fi
  if docker inspect "${WORKLOAD_CONTAINER_NAME}" >/dev/null 2>&1; then
    docker stop --time 5 "${WORKLOAD_CONTAINER_NAME}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${profiler_pid}" ]] && kill -0 "${profiler_pid}" 2>/dev/null; then
    kill "${profiler_pid}" 2>/dev/null || true
    wait "${profiler_pid}" 2>/dev/null || true
  fi
  if [[ -n "${df_engine_pid}" ]] && kill -0 "${df_engine_pid}" 2>/dev/null; then
    kill "${df_engine_pid}" 2>/dev/null || true
    wait "${df_engine_pid}" 2>/dev/null || true
  fi

  case "${WORK_DIR}" in
    "${TMPDIR:-/tmp}"/otel-arrow-profiles-ebpf.*)
      rm -rf -- "${WORK_DIR}"
      ;;
  esac
  exit "${exit_code}"
}
trap cleanup EXIT

fail() {
  echo "error: $*" >&2
  exit 1
}

for command in cargo curl docker python3 rustc sort uname; do
  command -v "${command}" >/dev/null 2>&1 || fail "required command not found: ${command}"
done

[[ "${DURATION_SECONDS}" =~ ^[0-9]+$ ]] \
  || fail "OTEL_ARROW_EBPF_DURATION_SECONDS must be an integer"
((DURATION_SECONDS >= 1 && DURATION_SECONDS <= 300)) \
  || fail "OTEL_ARROW_EBPF_DURATION_SECONDS must be between 1 and 300"

case "$(uname -m)" in
  x86_64 | aarch64 | arm64) ;;
  *) fail "the profiler image supports only amd64 and arm64 hosts" ;;
esac

kernel_version="$(uname -r | cut -d- -f1)"
[[ "$(printf '%s\n' "5.10" "${kernel_version}" | sort -V | head -n1)" == "5.10" ]] \
  || fail "kernel 5.10 or newer is required; found ${kernel_version}"
[[ -d /sys/kernel/tracing ]] || fail "/sys/kernel/tracing is not available"
docker info >/dev/null 2>&1 || fail "Docker daemon is unavailable"
docker_operating_system="$(docker info --format '{{.OperatingSystem}}')"
docker_desktop_mode=false
if [[ "${docker_operating_system}" == *"Docker Desktop"* ]]; then
  docker_desktop_mode=true
fi
ingest_listen_host="127.0.0.1"
if [[ "${docker_desktop_mode}" == true ]]; then
  ingest_listen_host="0.0.0.0"
fi

if [[ "${OTEL_ARROW_EBPF_SKIP_BUILD:-0}" != "1" ]]; then
  (
    cd "${DATAFLOW_DIR}"
    cargo build --quiet --bin df_engine
    if [[ "${docker_desktop_mode}" == false ]]; then
      cargo build --quiet -p otel-arrow-dfe-validation --example profile_workload
    fi
  )
fi

readonly DF_ENGINE="${DATAFLOW_DIR}/target/debug/df_engine"
readonly PROFILE_WORKLOAD="${DATAFLOW_DIR}/target/debug/examples/profile_workload"
[[ -x "${DF_ENGINE}" ]] || fail "missing ${DF_ENGINE}; run without OTEL_ARROW_EBPF_SKIP_BUILD"
if [[ "${docker_desktop_mode}" == true ]]; then
  rustc \
    --edition=2024 \
    -C opt-level=1 \
    -C debuginfo=1 \
    -C target-feature=+crt-static \
    "${DATAFLOW_DIR}/crates/validation/examples/profile_workload.rs" \
    -o "${STATIC_PROFILE_WORKLOAD}"
  echo "Using Docker Desktop sidecar PID namespace mode"
else
  [[ -x "${PROFILE_WORKLOAD}" ]] \
    || fail "missing ${PROFILE_WORKLOAD}; run without OTEL_ARROW_EBPF_SKIP_BUILD"
  echo "Using native host PID namespace mode"
fi

mkdir -p "${BUFFER_PATH}"
OTEL_ARROW_EBPF_INGEST_HOST="${ingest_listen_host}" \
OTEL_ARROW_EBPF_INGEST_PORT="${INGEST_PORT}" \
OTEL_ARROW_EBPF_SINK_PORT="${SINK_PORT}" \
OTEL_ARROW_EBPF_ADMIN_PORT="${ADMIN_PORT}" \
OTEL_ARROW_EBPF_BUFFER_PATH="${BUFFER_PATH}" \
  "${DF_ENGINE}" --config "${DATAFLOW_CONFIG}" --num-cores 2 \
  >"${DATAFLOW_LOG}" 2>&1 &
df_engine_pid=$!

ready_url="http://127.0.0.1:${ADMIN_PORT}/api/v1/readyz"
for _ in $(seq 1 60); do
  if curl --fail --silent --show-error "${ready_url}" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "${df_engine_pid}" 2>/dev/null; then
    cat "${DATAFLOW_LOG}" >&2
    fail "df_engine exited before becoming ready"
  fi
  sleep 1
done
curl --fail --silent --show-error "${ready_url}" >/dev/null \
  || {
    cat "${DATAFLOW_LOG}" >&2
    fail "df_engine did not become ready"
  }

docker_args=(
  run
  --rm
  --name "${CONTAINER_NAME}"
  --cap-add BPF
  --cap-add PERFMON
  --cap-add SYS_PTRACE
  --cap-add SYS_RESOURCE
  --cap-add DAC_READ_SEARCH
  --cap-add SYSLOG
  --cap-add CHECKPOINT_RESTORE
  --cap-add IPC_LOCK
  --security-opt seccomp=unconfined
  --mount "type=bind,src=${PROFILER_CONFIG},dst=/etc/otelcol-ebpf-profiler/config.yaml,readonly"
  --mount "type=bind,src=/sys/kernel/tracing,dst=/sys/kernel/tracing,readonly"
)
profiler_endpoint="127.0.0.1:${INGEST_PORT}"
if [[ "${docker_desktop_mode}" == true ]]; then
  profiler_endpoint="host.docker.internal:${INGEST_PORT}"
else
  docker_args+=(--network host --pid host)
fi
docker_args+=(--env "OTEL_EXPORTER_OTLP_ENDPOINT=${profiler_endpoint}")
if [[ -r /sys/module/apparmor/parameters/enabled ]] \
  && grep -q '^Y' /sys/module/apparmor/parameters/enabled; then
  docker_args+=(--security-opt apparmor=unconfined)
fi
docker_args+=(
  "${PROFILER_IMAGE}"
  --feature-gates=+service.profilesSupport
  --config=/etc/otelcol-ebpf-profiler/config.yaml
)
if [[ "${docker_desktop_mode}" == true ]]; then
  docker_args+=(--set=receivers.profiling.pid_namespace_translation=true)
fi

docker "${docker_args[@]}" >"${PROFILER_LOG}" 2>&1 &
profiler_pid=$!

for _ in $(seq 1 60); do
  if grep -q "Attached sched monitor" "${PROFILER_LOG}"; then
    break
  fi
  if ! kill -0 "${profiler_pid}" 2>/dev/null; then
    cat "${PROFILER_LOG}" >&2
    fail "eBPF profiler exited before attaching"
  fi
  sleep 1
done
grep -q "Attached sched monitor" "${PROFILER_LOG}" \
  || {
    cat "${PROFILER_LOG}" >&2
    fail "eBPF profiler did not attach within 60 seconds"
  }

if [[ "${docker_desktop_mode}" == true ]]; then
  docker run \
    --rm \
    --name "${WORKLOAD_CONTAINER_NAME}" \
    --pid "container:${CONTAINER_NAME}" \
    --mount "type=bind,src=${STATIC_PROFILE_WORKLOAD},dst=/profile_workload,readonly" \
    "${WORKLOAD_IMAGE}" \
    /profile_workload "${DURATION_SECONDS}" \
    >"${WORK_DIR}/workload.log" 2>&1
else
  "${PROFILE_WORKLOAD}" "${DURATION_SECONDS}" >"${WORK_DIR}/workload.log" 2>&1 &
  workload_pid=$!
  wait "${workload_pid}"
  workload_pid=""
fi

metrics_url="http://127.0.0.1:${ADMIN_PORT}/api/v1/metrics?format=json_compact"
for _ in $(seq 1 30); do
  if ! curl --fail --silent --show-error "${metrics_url}" >"${METRICS_JSON}"; then
    sleep 1
    continue
  fi
  if python3 - "${METRICS_JSON}" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as metrics_file:
    snapshot = json.load(metrics_file)


def text(value):
    if isinstance(value, str):
        return value
    if isinstance(value, dict) and len(value) == 1:
        return text(next(iter(value.values())))
    return ""


def numeric(value):
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, dict):
        return max((numeric(item) for item in value.values()), default=0.0)
    if isinstance(value, list):
        return max((numeric(item) for item in value), default=0.0)
    return 0.0


observed = set()
for metric_set in snapshot.get("metric_sets", []):
    if metric_set.get("name") != "receiver.otlp.requests":
        continue
    attributes = metric_set.get("attributes") or {}
    data_point_attributes = metric_set.get("data_point_attributes") or {}
    if text(data_point_attributes.get("signal")).lower() != "profiles":
        continue
    metrics = metric_set.get("metrics") or {}
    if any(numeric(value) > 0 for value in metrics.values()):
        observed.add(text(attributes.get("node.id")))

required = {"source_receiver", "sink_receiver"}
if required.issubset(observed):
    print("observed non-empty Profiles at: " + ", ".join(sorted(observed)))
    raise SystemExit(0)
raise SystemExit(1)
PY
  then
    echo "eBPF Profiles smoke test passed"
    exit 0
  fi
  sleep 1
done

cat "${PROFILER_LOG}" >&2
cat "${DATAFLOW_LOG}" >&2
fail "non-empty Profiles did not traverse both OTLP receivers"
