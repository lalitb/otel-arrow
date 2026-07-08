// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plan extraction for the listener-group manager.
//!
//! Walks a resolved pipeline configuration and produces
//! [`ListenerGroupPlan`]s for the set of known receiver URNs that
//! expose a TCP or UDP `listening_addr` field. Used by the controller during
//! Phase 2.5 startup to register plans before launching pipeline
//! threads.
//!
//! The helper is intentionally conservative: it only matches the
//! production receiver URNs that already use
//! `EffectHandler::tcp_listener` or `EffectHandler::udp_socket`.
//! Unknown URNs are silently skipped so a configuration with a
//! third-party receiver is not rejected.
//!
//! Because the controller crate cannot depend on receiver crates
//! (circular), we read just the known `listening_addr` fields from
//! raw `serde_json::Value` rather than pulling in each receiver's
//! typed `Config`.

use crate::listener_group::{
    ListenerGroupKey, ListenerGroupMember, ListenerGroupPlan, Protocol as ListenerProtocol,
};
use crate::topology::NumaTopology;
use otap_df_config::engine::ResolvedPipelineConfig;
use std::net::SocketAddr;
use std::str::FromStr;

/// Receiver URNs that the helper recognises. Each one has one or more
/// `listening_addr: SocketAddr` fields in its node config that the
/// seam can bind via `EffectHandler::tcp_listener` or
/// `EffectHandler::udp_socket`.
const KNOWN_RECEIVER_URNS: &[&str] = &[
    "urn:otel:receiver:otlp",
    "urn:otel:receiver:otap",
    "urn:otel:receiver:syslog_cef",
];

fn parse_listening_addr(value: &serde_json::Value) -> Option<SocketAddr> {
    value
        .get("listening_addr")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| SocketAddr::from_str(s).ok())
}

