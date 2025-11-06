// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Fan-out processor factory for OTAP pipelines.
//!
//! Wraps the engine-level `FanoutProcessor` (mutation-aware fan-out).
//! Current implementation:
//! - Provides a processor factory registered in `OTAP_PROCESSOR_FACTORIES`.
//! - Accepts (but ignores) user config (reserved for future options like
//!   copy-on-write tuning or mutation interest overrides).
//!
//! Plugin URN: `urn:otel:fanout:processor`

use crate::OTAP_PROCESSOR_FACTORIES;
use crate::pdata::OtapPdata;
use linkme::distributed_slice;
use otap_df_config::error::Error as ConfigError;
use otap_df_config::node::NodeUserConfig;
use otap_df_engine::ProcessorFactory;
use otap_df_engine::config::ProcessorConfig;
use otap_df_engine::context::PipelineContext;
use otap_df_engine::fanout_processor::FanoutProcessor;
use otap_df_engine::node::NodeId;
use otap_df_engine::processor::ProcessorWrapper;
use std::sync::Arc;

/// URN for the fan-out processor
pub const FANOUT_PROCESSOR_URN: &str = "urn:otel:fanout:processor";

/// Factory create function
pub fn create_fanout_processor(
    _pipeline_ctx: PipelineContext,
    node: NodeId,
    node_config: Arc<NodeUserConfig>,
    processor_config: &ProcessorConfig,
) -> Result<ProcessorWrapper<OtapPdata>, ConfigError> {
    // No user-config parsing yet; reserved for future fan-out options.
    Ok(ProcessorWrapper::local(
        FanoutProcessor,
        node,
        node_config,
        processor_config,
    ))
}

/// Register FanoutProcessor as an OTAP processor factory
#[allow(unsafe_code)]
#[distributed_slice(OTAP_PROCESSOR_FACTORIES)]
pub static FANOUT_PROCESSOR_FACTORY: ProcessorFactory<OtapPdata> = ProcessorFactory {
    name: FANOUT_PROCESSOR_URN,
    create: |pipeline_ctx: PipelineContext,
             node: NodeId,
             node_config: Arc<NodeUserConfig>,
             proc_cfg: &ProcessorConfig| {
        create_fanout_processor(pipeline_ctx, node, node_config, proc_cfg)
    },
};
