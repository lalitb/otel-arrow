// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Ownership, semantic identity, and graph integrity during finalization.

use std::time::{Duration, SystemTime};

use super::*;
use crate::{
    FunctionRecord, LocationRecord, MappingKind, MappingRecord, ProcessRecord, ProfilerStatistics,
    SampleRecord, StackRecord, ThreadRecord,
};

fn fixture(pid: u32, generation: u64, cpu: u32, mapped: bool) -> ProfileSnapshot {
    let mut snapshot = ProfileSnapshot::empty(
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        Duration::from_millis(10),
    );
    snapshot.processes.push(ProcessRecord {
        pid,
        start_time_ticks: generation,
        name: "workload".to_owned(),
        executable: None,
    });
    snapshot.threads.push(ThreadRecord {
        process: ProcessId(0),
        tid: pid,
        start_time_ticks: generation,
        name: None,
    });
    if mapped {
        snapshot.mappings.push(MappingRecord {
            process: ProcessId(0),
            start: 0x1000,
            end: 0x2000,
            file_offset: 0,
            path: Some("/fixture".to_owned()),
            executable: true,
            kind: MappingKind::File,
            file_identity: crate::MappingFileIdentity {
                device_major: 0,
                device_minor: 0,
                inode: 1,
            },
            path_truncated: false,
        });
        snapshot.functions.push(FunctionRecord {
            mapping: MappingId(0),
            address: 0x10,
            name: "work".to_owned(),
            filename: None,
            line: None,
        });
    }
    snapshot.locations.push(LocationRecord {
        process: Some(ProcessId(0)),
        mapping: mapped.then_some(MappingId(0)),
        address: 0x20,
        function: mapped.then_some(FunctionId(0)),
        kernel: false,
    });
    snapshot.stacks.push(StackRecord {
        process: Some(ProcessId(0)),
        kernel: false,
        locations: vec![LocationId(0)],
        truncated: false,
        collection_error: None,
    });
    snapshot.samples.push(SampleRecord {
        process: ProcessId(0),
        thread: ThreadId(0),
        user_stack: StackId(0),
        kernel_stack: None,
        count: 2,
        first_timestamp_ns: 1,
        last_timestamp_ns: 2,
        cpu,
    });
    snapshot.statistics = ProfilerStatistics {
        raw_events_received: 2,
        samples_accepted: 2,
        samples_aggregated: 1,
        samples_in_snapshots: 2,
        ..ProfilerStatistics::default()
    };
    snapshot
}

/// Scenario: Two shards contain an identical owned graph and sample identity.
/// Guarantees: Tables deduplicate by full equality and counts reaggregate.
#[test]
fn cross_shard_graphs_deduplicate_and_reaggregate() {
    let merged = merge_snapshots(
        vec![fixture(1, 1, 0, true), fixture(1, 1, 0, true)],
        &Limits::default(),
    )
    .expect("valid graphs merge");
    merged.validate().expect("merged graph is valid");
    assert_eq!(
        (
            merged.processes.len(),
            merged.threads.len(),
            merged.mappings.len()
        ),
        (1, 1, 1)
    );
    assert_eq!(
        (
            merged.functions.len(),
            merged.locations.len(),
            merged.stacks.len()
        ),
        (1, 1, 1)
    );
    assert_eq!(merged.samples.len(), 1);
    assert_eq!(merged.samples[0].count, 4);
    assert_eq!(merged.statistics.samples_in_snapshots, 4);
}

/// Scenario: A migrating process is observed on two shards' distinct CPUs.
/// Guarantees: Shared metadata deduplicates, but CPU remains a sample dimension.
#[test]
fn migration_preserves_cpu_dimension() {
    let merged = merge_snapshots(
        vec![fixture(1, 1, 0, true), fixture(1, 1, 1, true)],
        &Limits::default(),
    )
    .expect("valid graphs merge");
    assert_eq!(merged.processes.len(), 1);
    assert_eq!(merged.stacks.len(), 1);
    assert_eq!(merged.samples.len(), 2);
}

/// Scenario: A PID is reused or different processes have equal unmapped addresses.
/// Guarantees: Unresolved frames and empty mapping state cannot collapse identity.
#[test]
fn unmapped_addresses_keep_process_generation() {
    let merged = merge_snapshots(
        vec![
            fixture(1, 1, 0, false),
            fixture(1, 2, 0, false),
            fixture(2, 1, 0, false),
        ],
        &Limits::default(),
    )
    .expect("valid graphs merge");
    assert_eq!(merged.processes.len(), 3);
    assert_eq!(merged.locations.len(), 3);
    assert_eq!(merged.stacks.len(), 3);
}

/// Scenario: Function names match in two different executable mapping instances.
/// Guarantees: Function identity retains the defining mapping, not just its name.
#[test]
fn same_symbol_name_does_not_merge_different_mappings() {
    let first = fixture(1, 1, 0, true);
    let mut second = fixture(1, 1, 0, true);
    second.mappings[0].path = Some("/other-fixture".to_owned());
    let merged =
        merge_snapshots(vec![first, second], &Limits::default()).expect("valid graphs merge");
    assert_eq!(merged.functions.len(), 2);
    assert_eq!(merged.locations.len(), 2);
}

/// Scenario: A window's sample count differs from its collection statistics.
/// Guarantees: A misleading profile cannot pass graph/accounting validation.
#[test]
fn inconsistent_sample_accounting_is_rejected() {
    let mut snapshot = fixture(1, 1, 0, false);
    snapshot.statistics.samples_in_snapshots = 1;
    assert!(snapshot.validate().is_err());
}

/// Scenario: A sample's thread belongs to another process row.
/// Guarantees: In-range IDs alone do not satisfy snapshot graph integrity.
#[test]
fn cross_process_thread_reference_is_rejected() {
    let mut snapshot = fixture(1, 1, 0, false);
    snapshot.processes.push(ProcessRecord {
        pid: 2,
        start_time_ticks: 1,
        name: "other".to_owned(),
        executable: None,
    });
    snapshot.threads[0].process = ProcessId(1);
    assert!(snapshot.validate().is_err());
}

/// Scenario: A user stack contains a kernel location with otherwise valid IDs.
/// Guarantees: Stack kind and frame ownership are validated together.
#[test]
fn mixed_user_and_kernel_stack_is_rejected() {
    let mut snapshot = fixture(1, 1, 0, false);
    snapshot.locations[0].process = None;
    snapshot.locations[0].kernel = true;
    assert!(snapshot.validate().is_err());
}

/// Scenario: Two valid input snapshots exceed an independently supplied merge cap.
/// Guarantees: Finalization rejects the merge before growing a table past its cap.
#[test]
fn global_merge_limits_are_checked() {
    let limits = Limits {
        max_processes: 1,
        ..Limits::default()
    };
    assert!(matches!(
        merge_snapshots(
            vec![fixture(1, 1, 0, false), fixture(2, 1, 0, false)],
            &limits
        ),
        Err(ProfilerError::Capacity(DropReason::SnapshotCapacity)),
    ));
}

/// Scenario: The consumer moves every table out of a finalized snapshot.
/// Guarantees: No reference into an active aggregation cache is needed.
#[test]
fn consumer_moves_tables_without_cloning() {
    let snapshot = fixture(1, 1, 0, true);
    let count = snapshot
        .samples
        .into_iter()
        .map(|sample| sample.count)
        .sum::<u64>();
    let names = snapshot
        .processes
        .into_iter()
        .map(|process| process.name)
        .collect::<Vec<_>>();
    assert_eq!(count, 2);
    assert_eq!(names, vec!["workload".to_owned()]);
}
