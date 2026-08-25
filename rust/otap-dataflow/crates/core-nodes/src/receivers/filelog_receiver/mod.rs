// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Filelog receiver (Phase 1, under construction).
//!
//! This module currently ships the following Phase 1 implementation-plan
//! deliverables:
//!
//! - Stage 2: the exact version-1 durable checkpoint byte format and its
//!   codec, specified in
//!   [`docs/filelog-checkpoint-format.md`](../../../../../../docs/filelog-checkpoint-format.md)
//!   and referenced from
//!   [`docs/filelog-receiver.md`](../../../../../../docs/filelog-receiver.md)
//!   Appendix B.
//! - The user-facing configuration schema, its validation into a
//!   runtime-ready form, and the shared logical-record-size function,
//!   specified in `docs/filelog-receiver.md` Appendix C.
//! - This stage: the durable, file-backed checkpoint store built on that
//!   codec ([`checkpoint::store`]) -- namespace ownership locking,
//!   generation selection and recovery, WAL appends and sync policy,
//!   atomic compaction, and retention -- specified in
//!   `docs/filelog-receiver.md` Appendix B.
//! - The process-local runtime-locator lease registry ([`lease`]), which
//!   gives each receiver a preallocated, bounded ownership scope and prevents
//!   two filelog readers in one engine process from controlling the same live
//!   file.
//! - Secure handle-based identity evidence plus durable recovery matching
//!   ([`identity`]), including exact-locator and guarded unique-fingerprint
//!   reconnect, start/mismatch policy, and atomic registration.
//! - Bounded periodic filesystem discovery and admission ([`discovery`]),
//!   including compiled include/exclude globs, resolved-target safety,
//!   incomplete-inventory fail-closed behavior, overflow fairness, and a
//!   dedicated cancellable OS thread with bounded channels.
//! - Fair, bounded logical-reader scheduling ([`reader`]), including
//!   source-byte read turns, least-recently-served descriptor rotation,
//!   exact identity revalidation on reopen, and runtime leases that survive
//!   temporary descriptor closure.
//! - Constant-state source decoding and bounded newline/multiline framing
//!   ([`framing`]), including exact source-byte evidence, EOF-gated partial
//!   flushes, split/truncate policies, and durable fragment continuation.
//! - Exact OTAP projection and bounded open-batch construction
//!   ([`batching`]), including shared logical sizing, contiguous Ack deltas,
//!   recordless finalization, and transactional worker-local record numbers.
//!
//! - Stage 11: the dedicated read/checkpoint worker, one retained
//!   receiver-wide batch, Ack/Nack retry and commit protocol, interruptible
//!   backpressure/drain lifecycle, and local receiver factory.
#![allow(dead_code)] // Phase 1 internals intentionally land before the receiver factory.

use std::sync::Arc;

use linkme::distributed_slice;
use otap_df_config::node::NodeUserConfig;
use otap_df_engine::ReceiverFactory;
use otap_df_engine::config::ReceiverConfig;
use otap_df_engine::context::PipelineContext;
use otap_df_engine::node::NodeId;
use otap_df_engine::receiver::ReceiverWrapper;
use otap_df_otap::OTAP_RECEIVER_FACTORIES;
use otap_df_otap::pdata::OtapPdata;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Durable checkpoint state: the version-1 snapshot/WAL byte format and the
/// file-backed store that persists it.
///
/// The codec modules encode, decode, and replay checkpoint bytes in memory
/// and perform no I/O. [`checkpoint::store`] owns the namespace on disk; it
/// blocks, so it belongs on the receiver's dedicated read/checkpoint OS
/// thread. OS-specific locator lookup and recovery matching live in
/// [`identity`].
pub mod checkpoint;

mod batching;
mod config;
mod delivery;
mod discovery;
mod framing;
mod identity;
mod lease;
mod reader;
mod runtime;
mod worker;

use config::RuntimeConfig;
use runtime::FilelogReceiver;

pub use config::{
    BatchConfig, CheckpointConfig, Config, DiscoveryConfig, Encoding, FILELOG_RECEIVER_URN,
    FramingConfig, IdentityConfig, LimitsConfig, MaxLogSizeBehavior, MetadataConfig,
    MultilineConfig, OnDecodeError, OnNack, OnRecoveryMismatch, OnTruncate, RegexProfile,
    RetryConfig, RotationConfig, StartAt,
};

#[allow(unsafe_code)]
#[otap_df_engine::component_inventory(category = Receiver)]
#[distributed_slice(OTAP_RECEIVER_FACTORIES)]
/// Declares the Phase 1 filelog receiver as a local receiver factory.
pub static FILELOG_RECEIVER: ReceiverFactory<OtapPdata> = ReceiverFactory {
    name: FILELOG_RECEIVER_URN,
    create: |pipeline: PipelineContext,
             node: NodeId,
             node_config: Arc<NodeUserConfig>,
             receiver_config: &ReceiverConfig,
             _capabilities: &otap_df_engine::capability::registry::Capabilities| {
        create_filelog_receiver(pipeline, node, node_config, receiver_config)
    },
    wiring_contract: otap_df_engine::wiring_contract::WiringContract::UNRESTRICTED,
    validate_config: validate_filelog_config,
};

