// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Shared test utilities for integration tests.
//!
//! Integration tests compile as separate binaries. Some shared modules are only
//! used by a subset of test binaries, so allow dead_code at module scope.

#[allow(dead_code)]
pub mod counting_exporter;
#[allow(dead_code)]
pub mod flaky_exporter;
#[cfg(feature = "experimental-tls")]
#[allow(dead_code)]
pub mod tls_helpers;
