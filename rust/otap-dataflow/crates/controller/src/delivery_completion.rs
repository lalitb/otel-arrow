// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Startup validation of node delivery-completion requirements.
//!
//! A node factory can declare, through its
//! [`WiringContract`](otap_df_engine::wiring_contract::WiringContract), that it
//! only makes progress on an *aggregate* delivery completion: one Ack that means
//! a nonempty set of required destinations all acked. This module proves, before
//! any pipeline worker starts, that every route leaving such a node can actually
//! provide that guarantee, and rejects the configuration otherwise.
//!
//! The validation is component-agnostic. It reads the requirement from factory
//! wiring contracts and the route from the resolved pipeline and topic graph; no
//! component URN or type string is special-cased as "the requiring node".
//!
//! Two proofs are involved and they are deliberately separate:
//!
//! * this module proves the *configured* path (a declared topic, a configured
//!   broadcast topic receiver, broadcast-only inference, automatic Ack
//!   propagation, and all-subscriber aggregation);
//! * the topic runtime enforces *live* readiness by rejecting an all-mode
//!   publish whose required-subscriber snapshot is empty.
//!
//! Configuration alone never claims that a subscriber is running.

use std::collections::{HashMap, HashSet, VecDeque};

use otap_df_config::engine::{
    OtelDataflowSpec, SYSTEM_OBSERVABILITY_PIPELINE_ID, SYSTEM_PIPELINE_GROUP_ID,
};
use otap_df_config::node::{NodeKind, NodeUserConfig};
use otap_df_config::pipeline::PipelineConfig;
use otap_df_config::topic::{TopicAckPropagationMode, TopicBroadcastAckMode};
use otap_df_config::{NodeId, PipelineGroupId, PipelineId, TopicName};
use otap_df_engine::PipelineFactory;
use otap_df_engine::wiring_contract::WiringContract;

use crate::error::Error;
use crate::{InferredTopicMode, TopicRouteFacts};

/// URN id shared by the topic receiver and topic exporter node types.
///
/// Topic nodes are the only pipeline nodes whose delivery completion crosses a
/// topic hop, so route proving needs to recognize them. This mirrors the
/// existing topic-topology inference in the controller and is unrelated to which
/// component declares the aggregate-Ack requirement.
const TOPIC_NODE_URN_ID: &str = "topic";

/// Resolved topic graph inputs needed to prove a delivery-completion path.
pub(crate) struct TopicGraphView<'a> {
    /// Global topic local name -> declared (fully qualified) name.
    pub(crate) global_names: &'a HashMap<TopicName, TopicName>,
    /// (group, local topic name) -> declared (fully qualified) name.
    pub(crate) group_names: &'a HashMap<(PipelineGroupId, TopicName), TopicName>,
    /// Declared topic name -> resolved topology and policy facts.
    pub(crate) route_facts: &'a HashMap<TopicName, TopicRouteFacts>,
}

impl TopicGraphView<'_> {
    fn resolve_declared_name(
        &self,
        pipeline_group_id: &PipelineGroupId,
        topic_name: &TopicName,
    ) -> Option<TopicName> {
        self.group_names
            .get(&(pipeline_group_id.clone(), topic_name.clone()))
            .cloned()
            .or_else(|| self.global_names.get(topic_name).cloned())
    }
}

/// Validates every aggregate-Ack-required source route in the configuration.
///
/// Returns the first unsupported route as an error. Validation is fail-closed:
/// a route that cannot be proven from the resolved graph is rejected rather than
/// assumed to work.
pub(crate) fn validate_delivery_completion_requirements<
    PData: 'static + Clone + std::fmt::Debug,
