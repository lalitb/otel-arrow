#!/usr/bin/env bash
# Copyright The OpenTelemetry Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail
export SOURCE_DATE_EPOCH=0

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source_file="${script_dir}/src/profiler.bpf.c"
output_file=${1:-"${script_dir}/target/profiler.bpf.o"}
architecture=${BPF_TARGET_ARCH:-$(uname -m)}
clang=${CLANG:-clang}

case "${architecture}" in
    x86_64|amd64)
        target_define=__TARGET_ARCH_x86
        ;;
    aarch64|arm64)
        target_define=__TARGET_ARCH_arm64
        ;;
    *)
        printf 'unsupported BPF target architecture: %s\n' "${architecture}" >&2
        exit 2
        ;;
esac

if ! command -v "${clang}" >/dev/null 2>&1; then
    printf 'clang with the BPF backend is required to build %s\n' "${source_file}" >&2
    exit 3
fi

mkdir -p -- "$(dirname -- "${output_file}")"
"${clang}" \
    -target bpfel \
    -D"${target_define}" \
    -O2 \
    -g \
    -gno-record-command-line \
    "-ffile-prefix-map=${script_dir}=." \
    "-fdebug-prefix-map=${script_dir}=." \
    -fdebug-compilation-dir=. \
    -fno-ident \
    -Wall \
    -Werror \
    -c "${source_file}" \
    -o "${output_file}"

case "${architecture}" in
    amd64)
        manifest_architecture=x86_64
        ;;
    arm64)
        manifest_architecture=aarch64
        ;;
    *)
        manifest_architecture=${architecture}
        ;;
esac

manifest_file="${output_file}.manifest"
source_sha256=$(cat "${source_file}" "${script_dir}/src/abi.h" | sha256sum | awk '{print $1}')
object_sha256=$(sha256sum "${output_file}" | awk '{print $1}')
compiler=$("${clang}" --version | sed -n '1p')
compiler_sha256=$(sha256sum "$(command -v "${clang}")" | awk '{print $1}')
printf '%s\n' \
    'format=otel-ebpf-profiler-object-v1' \
    'abi_version=1' \
    'endianness=little' \
    'timestamp_clock=BOOTTIME' \
    "architecture=${manifest_architecture}" \
    "source_sha256=${source_sha256}" \
    "object_sha256=${object_sha256}" \
    'program_user=profile_cpu' \
    'program_user_kernel=profile_cpu_kernel' \
    'events_map=EVENTS' \
    'counters_map=COUNTERS' \
    'license=GPL-2.0-only' \
    "compiler_sha256=${compiler_sha256}" \
    "compiler=${compiler}" >"${manifest_file}"

printf '%s\n' "${output_file}"
printf '%s\n' "${manifest_file}"