fn create_filelog_receiver(
    pipeline: PipelineContext,
    node: NodeId,
    node_config: Arc<NodeUserConfig>,
    receiver_config: &ReceiverConfig,
) -> Result<ReceiverWrapper<OtapPdata>, otap_df_config::error::Error> {
    validate_pipeline_cores(pipeline.num_cores())?;
    let pipeline_group_id = pipeline.pipeline_group_id();
    let pipeline_id = pipeline.pipeline_id();
    let default_checkpoint_id = derive_default_checkpoint_id(
        pipeline_group_id.as_ref(),
        pipeline_id.as_ref(),
        node.name.as_ref(),
        receiver_config.name.as_ref(),
    )?;
    let parsed: Config = serde_json::from_value(node_config.config.clone()).map_err(|error| {
        otap_df_config::error::Error::InvalidUserConfig {
            error: error.to_string(),
        }
    })?;
    let runtime = RuntimeConfig::from_config(parsed, &default_checkpoint_id)?;
    Ok(ReceiverWrapper::local(
        FilelogReceiver::new(runtime),
        node,
        node_config,
        receiver_config,
    ))
}

fn validate_filelog_config(config: &Value) -> Result<(), otap_df_config::error::Error> {
    let parsed: Config = serde_json::from_value(config.clone()).map_err(|error| {
        otap_df_config::error::Error::InvalidUserConfig {
            error: error.to_string(),
        }
    })?;
    RuntimeConfig::from_config(parsed, "filelog-validation-default").map(|_| ())
}

fn validate_pipeline_cores(num_cores: usize) -> Result<(), otap_df_config::error::Error> {
    if num_cores > 1 {
        return Err(otap_df_config::error::Error::InvalidUserConfig {
            error: "filelog must run in a one-core source pipeline; use \
                    receiver:filelog -> exporter:topic and fan out downstream"
                .to_owned(),
        });
    }
    Ok(())
}

fn derive_default_checkpoint_id(
    pipeline_group_id: &str,
    pipeline_id: &str,
    node_name: &str,
    receiver_name: &str,
) -> Result<String, otap_df_config::error::Error> {
    let mut digest = Sha256::new();
    digest.update(b"otap-filelog-checkpoint-default-v1");
    for field in [
        pipeline_group_id.as_bytes(),
        pipeline_id.as_bytes(),
        node_name.as_bytes(),
        receiver_name.as_bytes(),
    ] {
        let length = u64::try_from(field.len()).map_err(|_| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: "filelog checkpoint identity input length does not fit u64".to_owned(),
            }
        })?;
        digest.update(length.to_be_bytes());
        digest.update(field);
    }
    Ok(format!("auto-{}", hex::encode(digest.finalize())))
}

#[cfg(test)]
mod runtime_factory_tests {
    use serde_json::json;

    use super::*;

    /// Scenario: checkpoint.id is omitted for otherwise valid filelog JSON.
    /// Guarantees: factory validation exercises the full RuntimeConfig path
    /// with a nonempty placeholder default and still rejects unknown fields.
    #[test]
    fn factory_validation_supports_default_checkpoint_and_rejects_unknown_fields() {
        validate_filelog_config(&json!({ "include": ["/tmp/*.log"] })).unwrap();
        let error = validate_filelog_config(&json!({
            "include": ["/tmp/*.log"],
            "unknown": true
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    /// Scenario: two receiver placements differ in each namespace identity
    /// input one at a time.
    /// Guarantees: the default checkpoint ID is deterministic for one
    /// placement and collision-resistant across group, pipeline, node, and
    /// receiver names.
    #[test]
    fn default_checkpoint_id_binds_complete_receiver_placement() {
        let base = derive_default_checkpoint_id("group", "pipeline", "node", "receiver").unwrap();
        assert_eq!(
            base,
            derive_default_checkpoint_id("group", "pipeline", "node", "receiver").unwrap()
        );
        for changed in [
            derive_default_checkpoint_id("other", "pipeline", "node", "receiver").unwrap(),
            derive_default_checkpoint_id("group", "other", "node", "receiver").unwrap(),
            derive_default_checkpoint_id("group", "pipeline", "other", "receiver").unwrap(),
            derive_default_checkpoint_id("group", "pipeline", "node", "other").unwrap(),
        ] {
            assert_ne!(base, changed);
        }
        assert!(base.starts_with("auto-"));
        assert_eq!(base.len(), 69);
    }

    /// Scenario: factory construction is requested for single-core and
    /// multicore source pipelines.
    /// Guarantees: one core is accepted and every value above one is rejected
    /// with the topic-fanout guidance required by Phase 1.
    #[test]
    fn factory_rejects_multicore_source_pipeline() {
        validate_pipeline_cores(1).unwrap();
        for cores in [2, 8, usize::MAX] {
            let error = validate_pipeline_cores(cores).unwrap_err();
            assert!(error.to_string().contains("one-core source pipeline"));
            assert!(error.to_string().contains("exporter:topic"));
        }
    }

    /// Scenario: component inventory reads the registered filelog factory.
    /// Guarantees: registration reuses the authoritative existing URN rather
    /// than introducing a second public component identity.
    #[test]
    fn factory_registers_authoritative_filelog_urn() {
        assert_eq!(FILELOG_RECEIVER.name, FILELOG_RECEIVER_URN);
    }
}