>(
    config: &OtelDataflowSpec,
    factory: &PipelineFactory<PData>,
    topics: &TopicGraphView<'_>,
) -> Result<(), Error> {
    let mut group_ids = config.groups.keys().cloned().collect::<Vec<_>>();
    group_ids.sort_by(|left, right| left.as_ref().cmp(right.as_ref()));
    for group_id in group_ids {
        let Some(group_cfg) = config.groups.get(&group_id) else {
            continue;
        };
        let mut pipelines = group_cfg.pipelines.iter().collect::<Vec<_>>();
        pipelines.sort_by(|(left, _), (right, _)| left.as_ref().cmp(right.as_ref()));
        for (pipeline_id, pipeline_cfg) in pipelines {
            validate_pipeline(&group_id, pipeline_id, pipeline_cfg, factory, topics)?;
        }
    }

    let system_group_id: PipelineGroupId = SYSTEM_PIPELINE_GROUP_ID.into();
    let observability_pipeline_id: PipelineId = SYSTEM_OBSERVABILITY_PIPELINE_ID.into();
    let observability_pipeline = config
        .engine
        .observability
        .pipeline
        .clone()
        .into_pipeline_config();
    validate_pipeline(
        &system_group_id,
        &observability_pipeline_id,
        &observability_pipeline,
        factory,
        topics,
    )
}

