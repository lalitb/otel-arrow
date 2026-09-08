// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded, runtime-neutral native profiling primitives and lifecycle.
//!
//! The crate owns Linux sampling, NUMA-aware shard planning, bounded
//! aggregation, metadata collection, and transport-neutral snapshots. It has
//! no dependency on the dataflow engine, OTAP, Arrow, OTLP, protobuf, or an
//! async runtime.

mod aggregate;
mod config;
mod coordinator;
mod error;
mod event;
mod finalize;
mod limits;
mod mappings;
mod process;
mod sampler;
mod shard;
mod snapshot;
mod statistics;
mod symbols;
mod topology;

#[cfg(target_os = "linux")]
mod linux;

pub use aggregate::AggregationWindow;
pub use config::{
    AffinityMode, CpuSelection, FailureMode, Limits, ProcessConfig, ProfilerConfig, ProgramConfig,
    ShardingStrategy, SymbolizationConfig, ValidatedConfig,
};
pub use coordinator::{
    Clock, Profiler, ProfilerBuilder, RuntimeFailure, ShutdownReport, SnapshotReceiver,
    SystemClock, WorkerAffinity,
};
pub use error::{ProfilerError, Result};
pub use event::{
    ABI_EVENT_SIZE, ABI_HEADER_SIZE, ABI_VERSION, EventFlags, MAX_ABI_STACK_DEPTH, RawSample,
    decode_event, encode_event,
};
pub use limits::MemoryEstimate;
pub use mappings::{
    ExecutableMapping, MappingFileIdentity, MappingKind, MappingTable, parse_proc_maps,
};
pub use process::{
    MetadataIssues, ProcessIdentity, ProcessMetadata, ProcessProvider, ProcfsProcessProvider,
    ThreadMetadata,
};
pub use sampler::{
    EventBatch, EventSource, Platform, PlatformGuard, PlatformStart, PreparedPlatform,
    SyntheticEventSource, SyntheticPlatform,
};
pub use shard::{ShardPlan, ShardPlanner};
pub use snapshot::{
    FunctionId, FunctionRecord, LocationId, LocationRecord, MappingId, MappingRecord, ProcessId,
    ProcessRecord, ProfileKind, ProfileSnapshot, SampleRecord, SnapshotAttributes, StackId,
    StackRecord, ThreadId, ThreadRecord, TimestampClock, UserUnwinding,
};
pub use statistics::{DropReason, ProfilerStatistics};
pub use symbols::{ObjectSymbolResolver, ResolvedSymbol, SymbolResolver, SymbolStatistics};
pub use topology::{CpuInfo, NumaNodeId, SystemTopology, TopologyProvider};
