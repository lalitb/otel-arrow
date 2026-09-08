// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deterministic CPU workload for future Go/Rust profiler comparison.

use std::time::{Duration, Instant};

#[inline(never)]
fn leaf(seed: u64) -> u64 {
    let mut value = seed;
    for _ in 0..256 {
        value = std::hint::black_box(
            value
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407),
        );
    }
    std::hint::black_box(value)
}

#[inline(never)]
fn descend(depth: u32, seed: u64) -> u64 {
    if depth == 0 {
        leaf(seed)
    } else {
        let value = descend(depth - 1, seed.wrapping_add(u64::from(depth)));
        // Work after the recursive call prevents tail-call elimination.
        std::hint::black_box(value).wrapping_add(u64::from(depth))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let duration = Duration::from_secs(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "10".to_owned())
            .parse()?,
    );
    let deadline = Instant::now()
        .checked_add(duration)
        .ok_or("duration exceeds Instant range")?;
    let mut seed = 1_u64;
    while Instant::now() < deadline {
        // Avoid spending a material share of samples in the vDSO clock reader.
        for _ in 0..1024 {
            seed = descend(32, seed);
        }
    }
    let _result = std::hint::black_box(seed);
    Ok(())
}