fn validate_pipeline<PData: 'static + Clone + std::fmt::Debug>(
    pipeline_group_id: &PipelineGroupId,
    pipeline_id: &PipelineId,
    pipeline: &PipelineConfig,
    factory: &PipelineFactory<PData>,
    topics: &TopicGraphView<'_>,
) -> Result<(), Error> {
    let nodes: HashMap<&NodeId, &NodeUserConfig> = pipeline
        .node_iter()
        .map(|(node_id, node_cfg)| (node_id, node_cfg.as_ref()))
        .collect();

    let mut required_sources = Vec::new();
    for (node_id, node_cfg) in &nodes {
        let contract = node_wiring_contract(factory, node_cfg);
        if contract.is_some_and(|contract| contract.requires_aggregate_ack()) {
            required_sources.push(*node_id);
        }
    }
    if required_sources.is_empty() {
        return Ok(());
    }
    required_sources.sort_by(|left, right| left.as_ref().cmp(right.as_ref()));

    let mut destinations: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut output_ports: HashMap<NodeId, HashSet<String>> = HashMap::new();
    for connection in pipeline.connection_iter() {
        let targets = connection.to_nodes();
        for source in connection.from_sources() {
            let _ = output_ports
                .entry(source.node_id().clone())
                .or_default()
                .insert(source.resolved_output_port().to_string());
            destinations
                .entry(source.node_id().clone())
                .or_default()
                .extend(targets.iter().cloned());
        }
    }

    for source in required_sources {
        validate_required_route(
            pipeline_group_id,
            pipeline_id,
            source,
            &nodes,
            &destinations,
            &output_ports,
            factory,
            topics,
        )?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_required_route<PData: 'static + Clone + std::fmt::Debug>(
    pipeline_group_id: &PipelineGroupId,
    pipeline_id: &PipelineId,
    source: &NodeId,
    nodes: &HashMap<&NodeId, &NodeUserConfig>,
    destinations: &HashMap<NodeId, Vec<NodeId>>,
    output_ports: &HashMap<NodeId, HashSet<String>>,
    factory: &PipelineFactory<PData>,
    topics: &TopicGraphView<'_>,
) -> Result<(), Error> {
    let reject = |reason: String| Error::AggregateAckRouteUnsupported {
        pipeline_group: pipeline_group_id.as_ref().to_owned(),
        pipeline: pipeline_id.as_ref().to_owned(),
        node: source.as_ref().to_owned(),
        reason,
    };

    let source_destinations = destinations.get(source).filter(|list| !list.is_empty());
    let Some(source_destinations) = source_destinations else {
        return Err(reject(
            "the node has no output route, so no delivery completion can ever reach it".to_owned(),
        ));
    };

    // More than one output port from the requiring node means the completion
    // path is not a single provable route. Fail closed rather than guess which
    // port carries the Ack-gated data.
    if output_ports
        .get(source)
        .is_some_and(|ports| ports.len() > 1)
    {
        return Err(reject(
            "the node fans out over multiple output ports, so its aggregate delivery completion path is ambiguous".to_owned(),
        ));
    }

    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<NodeId> = source_destinations.iter().cloned().collect();

    while let Some(node_id) = queue.pop_front() {
        if !visited.insert(node_id.clone()) {
            continue;
        }
        let Some(node_cfg) = nodes.get(&node_id) else {
            return Err(reject(format!(
                "route reaches node `{}`, which is missing from the resolved pipeline topology",
                node_id.as_ref()
            )));
        };
        if node_wiring_contract(factory, node_cfg).is_none() {
            return Err(reject(format!(
                "route reaches node `{}` of unregistered type `{}`, so the delivery path cannot be proven",
                node_id.as_ref(),
                node_cfg.r#type.as_str()
            )));
        }

        let next = destinations
            .get(&node_id)
            .map(Vec::as_slice)
            .unwrap_or_default();

        if node_cfg.kind() == NodeKind::Exporter {
            if is_topic_node(node_cfg) {
                validate_topic_hop(pipeline_group_id, &node_id, node_cfg, topics, &reject)?;
            }
            // A non-topic exporter terminates the route inside this pipeline,
            // where the engine already turns the exporter's own Ack/Nack into
            // the upstream completion.
            continue;
        }

        if next.is_empty() {
            return Err(reject(format!(
                "route ends at `{}`, which is not an exporter and cannot complete delivery",
                node_id.as_ref()
            )));
        }
        queue.extend(next.iter().cloned());
    }

    Ok(())
}

fn validate_topic_hop(
    pipeline_group_id: &PipelineGroupId,
    node_id: &NodeId,
    node_cfg: &NodeUserConfig,
    topics: &TopicGraphView<'_>,
    reject: &impl Fn(String) -> Error,
) -> Result<(), Error> {
    let Some(topic_name) = topic_name_of(node_cfg) else {
        return Err(reject(format!(
            "topic exporter `{}` has no resolvable `topic` name",
            node_id.as_ref()
        )));
    };
    let Some(declared_name) = topics.resolve_declared_name(pipeline_group_id, &topic_name) else {
        return Err(reject(format!(
            "topic exporter `{}` publishes to undeclared topic `{}`",
            node_id.as_ref(),
            topic_name.as_ref()
        )));
    };
    let Some(facts) = topics.route_facts.get(&declared_name) else {
        return Err(reject(format!(
            "topic `{}` has no resolved topology metadata, so its delivery guarantee cannot be proven",
            declared_name.as_ref()
        )));
    };

    if facts.receiver_refs == 0 || !facts.has_broadcast_receivers {
        return Err(reject(format!(
            "topic `{}` has no configured broadcast topic receiver, so its required membership would be empty",
            declared_name.as_ref()
        )));
    }
    if facts.has_unknown_receiver_mode {
        return Err(reject(format!(
            "topic `{}` has a topic receiver with an unrecognized subscription mode",
            declared_name.as_ref()
        )));
    }
    if facts.balanced_group_count > 0 {
        return Err(reject(format!(
            "topic `{}` has balanced topic receivers, which cannot participate in all-subscriber aggregation",
            declared_name.as_ref()
        )));
    }
    if facts.selected_mode != InferredTopicMode::BroadcastOnly {
        return Err(reject(format!(
            "topic `{}` resolves to `{}` mode instead of `broadcast_only`",
            declared_name.as_ref(),
            facts.selected_mode.as_str()
        )));
    }
    if facts.ack_propagation_mode != TopicAckPropagationMode::Auto {
        return Err(reject(format!(
            "topic `{}` does not set `policies.ack_propagation.mode: auto`",
            declared_name.as_ref()
        )));
    }
    if facts.broadcast_ack_mode != TopicBroadcastAckMode::All {
        return Err(reject(format!(
            "topic `{}` does not set `policies.broadcast.ack_mode: all`",
            declared_name.as_ref()
        )));
    }

    Ok(())
}

fn is_topic_node(node_cfg: &NodeUserConfig) -> bool {
    node_cfg.r#type.id() == TOPIC_NODE_URN_ID
}

fn topic_name_of(node_cfg: &NodeUserConfig) -> Option<TopicName> {
    TopicName::parse(node_cfg.config.get("topic")?.as_str()?).ok()
}

fn node_wiring_contract<PData: 'static + Clone + std::fmt::Debug>(
    factory: &PipelineFactory<PData>,
    node_cfg: &NodeUserConfig,
) -> Option<WiringContract> {
    let urn = node_cfg.r#type.as_str();
    match node_cfg.kind() {
        NodeKind::Receiver => factory
            .get_receiver_factory_map()
            .get(urn)
            .map(|factory| factory.wiring_contract),
        NodeKind::Processor => factory
            .get_processor_factory_map()
            .get(urn)
            .map(|factory| factory.wiring_contract),
        NodeKind::Exporter => factory
            .get_exporter_factory_map()
            .get(urn)
            .map(|factory| factory.wiring_contract),
    }
}