/// Extracts every listener address present in `config`, looking at the
/// top level, OTLP's actual `{protocols: {grpc|http}}` TCP nesting,
/// and syslog CEF's `{protocol: {tcp|udp}}` shape. Returns an empty
/// vector if no recognised shape applies. Uses raw `serde_json::Value`
/// access so the engine crate does not need a direct `serde` derive
/// dependency.
fn extract_listener_addresses(config: &serde_json::Value) -> Vec<(ListenerProtocol, SocketAddr)> {
    let mut out = Vec::new();
    if let Some(addr) = parse_listening_addr(config) {
        out.push((ListenerProtocol::Tcp, addr));
    }

    if let Some(parent) = config.get("protocols") {
        for sub in ["grpc", "http"] {
            if let Some(addr) = parent.get(sub).and_then(parse_listening_addr) {
                out.push((ListenerProtocol::Tcp, addr));
            }
        }
    }

    if let Some(protocol) = config.get("protocol") {
        for (key, listener_protocol) in [
            ("tcp", ListenerProtocol::Tcp),
            ("udp", ListenerProtocol::Udp),
        ] {
            if let Some(addr) = protocol.get(key).and_then(parse_listening_addr) {
                out.push((listener_protocol, addr));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Returns every listener-group key declared by recognised receivers
/// in `pipeline`.
///
/// This is policy-neutral: the controller uses it both to build full
/// coordinated plans for `ebpf_numa` pipelines and to reserve bind
/// identities for non-coordinated pipelines so mixed policies cannot
/// accidentally share one kernel reuseport group.
#[must_use]
pub fn extract_listener_keys_for_pipeline(
    pipeline: &ResolvedPipelineConfig,
) -> Vec<ListenerGroupKey> {
    let mut keys = Vec::new();
    let pipeline_group_id = pipeline.pipeline_group_id.as_ref().to_string();
    let pipeline_id = pipeline.pipeline_id.as_ref().to_string();

    for (node_id, node_cfg) in pipeline.pipeline.nodes().iter() {
        let urn_str = node_cfg.r#type.as_str();
        if !KNOWN_RECEIVER_URNS.contains(&urn_str) {
            continue;
        }
        // Try addresses (one receiver may yield 0..N: e.g. OTLP can
        // expose grpc and/or http simultaneously). The receiver_node_id
        // in the key MUST match what `runtime_pipeline.rs` builds when
        // constructing the per-receiver `ListenerGroupHandle` (plain
        // `node_id.name.to_string()`) -- otherwise the runtime's
        // `acquire(...)` returns `NoPlan` and the effect-handler seam
        // fails startup instead of silently rebinding. The bind
        // address is already part of `ListenerGroupKey`, so it does
        // not need to appear in the receiver_node_id; multiple
        // addresses on the same receiver simply produce multiple
        // keyed plans.
        for (protocol, addr) in extract_listener_addresses(&node_cfg.config) {
            let key = match protocol {
                ListenerProtocol::Tcp => ListenerGroupKey::tcp_for_receiver(
                    pipeline_group_id.clone(),
                    pipeline_id.clone(),
                    node_id,
                    addr,
                ),
                ListenerProtocol::Udp => ListenerGroupKey::udp_for_receiver(
                    pipeline_group_id.clone(),
                    pipeline_id.clone(),
                    node_id,
                    addr,
                ),
            };
            keys.push(key);
        }
    }
    keys
}

/// Returns one [`ListenerGroupPlan`] per recognised receiver in the
/// pipeline that exposes a TCP or UDP `listening_addr`. Members are
/// constructed from `cores`, with `numa_node` looked up via
/// [`NumaTopology::numa_node`] and `listener_id` assigned
/// stably as the position of the core within `cores`.
#[must_use]
pub fn extract_plans_for_pipeline(
    pipeline: &ResolvedPipelineConfig,
    cores: &[u32],
    topology: &NumaTopology,
) -> Vec<ListenerGroupPlan> {
    if cores.is_empty() {
        return Vec::new();
    }

    let members = cores
        .iter()
        .enumerate()
        .map(|(idx, core_id)| ListenerGroupMember {
            listener_id: idx as u32,
            core_id: *core_id,
            numa_node: topology.numa_node(*core_id),
        })
        .collect::<Vec<_>>();

    extract_listener_keys_for_pipeline(pipeline)
        .into_iter()
        .map(|key| ListenerGroupPlan {
            key,
            expected_members: members.clone(),
            strict: pipeline.policies.load_balancing.strict,
        })
        .collect::<Vec<_>>()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_top_level_listening_addr() {
        let cfg = json!({"listening_addr": "0.0.0.0:4317"});
        let addrs = extract_listener_addresses(&cfg);
        assert_eq!(addrs.len(), 1);
        assert_eq!(
            addrs[0],
            (ListenerProtocol::Tcp, "0.0.0.0:4317".parse().unwrap())
        );
    }

    #[test]
    fn extract_ignores_non_production_otlp_grpc_and_http_shape() {
        let cfg = json!({
            "grpc": {"listening_addr": "0.0.0.0:4317"},
            "http": {"listening_addr": "0.0.0.0:4318"}
        });
        let addrs = extract_listener_addresses(&cfg);
        assert!(addrs.is_empty());
    }

    #[test]
    fn extract_otlp_protocols_grpc_and_http() {
        let cfg = json!({
            "protocols": {
                "grpc": {"listening_addr": "0.0.0.0:4317"},
                "http": {"listening_addr": "0.0.0.0:4318"}
            }
        });
        let addrs = extract_listener_addresses(&cfg);
        assert_eq!(addrs.len(), 2);
        let ports: Vec<u16> = addrs.iter().map(|(_, a)| a.port()).collect();
        assert!(ports.contains(&4317));
        assert!(ports.contains(&4318));
    }

    #[test]
    fn extract_syslog_udp_protocol_addr() {
        let cfg = json!({
            "protocol": {
                "udp": {"listening_addr": "0.0.0.0:5140"}
            }
        });
        let addrs = extract_listener_addresses(&cfg);
        assert_eq!(addrs.len(), 1);
        assert_eq!(
            addrs[0],
            (ListenerProtocol::Udp, "0.0.0.0:5140".parse().unwrap())
        );
    }

    #[test]
    fn extract_returns_empty_for_unrecognised_shape() {
        let cfg = json!({"some_other_field": 42});
        assert!(extract_listener_addresses(&cfg).is_empty());
    }

    /// Regression: the key the controller registers must equal the
    /// key the runtime later builds for the same receiver. The
    /// runtime constructs a `ListenerGroupHandle` with
    /// `pipeline_id` plus `receiver_node_id = node_id.name.to_string()`
    /// and then calls `handle.tcp_key(addr)`. Extraction must produce
    /// the same key, otherwise `acquire(...)` returns `NoPlan` and
    /// silently falls back to independent bind.
    #[test]
    fn extracted_plan_key_matches_runtime_handle_key() {
        use crate::listener_group::{ListenerGroupHandle, ListenerGroupManager};
        use otap_df_config::engine::{ResolvedPipelineConfig, ResolvedPipelineRole};
        use otap_df_config::pipeline::PipelineConfig;
        use otap_df_config::policy::ResolvedPolicies;

        let yaml = r#"
            nodes:
              otlp_receiver:
                type: "urn:otel:receiver:otlp"
                config:
                  protocols:
                    grpc:
                      listening_addr: "127.0.0.1:18011"
        "#;
        let pipeline = PipelineConfig::from_yaml("pg".into(), "pipe".into(), yaml).unwrap();
        let resolved = ResolvedPipelineConfig {
            pipeline_group_id: "pg".into(),
            pipeline_id: "pipe".into(),
            pipeline,
            policies: ResolvedPolicies::default(),
            role: ResolvedPipelineRole::Regular,
        };

        let topo = NumaTopology::empty();
        let cores = vec![0u32, 1];
        let plans = extract_plans_for_pipeline(&resolved, &cores, &topo);
        assert_eq!(plans.len(), 1, "expected one plan for the OTLP receiver");
        let plan = &plans[0];

        // Build the runtime-style handle the same way
        // `runtime_pipeline.rs` does, then compare keys.
        let manager = ListenerGroupManager::new();
        let addr = plan.key.addr;
        let handle =
            ListenerGroupHandle::new(manager.clone(), "pg", "pipe", "otlp_receiver", 0, false);
        assert_eq!(handle.tcp_key(addr), plan.key);
    }

    /// End-to-end-ready: prove that the controller's extracted plan,
    /// once registered, can actually be acquired by two runtime-style
    /// handles concurrently and yield two distinct listeners on the
    /// same bound address. Does not require root, eBPF, or any
    /// receiver code path.
    #[test]
    fn extracted_plan_can_be_acquired_by_runtime_handles() {
        use crate::listener_group::{AcquireOutcome, ListenerGroupHandle, ListenerGroupManager};
        use otap_df_config::engine::{ResolvedPipelineConfig, ResolvedPipelineRole};
        use otap_df_config::pipeline::PipelineConfig;
        use otap_df_config::policy::ResolvedPolicies;
        use std::net::TcpListener as StdTcpListener;
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        // Discover an unused ephemeral port.
        let probe = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let yaml = format!(
            r#"
            nodes:
              otlp_receiver:
                type: "urn:otel:receiver:otlp"
                config:
                  protocols:
                    grpc:
                      listening_addr: "{addr}"
            "#
        );
        let pipeline = PipelineConfig::from_yaml("pg".into(), "pipe".into(), &yaml).unwrap();
        let resolved = ResolvedPipelineConfig {
            pipeline_group_id: "pg".into(),
            pipeline_id: "pipe".into(),
            pipeline,
            policies: ResolvedPolicies::default(),
            role: ResolvedPipelineRole::Regular,
        };

        let topo = NumaTopology::empty();
        let cores = vec![0u32, 1u32];
        let plans = extract_plans_for_pipeline(&resolved, &cores, &topo);
        assert_eq!(plans.len(), 1);

        let manager = ListenerGroupManager::new();
        manager
            .register_plan(plans.into_iter().next().unwrap())
            .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for core in cores.iter().copied() {
            let mgr = manager.clone();
            let bar = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let _ = bar.wait();
                let h = ListenerGroupHandle::new(
                    mgr.clone(),
                    "pg",
                    "pipe",
                    "otlp_receiver",
                    core,
                    false,
                );
                mgr.acquire(&h.tcp_key(addr), core, Duration::from_secs(2))
            }));
        }
        let mut listeners = Vec::new();
        for h in handles {
            match h.join().unwrap() {
                AcquireOutcome::Listener(l) => listeners.push(l),
                other => panic!("expected Listener, got {other:?}"),
            }
        }
        assert_eq!(listeners.len(), 2);
        assert_eq!(listeners[0].local_addr().unwrap(), addr);
        assert_eq!(listeners[1].local_addr().unwrap(), addr);
        assert_ne!(listeners[0].as_raw_fd(), listeners[1].as_raw_fd());
    }

    #[test]
    fn extracted_keys_include_pipeline_id() {
        use otap_df_config::engine::{ResolvedPipelineConfig, ResolvedPipelineRole};
        use otap_df_config::pipeline::PipelineConfig;
        use otap_df_config::policy::ResolvedPolicies;

        let yaml = r#"
            nodes:
              receiver:
                type: "urn:otel:receiver:otlp"
                config:
                  protocols:
                    grpc:
                      listening_addr: "127.0.0.1:18012"
        "#;
        let pipeline_a = PipelineConfig::from_yaml("pg".into(), "pipe-a".into(), yaml).unwrap();
        let pipeline_b = PipelineConfig::from_yaml("pg".into(), "pipe-b".into(), yaml).unwrap();
        let resolved_a = ResolvedPipelineConfig {
            pipeline_group_id: "pg".into(),
            pipeline_id: "pipe-a".into(),
            pipeline: pipeline_a,
            policies: ResolvedPolicies::default(),
            role: ResolvedPipelineRole::Regular,
        };
        let resolved_b = ResolvedPipelineConfig {
            pipeline_group_id: "pg".into(),
            pipeline_id: "pipe-b".into(),
            pipeline: pipeline_b,
            policies: ResolvedPolicies::default(),
            role: ResolvedPipelineRole::Regular,
        };

        let topo = NumaTopology::empty();
        let cores = vec![0u32, 1];
        let key_a = extract_plans_for_pipeline(&resolved_a, &cores, &topo)
            .into_iter()
            .next()
            .unwrap()
            .key;
        let key_b = extract_plans_for_pipeline(&resolved_b, &cores, &topo)
            .into_iter()
            .next()
            .unwrap()
            .key;

        assert_ne!(key_a, key_b);
        assert_eq!(key_a.pipeline_id, "pipe-a");
        assert_eq!(key_b.pipeline_id, "pipe-b");
    }

    #[test]
    fn extracted_plan_preserves_unknown_numa_nodes() {
        use otap_df_config::engine::{ResolvedPipelineConfig, ResolvedPipelineRole};
        use otap_df_config::pipeline::PipelineConfig;
        use otap_df_config::policy::ResolvedPolicies;
        use std::collections::BTreeMap;

        let yaml = r#"
            nodes:
              receiver:
                type: "urn:otel:receiver:otlp"
                config:
                  protocols:
                    grpc:
                      listening_addr: "127.0.0.1:18013"
        "#;
        let pipeline = PipelineConfig::from_yaml("pg".into(), "pipe".into(), yaml).unwrap();
        let resolved = ResolvedPipelineConfig {
            pipeline_group_id: "pg".into(),
            pipeline_id: "pipe".into(),
            pipeline,
            policies: ResolvedPolicies::default(),
            role: ResolvedPipelineRole::Regular,
        };
        let topology = NumaTopology::new(
            BTreeMap::from([(0, 0)]),
            crate::topology::TopologyCompleteness::Partial,
        );

        let plans = extract_plans_for_pipeline(&resolved, &[0, 99], &topology);

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].expected_members[0].numa_node, Some(0));
        assert_eq!(plans[0].expected_members[1].numa_node, None);
    }

    #[test]
    fn extracted_plan_carries_strict_policy() {
        use otap_df_config::engine::{ResolvedPipelineConfig, ResolvedPipelineRole};
        use otap_df_config::pipeline::PipelineConfig;
        use otap_df_config::policy::{
            LoadBalancingPolicy, LoadBalancingStrategy, ResolvedPolicies,
        };

        let yaml = r#"
            nodes:
              receiver:
                type: "urn:otel:receiver:otlp"
                config:
                  protocols:
                    grpc:
                      listening_addr: "127.0.0.1:18014"
        "#;
        let pipeline = PipelineConfig::from_yaml("pg".into(), "pipe".into(), yaml).unwrap();
        let policies = ResolvedPolicies {
            load_balancing: LoadBalancingPolicy {
                strategy: LoadBalancingStrategy::EbpfNuma,
                strict: true,
            },
            ..ResolvedPolicies::default()
        };
        let resolved = ResolvedPipelineConfig {
            pipeline_group_id: "pg".into(),
            pipeline_id: "pipe".into(),
            pipeline,
            policies,
            role: ResolvedPipelineRole::Regular,
        };

        let plans = extract_plans_for_pipeline(&resolved, &[0], &NumaTopology::empty());

        assert_eq!(plans.len(), 1);
        assert!(plans[0].strict);
    }
}
