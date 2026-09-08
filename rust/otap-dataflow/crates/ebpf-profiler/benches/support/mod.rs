// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deterministic bounded inputs shared by profiler benchmarks.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use otel_arrow_dfe_ebpf_profiler::{
    AggregationWindow, EventFlags, Limits, MAX_ABI_STACK_DEPTH, MappingTable, ProcessIdentity,
    ProcessMetadata, ProcessProvider, RawSample, ResolvedSymbol, Result, SymbolResolver,
    ThreadMetadata,
};

struct Processes;
impl ProcessProvider for Processes {
    fn identity(&self, pid: u32) -> Result<ProcessIdentity> {
        Ok(ProcessIdentity {
            pid,
            start_time_ticks: 1,
        })
    }
    fn process(&self, pid: u32) -> Result<ProcessMetadata> {
        Ok(ProcessMetadata {
            identity: self.identity(pid)?,
            name: "workload".to_owned(),
            executable: None,
            mappings: MappingTable::default(),
            issues: otel_arrow_dfe_ebpf_profiler::MetadataIssues::default(),
        })
    }
    fn thread(&self, process: ProcessIdentity, tid: u32) -> Result<ThreadMetadata> {
        Ok(ThreadMetadata {
            process,
            tid,
            start_time_ticks: 1,
            name: None,
            issues: otel_arrow_dfe_ebpf_profiler::MetadataIssues::default(),
        })
    }
}

struct Symbols;
impl SymbolResolver for Symbols {
    fn resolve(&mut self, _path: &Path, _offset: u64) -> Result<Option<ResolvedSymbol>> {
        Ok(None)
    }
}

pub fn window() -> AggregationWindow {
    AggregationWindow::new(
        Limits::default(),
        Duration::from_millis(10),
        SystemTime::UNIX_EPOCH,
        Arc::new(Processes),
        Box::new(Symbols),
    )
    .expect("benchmark limits are valid")
}

pub fn sample(tid: u32, kernel: bool) -> RawSample {
    let mut user_frames = [0; MAX_ABI_STACK_DEPTH];
    user_frames[..4].copy_from_slice(&[0x1100 + u64::from(tid) * 8, 0x1200, 0x1300, 0x1400]);
    let mut kernel_frames = [0; MAX_ABI_STACK_DEPTH];
    if kernel {
        kernel_frames[..2].copy_from_slice(&[0xffff_1000, 0xffff_2000]);
    }
    RawSample {
        pid: 1,
        tid,
        cpu: 0,
        timestamp_ns: 1,
        flags: EventFlags::empty(),
        user_stack_error: 0,
        kernel_stack_error: 0,
        user_depth: 4,
        kernel_depth: if kernel { 2 } else { 0 },
        user_frames,
        kernel_frames,
    }
}

pub fn populated(count: u32) -> AggregationWindow {
    let mut window = window();
    for tid in 1..=count {
        assert!(
            window
                .record(&sample(tid, false))
                .expect("benchmark sample is valid"),
            "benchmark must measure admitted samples, not capacity rejection"
        );
    }
    window
}