#[cfg(test)]
mod tests {
    use super::{TopicGraphView, validate_delivery_completion_requirements};
    use crate::error::Error;
    use crate::{InferredTopicMode, TopicRouteFacts};
    use otap_df_config::engine::OtelDataflowSpec;
    use otap_df_config::node::NodeUserConfig;
    use otap_df_config::topic::{TopicAckPropagationMode, TopicBroadcastAckMode};
    use otap_df_config::{PipelineGroupId, TopicName};
    use otap_df_engine::config::{ExporterConfig, ProcessorConfig, ReceiverConfig};
    use otap_df_engine::context::PipelineContext;
    use otap_df_engine::exporter::ExporterWrapper;
    use otap_df_engine::processor::ProcessorWrapper;
    use otap_df_engine::receiver::ReceiverWrapper;
    use otap_df_engine::wiring_contract::WiringContract;
    use otap_df_engine::{ExporterFactory, PipelineFactory, ProcessorFactory, ReceiverFactory};
    use std::collections::HashMap;
    use std::sync::Arc;

    const REQUIRED_RECEIVER_URN: &str = "urn:test:receiver:required_ack";
    const PLAIN_RECEIVER_URN: &str = "urn:test:receiver:plain";
    const TOPIC_RECEIVER_URN: &str = "urn:otel:receiver:topic";
    const PASSTHROUGH_PROCESSOR_URN: &str = "urn:test:processor:passthrough";
    const PLAIN_EXPORTER_URN: &str = "urn:test:exporter:plain";
    const TOPIC_EXPORTER_URN: &str = "urn:otel:exporter:topic";

    fn unused_receiver(
        _pipeline: PipelineContext,
        _node: otap_df_engine::node::NodeId,
        _node_config: Arc<NodeUserConfig>,
        _receiver_config: &ReceiverConfig,
        _capabilities: &otap_df_engine::capability::registry::Capabilities,
    ) -> Result<ReceiverWrapper<()>, otap_df_config::error::Error> {
        unreachable!("delivery-completion validation never constructs nodes")
    }

    fn unused_processor(
        _pipeline: PipelineContext,
        _node: otap_df_engine::node::NodeId,
        _node_config: Arc<NodeUserConfig>,
        _processor_config: &ProcessorConfig,
        _capabilities: &otap_df_engine::capability::registry::Capabilities,
    ) -> Result<ProcessorWrapper<()>, otap_df_config::error::Error> {
        unreachable!("delivery-completion validation never constructs nodes")
    }

    fn unused_exporter(
        _pipeline: PipelineContext,
        _node: otap_df_engine::node::NodeId,
        _node_config: Arc<NodeUserConfig>,
        _exporter_config: &ExporterConfig,
        _capabilities: &otap_df_engine::capability::registry::Capabilities,
    ) -> Result<ExporterWrapper<()>, otap_df_config::error::Error> {
        unreachable!("delivery-completion validation never constructs nodes")
    }

