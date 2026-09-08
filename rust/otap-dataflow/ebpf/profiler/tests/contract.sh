#!/usr/bin/env bash
# Copyright The OpenTelemetry Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
workspace=$(cd -- "${script_dir}/../.." && pwd)
target="${script_dir}/target"
cc=${CC:-cc}
mkdir -p -- "${target}/test-files"
export TMPDIR="${target}/test-files"

# Scenario: Each target is compiled twice in different output directories.
# Guarantees: Pinned compiler/source inputs produce byte-identical BPF artifacts.
for architecture in x86_64 aarch64; do
    BPF_TARGET_ARCH=${architecture} bash "${script_dir}/build.sh" "${target}/${architecture}/profiler.bpf.o"
    BPF_TARGET_ARCH=${architecture} bash "${script_dir}/build.sh" "${target}/repeat/${architecture}/profiler.bpf.o"
    cmp "${target}/${architecture}/profiler.bpf.o" "${target}/repeat/${architecture}/profiler.bpf.o"
    cmp "${target}/${architecture}/profiler.bpf.o.manifest" "${target}/repeat/${architecture}/profiler.bpf.o.manifest"
done

# Scenario: The shared C header is used by a native producer, not Rust encoding.
# Guarantees: Decoder tests check independently produced little-endian wire bytes.
"${cc}" -std=c11 -Wall -Werror "${script_dir}/tests/golden.c" -o "${target}/golden-producer"
"${target}/golden-producer" >"${target}/golden.bin"
"${cc}" -std=c11 -O2 -Wall -Werror -Wno-unknown-pragmas \
    "${script_dir}/tests/kernel_contract.c" -o "${target}/kernel-contract"
"${target}/kernel-contract" >"${target}/kernel-golden.bin"
cmp "${target}/golden.bin" "${target}/kernel-golden.bin"
"${cc}" -O2 -g -fno-omit-frame-pointer -fno-optimize-sibling-calls \
    -Wall -Werror "${script_dir}/tests/workload.c" -o "${target}/native-workload"

case "$(uname -m)" in
    x86_64) host=x86_64; cross=aarch64 ;;
    aarch64) host=aarch64; cross=x86_64 ;;
    *) printf 'unsupported native test architecture\n' >&2; exit 2 ;;
esac

cd -- "${workspace}"
export OTEL_EBPF_PROFILER_GOLDEN="${target}/kernel-golden.bin"
export OTEL_EBPF_PROFILER_CONTRACT_OBJECT="${target}/${host}/profiler.bpf.o"
export OTEL_EBPF_PROFILER_CROSS_OBJECT="${target}/${cross}/profiler.bpf.o"
cargo test -p otel-arrow-dfe-ebpf-profiler --lib --test real_kernel --quiet

# Use Cargo's selected artifact, not a guessed registry version or stale glob.
aya_object=$(cargo build -p otel-arrow-dfe-ebpf-profiler --message-format=json --quiet |
    python3 -c '
import json, sys
artifact = None
for line in sys.stdin:
    item = json.loads(line)
    if item.get("reason") == "compiler-artifact" and item["target"]["name"] == "aya_obj":
        artifact = next((name for name in item["filenames"] if name.endswith(".rlib")), None)
if artifact is None:
    sys.exit("Cargo did not report its aya-obj library")
print(artifact)
')
rustc --edition=2024 --test "${script_dir}/tests/aya_contract.rs" \
    --extern "aya_obj=${aya_object}" -L "dependency=$(dirname -- "${aya_object}")" \
    -o "${target}/aya-contract"
"${target}/aya-contract"
sha256sum "${target}/x86_64/profiler.bpf.o" "${target}/aarch64/profiler.bpf.o" "${target}/golden.bin"
