// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Ultra-minimal test utilities for OTAP components

use crate::pdata::OtapPdata;
use bytes::Bytes;
use otap_df_engine::testing::exporter::{TestRuntime, create_exporter_from_factory};
use otap_df_engine::{
    ExporterFactory, Interests,
    control::{CallData, PipelineControlMsg},
};
use otap_df_pdata::OtlpProtoBytes;
use prost::Message;
use serde_json::Value;
use std::ops::Add;
use std::process::Command;
use std::time::Instant;

/// Generates a CA certificate in the given directory.
pub fn generate_ca(dir: &std::path::Path) {
    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            "ca.key",
            "-out",
            "ca.crt",
            "-days",
            "365",
            "-nodes",
            "-subj",
            "/CN=Test CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
        ])
        .current_dir(dir)
        .output()
        .expect("Failed to generate CA");
    if !status.status.success() {
        panic!("CA gen failed: {}", String::from_utf8_lossy(&status.stderr));
    }
}

/// Generates a server certificate signed by the CA in the given directory.
pub fn generate_server_cert(dir: &std::path::Path, cn: &str) {
    // Generate Server Key and CSR
    let status = Command::new("openssl")
        .args([
            "req",
            "-newkey",
            "rsa:2048",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
            "-nodes",
            "-subj",
            &format!("/CN={}", cn),
            "-addext",
            &format!("subjectAltName=DNS:{},IP:127.0.0.1", cn),
        ])
        .current_dir(dir)
        .output()
        .expect("Failed to generate CSR");
    if !status.status.success() {
        panic!(
            "CSR gen failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    // Sign Server CSR with CA
    let status = Command::new("openssl")
        .args([
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "ca.crt",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-out",
            "server.crt",
            "-days",
            "365",
            "-copy_extensions",
            "copy",
        ])
        .current_dir(dir)
        .output()
        .expect("Failed to sign cert");
    if !status.status.success() {
        panic!("Sign failed: {}", String::from_utf8_lossy(&status.stderr));
    }
}

/// Generates test certificates (CA and server cert with CN=localhost) in the given directory.
pub fn generate_test_certs(dir: &std::path::Path) {
    generate_ca(dir);
    generate_server_cert(dir, "localhost");
}

/// TestCallData helps test the CallData type.
#[derive(Eq, PartialEq, Debug, Clone)]
pub struct TestCallData {
    id0: u64,
    id1: usize,
}

impl TestCallData {
    /// Create test calldata
    #[must_use]
    pub fn new_with(id0: u64, id1: usize) -> Self {
        Self { id0, id1 }
    }
}

impl Default for TestCallData {
    fn default() -> TestCallData {
        TestCallData::new_with(123, 4567)
    }
}

impl From<TestCallData> for CallData {
    fn from(value: TestCallData) -> Self {
        smallvec::smallvec![value.id0.into(), value.id1.into()]
    }
}

impl TryFrom<CallData> for TestCallData {
    type Error = otap_df_engine::error::Error;

    fn try_from(value: CallData) -> Result<Self, Self::Error> {
        if value.len() != 2 {
            return Err(Self::Error::InternalError {
                message: "invalid calldata".into(),
            });
        }
        Ok(Self {
            id0: value[0].into(),
            id1: value[1].try_into()?,
        })
    }
}

/// Create minimal test pdata
#[must_use]
pub fn create_test_pdata() -> OtapPdata {
    // Note this has to be one log record for existing tests.
    let otlp_service_req = otap_df_pdata::testing::fixtures::log_with_no_scope();
    let mut otlp_bytes = vec![];
    otlp_service_req
        .encode(&mut otlp_bytes)
        .expect("failed to encode otlp request");

    OtapPdata::new_default(OtlpProtoBytes::ExportLogsRequest(Bytes::from(otlp_bytes)).into())
}

/// Simple exporter test where there is NO subscribe_to() in the context.
pub fn test_exporter_no_subscription(factory: &ExporterFactory<OtapPdata>, config: Value) {
    let test_runtime = TestRuntime::new();
    let exporter =
        create_exporter_from_factory(factory, config).expect("failed to create exporter");

    test_runtime
        .set_exporter(exporter)
        .run_test(|ctx| async move {
            ctx.send_pdata(create_test_pdata())
                .await
                .expect("failed to send pdata");
            ctx.send_shutdown(
                Instant::now().add(std::time::Duration::from_secs(1)),
                "test shutdown",
            )
            .await
            .expect("failed to send shutdown");
        })
        .run_validation(|mut ctx, result| async move {
            result.expect("success");

            let mut pipeline_rx = ctx
                .take_pipeline_ctrl_receiver()
                .expect("failed to take pipeline ctrl receiver");

            match pipeline_rx.recv().await {
                Ok(received_msg) => {
                    panic!("expected no pipeline control messages, received: {received_msg:?}");
                }
                Err(err) => {
                    assert!(err.to_string().contains("channel is closed"));
                }
            }
        });
}

/// Simple exporter test where there is a subscribe_to() in the context.
pub fn test_exporter_with_subscription(
    factory: &ExporterFactory<OtapPdata>,
    config: Value,
    subscribe_interests: Interests,
    expect_interest: Interests,
) {
    let test_runtime = TestRuntime::new();
    let exporter =
        create_exporter_from_factory(factory, config).expect("failed to create exporter");
    test_runtime
        .set_exporter(exporter)
        .run_test(move |ctx| async move {
            let req_data = create_test_pdata()
                .test_subscribe_to(subscribe_interests, TestCallData::default().into(), 654321);
            ctx.send_pdata(req_data).await.expect("failed to send pdata");
            ctx.send_shutdown(Instant::now().add(std::time::Duration::from_secs(1)), "test shutdown")
                .await
                .expect("failed to send shutdown");
        })
        .run_validation(|mut ctx, result| async move {
            result.expect("success");

            let mut pipeline_rx = ctx.take_pipeline_ctrl_receiver().expect("failed to take pipeline ctrl receiver");
            let (trigger, calldata, reqdata, reason) = match pipeline_rx.recv().await {
                Ok(PipelineControlMsg::DeliverAck { ack, node_id }) => {
                    assert_eq!(node_id, 654321);
                    (Interests::ACKS, ack.calldata, Some(ack.accepted), "success".into())
                }
                Ok(PipelineControlMsg::DeliverNack { nack, node_id }) => {
                    assert_eq!(node_id, 654321);
                    (Interests::NACKS, nack.calldata, Some(nack.refused), nack.reason)
                }
                Ok(other) => (
                    Interests::empty(),
                    CallData::default(),
                    None,
                    format!("other message {other:?}"),
                ),
                Err(err) => (
                    Interests::empty(),
                    CallData::default(),
                    None,
                    format!("error {err:?}"),
                ),
            };
            assert_eq!(expect_interest&Interests::ACKS_OR_NACKS, trigger);

            if !trigger.is_empty() {
                let got: TestCallData = calldata.try_into().expect("failed to convert calldata");
                assert_eq!(TestCallData::default(), got);
                assert_eq!(
                    reason,
                    if trigger == Interests::NACKS { "THIS specific error" } else { "success" },
                );

                assert_eq!(reqdata.expect("has payload").num_items(),
                           if (subscribe_interests & Interests::RETURN_DATA).is_empty() {
                               0
                           } else {
                               1
                           });

            } else {
                assert!(
                    reason.contains("Closed"),
                    "subscribed {subscribe_interests:?}: expecting {expect_interest:?}: trigger {trigger:?}: failed reason {reason}",
                );
                assert_eq!(calldata.len(), 0);
            }
        });
}