    fn accept_any(_config: &serde_json::Value) -> Result<(), otap_df_config::error::Error> {
        Ok(())
    }

    static TEST_RECEIVERS: &[ReceiverFactory<()>] = &[
        ReceiverFactory {
            name: REQUIRED_RECEIVER_URN,
            create: unused_receiver,
            wiring_contract: WiringContract::UNRESTRICTED.requiring_aggregate_ack(),
            validate_config: accept_any,
        },
        ReceiverFactory {
            name: PLAIN_RECEIVER_URN,
            create: unused_receiver,
            wiring_contract: WiringContract::UNRESTRICTED,
            validate_config: accept_any,
        },
        ReceiverFactory {
            name: TOPIC_RECEIVER_URN,
            create: unused_receiver,
            wiring_contract: WiringContract::UNRESTRICTED,
            validate_config: accept_any,
        },
    ];

    static TEST_PROCESSORS: &[ProcessorFactory<()>] = &[ProcessorFactory {
        name: PASSTHROUGH_PROCESSOR_URN,
        create: unused_processor,
        wiring_contract: WiringContract::UNRESTRICTED,
        validate_config: accept_any,
    }];

    static TEST_EXPORTERS: &[ExporterFactory<()>] = &[
        ExporterFactory {
            name: PLAIN_EXPORTER_URN,
            create: unused_exporter,
            wiring_contract: WiringContract::UNRESTRICTED,
            validate_config: accept_any,
        },
        ExporterFactory {
            name: TOPIC_EXPORTER_URN,
            create: unused_exporter,
            wiring_contract: WiringContract::UNRESTRICTED,
            validate_config: accept_any,
        },
    ];

