// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded CPU workload used by the environment-gated eBPF profiler smoke test.

use std::hint::black_box;
use std::time::{Duration, Instant};

#[inline(never)]
fn profile_leaf(seed: u64) -> u64 {
    let mut value = seed;
    for index in 0..100_000_u64 {
        value = value
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(index | 1)
            .rotate_left((index % 63) as u32);
    }
    black_box(value)
}

#[inline(never)]
fn profile_branch(seed: u64) -> u64 {
    profile_leaf(seed).wrapping_add(profile_leaf(seed ^ 0x9e37_79b9_7f4a_7c15))
}

#[inline(never)]
fn profile_root(seed: u64) -> u64 {
    profile_branch(seed).wrapping_add(profile_branch(seed.rotate_left(17)))
}

fn main() {
    let seconds = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(15)
        .clamp(1, 300);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut checksum = 0_u64;
    while Instant::now() < deadline {
        checksum = checksum.wrapping_add(profile_root(checksum | 1));
    }
    let _ = black_box(checksum);
}
