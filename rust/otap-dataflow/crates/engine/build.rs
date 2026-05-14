// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Builds optional Linux eBPF objects for the engine crate.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=bpf/reuseport_numa_kern.c");
    println!("cargo:rerun-if-changed=bpf/vmlinux.h");
    println!("cargo:rerun-if-env-changed=CLANG");
    println!("cargo:rerun-if-env-changed=OTAP_DF_REUSEPORT_EBPF_GENERATE_VMLINUX_H");
    println!("cargo:rerun-if-env-changed=OTAP_DF_REUSEPORT_EBPF_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=OTAP_DF_REUSEPORT_EBPF_VMLINUX_H");

    if env::var_os("CARGO_FEATURE_REUSEPORT_EBPF").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let bpf_dir = manifest_dir.join("bpf");
    let source = bpf_dir.join("reuseport_numa_kern.c");
    let object = out_dir.join("reuseport_numa_kern.bpf.o");
    let vmlinux_include_dir = prepare_vmlinux_header(&bpf_dir, &out_dir);
    let helper_include_dirs = bpf_helper_include_dirs();

    let mut clang = Command::new(env::var_os("CLANG").unwrap_or_else(|| "clang".into()));
    let _ = clang
        .arg("-g")
        .arg("-O2")
        .arg("-target")
        .arg(bpf_llvm_target())
        // BPF ISA v3 enables BPF_ATOMIC_FETCH_ADD, required by the
        // `__sync_fetch_and_add` use in the selector. Needs clang >= 12 and
        // Linux >= 5.12.
        .arg("-mcpu=v3")
        .arg(format!("-D__TARGET_ARCH_{}", bpf_target_arch()))
        .arg("-I")
        .arg(&vmlinux_include_dir)
        .arg("-I")
        .arg(&bpf_dir);
    for include_dir in helper_include_dirs {
        let _ = clang.arg("-I").arg(include_dir);
    }
    let _ = clang.arg("-c").arg(&source).arg("-o").arg(&object);

    let output = clang
        .output()
        .expect("failed to execute clang for eBPF build");
    if !output.status.success() {
        panic!(
            "failed to compile {} with clang:\nstdout:\n{}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!(
        "cargo:rustc-env=OTAP_DF_REUSEPORT_EBPF_OBJECT={}",
        object.display()
    );
}

fn prepare_vmlinux_header(bpf_dir: &Path, out_dir: &Path) -> PathBuf {
    if let Some(path) = env::var_os("OTAP_DF_REUSEPORT_EBPF_VMLINUX_H") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return path;
        }
        let generated = out_dir.join("vmlinux.h");
        let _bytes_copied = fs::copy(&path, &generated).unwrap_or_else(|error| {
            panic!("failed to copy vmlinux.h from {}: {error}", path.display())
        });
        return out_dir.to_path_buf();
    }

    let checked_in = bpf_dir.join("vmlinux.h");
    if checked_in.exists() {
        return bpf_dir.to_path_buf();
    }

    if env::var("OTAP_DF_REUSEPORT_EBPF_GENERATE_VMLINUX_H").as_deref() != Ok("1") {
        panic!(
            "vmlinux.h is required to build reuseport eBPF; set OTAP_DF_REUSEPORT_EBPF_VMLINUX_H or opt in to bpftool generation with OTAP_DF_REUSEPORT_EBPF_GENERATE_VMLINUX_H=1"
        );
    }

    let output = Command::new("bpftool")
        .args([
            "btf",
            "dump",
            "file",
            "/sys/kernel/btf/vmlinux",
            "format",
            "c",
        ])
        .output()
        .expect("failed to execute bpftool to generate vmlinux.h");
    if !output.status.success() {
        panic!(
            "failed to generate vmlinux.h with bpftool from /sys/kernel/btf/vmlinux:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::write(out_dir.join("vmlinux.h"), output.stdout)
        .expect("failed to write generated vmlinux.h");
    out_dir.to_path_buf()
}

fn bpf_helper_include_dirs() -> Vec<PathBuf> {
    let mut include_dirs = Vec::new();
    if let Some(raw) = env::var_os("OTAP_DF_REUSEPORT_EBPF_INCLUDE_DIR") {
        include_dirs.extend(env::split_paths(&raw));
    }

    if let Ok(output) = Command::new("pkg-config")
        .args(["--cflags", "libbpf"])
        .output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for flag in stdout.split_whitespace() {
            if let Some(path) = flag.strip_prefix("-I") {
                include_dirs.push(PathBuf::from(path));
            }
        }
    }

    include_dirs
}

fn bpf_llvm_target() -> &'static str {
    match env::var("CARGO_CFG_TARGET_ENDIAN")
        .expect("CARGO_CFG_TARGET_ENDIAN")
        .as_str()
    {
        "big" => "bpfeb",
        "little" => "bpfel",
        target_endian => panic!("unsupported eBPF target endianness `{target_endian}`"),
    }
}

fn bpf_target_arch() -> String {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    match target_arch.as_str() {
        "aarch64" => "arm64".to_string(),
        "arm" => "arm".to_string(),
        "powerpc64" => "powerpc".to_string(),
        "riscv64" => "riscv".to_string(),
        "s390x" => "s390".to_string(),
        "x86" | "x86_64" => "x86".to_string(),
        _ => panic!("unsupported eBPF target architecture `{target_arch}`"),
    }
}
