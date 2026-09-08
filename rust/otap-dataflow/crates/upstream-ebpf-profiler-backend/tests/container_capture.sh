#!/usr/bin/env bash
# Copyright The OpenTelemetry Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

# Scenario: An authorized operator exercises the external artifact on a synthetic container workload.
# Guarantees: Capture is namespace-filtered, time/resource-bounded, read-only, offline, and auto-cleaned.
main() {
    if [[ "${OTEL_ARROW_EBPF_CONTAINER_TEST:-}" != "1" ]]; then
        printf 'Set OTEL_ARROW_EBPF_CONTAINER_TEST=1 only after authorizing privileged container testing.\n' >&2
        return 2
    fi
    if [[ "$#" -lt 3 || "$#" -gt 5 ]]; then
        printf 'usage: container_capture.sh <capture-binary> <workload-binary> <artifact> [seconds] [cpu-ids]\n' >&2
        return 2
    fi
    local capture="$1" workload="$2" artifact="$3" seconds="${4:-3}" cpu="${5:-0}"
    if [[ ! "$seconds" =~ ^[1-9][0-9]?$ ]] || (( seconds > 30 )); then
        printf 'capture duration must be 1 through 30 seconds\n' >&2
        return 2
    fi
    if [[ ! "$cpu" =~ ^[0-9]+(,[0-9]+)*$ ]]; then
        printf 'cpu-ids must be a comma-separated numeric CPU list\n' >&2
        return 2
    fi
    local -a selected_cpus
    IFS=, read -r -a selected_cpus <<< "$cpu"
    if (( ${#selected_cpus[@]} > 16 )); then
        printf 'at most 16 CPUs may be selected\n' >&2
        return 2
    fi
    for path in "$capture" "$workload" "$artifact"; do
        if [[ "$path" != /* || ! -f "$path" ]]; then
            printf 'expected an existing absolute file path: %s\n' "$path" >&2
            return 2
        fi
    done
    local timeout_seconds=$((seconds + 20))
    docker run --rm --init --network none --privileged --read-only \
        --cpuset-cpus "$cpu" --memory 512m --pids-limit 64 \
        --ulimit memlock=134217728:134217728 \
        --label otel-arrow.experiment=upstream-ebpf \
        -e OTEL_ARROW_EBPF_CAPTURE_DIAGNOSTICS=1 \
        --mount "type=bind,source=$capture,target=/experiment/capture,readonly" \
        --mount "type=bind,source=$workload,target=/experiment/workload,readonly" \
        --mount "type=bind,source=$artifact,target=/experiment/tracer.ebpf.amd64,readonly" \
        --entrypoint /usr/bin/timeout \
        ubuntu@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b \
        --signal=TERM --kill-after=5s "${timeout_seconds}s" /bin/sh -c \
        'set -eu
         if ! test -r /sys/kernel/tracing/events/sched/sched_process_free/id; then
             mount -t tracefs -o ro tracefs /sys/kernel/tracing
         fi
         for cpu in $(printf "%s" "$3" | tr "," " "); do
             taskset -c "$cpu" /experiment/workload "$1" >/dev/null &
         done
         /experiment/capture /experiment/tracer.ebpf.amd64 "$2" "$3" current' \
        sh "$timeout_seconds" "$seconds" "$cpu"
}

main "$@"