    fn test_factory() -> &'static PipelineFactory<()> {
        Box::leak(Box::new(PipelineFactory::new(
            TEST_RECEIVERS,
            TEST_PROCESSORS,
            TEST_EXPORTERS,
            &[],
        )))
    }

    fn supported_topic_facts() -> TopicRouteFacts {
        TopicRouteFacts {
            selected_mode: InferredTopicMode::BroadcastOnly,
            has_broadcast_receivers: true,
            has_unknown_receiver_mode: false,
            balanced_group_count: 0,
            receiver_refs: 1,
            ack_propagation_mode: TopicAckPropagationMode::Auto,
            broadcast_ack_mode: TopicBroadcastAckMode::All,
        }
    }

    fn run(yaml: &str, facts: &[(&str, TopicRouteFacts)]) -> Result<(), Error> {
        let config = OtelDataflowSpec::from_yaml(yaml).expect("test config should parse");
        let mut global_names = HashMap::new();
        for topic_name in config.topics.keys() {
            let declared =
                TopicName::parse(&format!("global::{}", topic_name.as_ref())).expect("valid name");
            let _ = global_names.insert(topic_name.clone(), declared);
        }
        let group_names = HashMap::<(PipelineGroupId, TopicName), TopicName>::new();
        let mut route_facts = HashMap::new();
        for (declared, fact) in facts {
            let _ = route_facts.insert(TopicName::parse(declared).expect("valid name"), *fact);
        }
        validate_delivery_completion_requirements(
            &config,
            test_factory(),
            &TopicGraphView {
                global_names: &global_names,
                group_names: &group_names,
                route_facts: &route_facts,
            },
        )
    }

    fn reason(result: Result<(), Error>) -> String {
        match result {
            Err(Error::AggregateAckRouteUnsupported { reason, node, .. }) => {
                assert_eq!(node, "required");
                reason
            }
            Ok(()) => panic!("route should have been rejected"),
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    fn topic_pipeline_yaml(topic_declaration: &str, exporter_topic: &str) -> String {
        format!(
            r#"
version: otel_dataflow/v1
{topic_declaration}
groups:
  g1:
    pipelines:
      p1:
        nodes:
          required:
            type: "{REQUIRED_RECEIVER_URN}"
            config: null
          to_topic:
            type: "{TOPIC_EXPORTER_URN}"
            config:
              topic: {exporter_topic}
        connections:
          - from: required
            to: to_topic
"#
        )
    }

    /// Scenario: a node that does not declare an aggregate-Ack requirement is
    /// wired to an ordinary exporter.
    /// Guarantees: validation stays inert for nodes without the requirement, so
    /// existing pipelines are unaffected.
    #[test]
    fn unrestricted_source_is_not_validated() {
        let yaml = format!(
            r#"
version: otel_dataflow/v1
groups:
  g1:
    pipelines:
      p1:
        nodes:
          plain:
            type: "{PLAIN_RECEIVER_URN}"
            config: null
          out:
            type: "{PLAIN_EXPORTER_URN}"
            config: null
        connections:
          - from: plain
            to: out
"#
        );
        run(&yaml, &[]).expect("unrestricted sources are always accepted");
    }

    /// Scenario: an aggregate-Ack-required receiver feeds a non-topic exporter
    /// through a processor inside the same pipeline.
    /// Guarantees: a route the engine can prove end to end in-pipeline is
    /// accepted, so direct delivery keeps relying on in-pipeline completion.
    #[test]
    fn accepts_direct_provable_in_pipeline_route() {
        let yaml = format!(
            r#"
version: otel_dataflow/v1
groups:
  g1:
    pipelines:
      p1:
        nodes:
          required:
            type: "{REQUIRED_RECEIVER_URN}"
            config: null
          mid:
            type: "{PASSTHROUGH_PROCESSOR_URN}"
            config: null
          out:
            type: "{PLAIN_EXPORTER_URN}"
            config: null
        connections:
          - from: required
            to: mid
          - from: mid
            to: out
"#
        );
        run(&yaml, &[]).expect("direct provable route should be accepted");

        // The shipped single-hop shape (required receiver straight to a
        // non-topic exporter) must keep working too.
        let direct = format!(
            r#"
version: otel_dataflow/v1
groups:
  g1:
    pipelines:
      p1:
        nodes:
          required:
            type: "{REQUIRED_RECEIVER_URN}"
            config: null
          out:
            type: "{PLAIN_EXPORTER_URN}"
            config: null
        connections:
          - from: required
            to: out
"#
        );
        run(&direct, &[]).expect("single-hop provable route should be accepted");
    }

    /// Scenario: an aggregate-Ack-required receiver has no outgoing connection.
    /// Guarantees: a source with no output route is rejected, because no
    /// delivery completion can ever reach it.
    #[test]
    fn rejects_source_without_output_route() {
        let yaml = format!(
            r#"
version: otel_dataflow/v1
groups:
  g1:
    pipelines:
      p1:
        nodes:
          required:
            type: "{REQUIRED_RECEIVER_URN}"
            config: null
          plain:
            type: "{PLAIN_RECEIVER_URN}"
            config: null
          out:
            type: "{PLAIN_EXPORTER_URN}"
            config: null
        connections:
          - from: plain
            to: out
"#
        );
        let message = reason(run(&yaml, &[]));
        assert!(message.contains("no output route"), "{message}");
    }

    /// Scenario: an aggregate-Ack-required receiver emits on two distinct output
    /// ports.
    /// Guarantees: an ambiguous output selection is rejected instead of guessing
    /// which port carries the Ack-gated data.
    #[test]
    fn rejects_source_with_ambiguous_output_ports() {
        let yaml = format!(
            r#"
version: otel_dataflow/v1
groups:
  g1:
    pipelines:
      p1:
        nodes:
          required:
            type: "{REQUIRED_RECEIVER_URN}"
            config: null
          out_a:
            type: "{PLAIN_EXPORTER_URN}"
            config: null
          out_b:
            type: "{PLAIN_EXPORTER_URN}"
            config: null
        connections:
          - from: 'required["primary"]'
            to: out_a
          - from: 'required["secondary"]'
            to: out_b
"#
        );
        let message = reason(run(&yaml, &[]));
        assert!(message.contains("multiple output ports"), "{message}");
    }

    /// Scenario: an aggregate-Ack-required receiver's route dead-ends at a
    /// processor with no downstream connection.
    /// Guarantees: validation fails closed instead of assuming an unproven
    /// completion point exists.
    #[test]
    fn rejects_route_that_never_reaches_an_exporter() {
        let yaml = format!(
            r#"
version: otel_dataflow/v1
groups:
  g1:
    pipelines:
      p1:
        nodes:
          required:
            type: "{REQUIRED_RECEIVER_URN}"
            config: null
          mid:
            type: "{PASSTHROUGH_PROCESSOR_URN}"
            config: null
        connections:
          - from: required
            to: mid
"#
        );
        let message = reason(run(&yaml, &[]));
        assert!(message.contains("cannot complete delivery"), "{message}");
    }

    /// Scenario: an aggregate-Ack-required receiver publishes to a topic that is
    /// not declared anywhere in the configuration.
    /// Guarantees: an unresolvable topic hop is rejected rather than assumed to
    /// provide aggregate completion.
    #[test]
    fn rejects_missing_topic_declaration() {
        let yaml = topic_pipeline_yaml("", "missing_topic");
        let message = reason(run(&yaml, &[]));
        assert!(message.contains("undeclared topic"), "{message}");
    }

    /// Scenario: an aggregate-Ack-required receiver publishes to a declared topic
    /// whose resolved topology metadata is unavailable.
    /// Guarantees: missing topology metadata fails closed.
    #[test]
    fn rejects_topic_without_resolved_metadata() {
        let yaml = topic_pipeline_yaml("topics:\n  events: {}", "events");
        let message = reason(run(&yaml, &[]));
        assert!(
            message.contains("no resolved topology metadata"),
            "{message}"
        );
    }

    /// Scenario: the topic hop has no configured topic receiver at all.
    /// Guarantees: an empty configured required membership is rejected, so the
    /// path can never claim an aggregate Ack over zero subscribers.
    #[test]
    fn rejects_topic_without_configured_broadcast_receiver() {
        let yaml = topic_pipeline_yaml("topics:\n  events: {}", "events");
        let facts = TopicRouteFacts {
            receiver_refs: 0,
            has_broadcast_receivers: false,
            selected_mode: InferredTopicMode::Mixed,
            ..supported_topic_facts()
        };
        let message = reason(run(&yaml, &[("global::events", facts)]));
        assert!(
            message.contains("no configured broadcast topic receiver"),
            "{message}"
        );
    }

    /// Scenario: the topic hop has a balanced topic receiver.
    /// Guarantees: balanced subscriptions cannot participate in all-subscriber
    /// aggregation, so the route is rejected.
    #[test]
    fn rejects_topic_with_balanced_receiver() {
        let yaml = topic_pipeline_yaml("topics:\n  events: {}", "events");
        let facts = TopicRouteFacts {
            balanced_group_count: 1,
            selected_mode: InferredTopicMode::Mixed,
            ..supported_topic_facts()
        };
        let message = reason(run(&yaml, &[("global::events", facts)]));
        assert!(message.contains("balanced topic receivers"), "{message}");
    }

    /// Scenario: the topic hop has a topic receiver whose subscription mode
    /// cannot be resolved from configuration.
    /// Guarantees: an unknown receiver mode is treated as unprovable topology.
    #[test]
    fn rejects_topic_with_unknown_receiver_mode() {
        let yaml = topic_pipeline_yaml("topics:\n  events: {}", "events");
        let facts = TopicRouteFacts {
            has_unknown_receiver_mode: true,
            selected_mode: InferredTopicMode::Mixed,
            ..supported_topic_facts()
        };
        let message = reason(run(&yaml, &[("global::events", facts)]));
        assert!(
            message.contains("unrecognized subscription mode"),
            "{message}"
        );
    }

    /// Scenario: the topic hop resolves to a mode other than broadcast-only.
    /// Guarantees: only the broadcast-only runtime can snapshot required
    /// membership, so other modes are rejected.
    #[test]
    fn rejects_topic_not_resolving_to_broadcast_only() {
        let yaml = topic_pipeline_yaml("topics:\n  events: {}", "events");
        let facts = TopicRouteFacts {
            selected_mode: InferredTopicMode::Mixed,
            ..supported_topic_facts()
        };
        let message = reason(run(&yaml, &[("global::events", facts)]));
        assert!(message.contains("instead of `broadcast_only`"), "{message}");
    }

    /// Scenario: the topic hop leaves Ack propagation disabled.
    /// Guarantees: without automatic propagation no completion can cross the
    /// topic hop, so the route is rejected.
    #[test]
    fn rejects_topic_with_ack_propagation_disabled() {
        let yaml = topic_pipeline_yaml("topics:\n  events: {}", "events");
        let facts = TopicRouteFacts {
            ack_propagation_mode: TopicAckPropagationMode::Disabled,
            ..supported_topic_facts()
        };
        let message = reason(run(&yaml, &[("global::events", facts)]));
        assert!(message.contains("ack_propagation.mode: auto"), "{message}");
    }

    /// Scenario: the topic hop keeps the default first-subscriber-wins broadcast
    /// aggregation.
    /// Guarantees: `first` cannot mean "all required subscribers acked", so the
    /// route is rejected.
    #[test]
    fn rejects_topic_with_first_broadcast_ack_mode() {
        let yaml = topic_pipeline_yaml("topics:\n  events: {}", "events");
        let facts = TopicRouteFacts {
            broadcast_ack_mode: TopicBroadcastAckMode::First,
            ..supported_topic_facts()
        };
        let message = reason(run(&yaml, &[("global::events", facts)]));
        assert!(message.contains("broadcast.ack_mode: all"), "{message}");
    }

    /// Scenario: the topic hop is broadcast-only with a configured broadcast
    /// receiver, automatic Ack propagation, and all-subscriber aggregation.
    /// Guarantees: the fully supported configured path is accepted.
    #[test]
    fn accepts_supported_topic_hop() {
        let yaml = topic_pipeline_yaml("topics:\n  events: {}", "events");
        run(&yaml, &[("global::events", supported_topic_facts())])
            .expect("supported topic hop should be accepted");
    }

    /// Scenario: an aggregate-Ack-required receiver fans out to both a supported
    /// topic hop and an unsupported one.
    /// Guarantees: every branch of a fanned-out required route must be provable;
    /// one unsupported branch rejects the configuration.
    #[test]
    fn rejects_when_any_fanout_branch_is_unsupported() {
        let yaml = format!(
            r#"
version: otel_dataflow/v1
topics:
  good: {{}}
  bad: {{}}
groups:
  g1:
    pipelines:
      p1:
        nodes:
          required:
            type: "{REQUIRED_RECEIVER_URN}"
            config: null
          to_good:
            type: "{TOPIC_EXPORTER_URN}"
            config:
              topic: good
          to_bad:
            type: "{TOPIC_EXPORTER_URN}"
            config:
              topic: bad
        connections:
          - from: required
            to: [to_good, to_bad]
"#
        );
        let bad = TopicRouteFacts {
            broadcast_ack_mode: TopicBroadcastAckMode::First,
            ..supported_topic_facts()
        };
        let message = reason(run(
            &yaml,
            &[
                ("global::good", supported_topic_facts()),
                ("global::bad", bad),
            ],
        ));
        assert!(message.contains("broadcast.ack_mode: all"), "{message}");
    }
}
