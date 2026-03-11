// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Example configuration resources.
//!
//! Provides access to the bundled example pipeline configurations that
//! ship with the OTAP dataflow engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Metadata for an example config file.
#[derive(Debug, Clone, Serialize)]
pub struct ExampleConfig {
    /// File name (e.g., `fake-otlp.yaml`).
    pub name: String,
    /// Brief description inferred from the file name.
    pub description: String,
}

/// Store for example configuration files.
#[derive(Debug, Clone)]
pub struct ExampleStore {
    configs: HashMap<String, String>,
}

impl ExampleStore {
    /// Creates a new example store by scanning the given directory for YAML files.
    pub fn from_directory(dir: &Path) -> std::io::Result<Self> {
        let mut configs = HashMap::new();

        if dir.is_dir() {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    if (ext == "yaml" || ext == "yml") && path.is_file() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if let Ok(content) = std::fs::read_to_string(&path) {
                                let _prev = configs.insert(name.to_string(), content);
                            }
                        }
                    }
                }
            }
        }

        Ok(Self { configs })
    }

    /// Creates an empty example store (used when no config directory is provided).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            configs: HashMap::new(),
        }
    }

    /// Lists all available example configs.
    #[must_use]
    pub fn list(&self) -> Vec<ExampleConfig> {
        let mut examples: Vec<ExampleConfig> = self
            .configs
            .keys()
            .map(|name| ExampleConfig {
                name: name.clone(),
                description: describe_config(name),
            })
            .collect();
        examples.sort_by(|a, b| a.name.cmp(&b.name));
        examples
    }

    /// Gets the YAML content of a specific example config.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.configs.get(name).map(|s| s.as_str())
    }

    /// Returns the number of loaded example configs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.configs.len()
    }

    /// Returns true if no example configs are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.configs.is_empty()
    }
}

/// Infers a human-readable description from a config file name.
fn describe_config(name: &str) -> String {
    let stem = name.trim_end_matches(".yaml").trim_end_matches(".yml");
    let parts: Vec<&str> = stem.split('-').collect();

    if parts.is_empty() {
        return "Pipeline configuration".to_string();
    }

    // Attempt to describe common patterns
    let description = parts
        .join(" → ")
        .replace("fake", "synthetic data generator");
    format!("Pipeline: {description}")
}

/// Resolves the default config directory path relative to the binary location
/// or a well-known path.
#[must_use]
pub fn default_config_dir() -> Option<PathBuf> {
    // Try relative to current directory first
    let candidates = [
        PathBuf::from("configs"),
        PathBuf::from("../configs"),
        PathBuf::from("../../configs"),
    ];

    for candidate in &candidates {
        if candidate.is_dir() {
            return Some(candidate.clone());
        }
    }

    None
}
