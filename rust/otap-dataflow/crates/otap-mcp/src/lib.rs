// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! MCP (Model Context Protocol) server for the OTAP dataflow engine.
//!
//! Provides AI-assisted discovery, configuration, and operational management
//! of the OTAP dataflow engine through the Model Context Protocol.
//!
//! # Modes
//!
//! - **Static mode** (always available): Component discovery, config schemas,
//!   example configs, validation, and generation.
//! - **Runtime mode** (when `--admin-url` is provided): Live pipeline status,
//!   metrics, health checks, and shutdown via the admin HTTP API.

pub mod error;
pub mod prompts;
pub mod resources;
pub mod runtime;
pub mod server;
pub mod tools;
