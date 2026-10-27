// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! OTAP integration tests, linked into one executable to reduce build and archive costs.

mod k8s_client_ca_reload;
mod mtls_tests;
mod otlp_exporter_proxy_tls;
mod otlp_exporter_tls;
mod tls_limits;
mod tls_reload;
mod tls_stream;
