// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Frame-pointer workload spending substantial CPU time inside read(2).

use std::fs::File;
use std::io::{self, Read};
use std::time::{Duration, Instant};

#[inline(never)]
fn descend(depth: u32, input: &mut File, buffer: &mut [u8]) -> io::Result<u64> {
    if depth == 0 {
        input.read_exact(buffer)?;
        Ok(u64::from(std::hint::black_box(buffer[0])))
    } else {
        let value = descend(depth - 1, input, buffer)?;
        Ok(std::hint::black_box(value).wrapping_add(u64::from(depth)))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "10".to_owned())
        .parse::<u64>()?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .ok_or("duration exceeds monotonic clock range")?;
    let mut input = File::open("/dev/zero")?;
    let mut buffer = vec![0_u8; 64 * 1024];
    while Instant::now() < deadline {
        for _ in 0..256 {
            let _value = std::hint::black_box(descend(24, &mut input, &mut buffer)?);
        }
    }
    Ok(())
}
