// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin API version, used during compatibility negotiation between the
//! host and a plugin's `descriptor()` export.
//!
//! Phase 1 fixes the API at `0.1`. Additive minor bumps are allowed; major
//! bumps are an explicit break and require host opt-in.

use serde::{Deserialize, Serialize};

/// Versioned plugin-API identity.
///
/// Compatibility rule (phase 1):
///   `host.major == plugin.major && plugin.minor <= host.minor`
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PluginApiVersion {
    /// Major version. A bump means the WIT world is not backward-compatible.
    pub major: u32,
    /// Minor version. Additive WIT-field changes only.
    pub minor: u32,
}

impl PluginApiVersion {
    /// Construct a new version pair.
    #[must_use]
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Returns true if this host can load a plugin built against `plugin`.
    #[must_use]
    pub const fn is_compatible_with(&self, plugin: &Self) -> bool {
        self.major == plugin.major && plugin.minor <= self.minor
    }
}

impl std::fmt::Display for PluginApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The version this build of the host implements.
pub const PLUGIN_API_VERSION: PluginApiVersion = PluginApiVersion::new(0, 1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compat_rules() {
        let host = PluginApiVersion::new(0, 1);
        assert!(host.is_compatible_with(&PluginApiVersion::new(0, 0)));
        assert!(host.is_compatible_with(&PluginApiVersion::new(0, 1)));
        assert!(!host.is_compatible_with(&PluginApiVersion::new(0, 2)));
        assert!(!host.is_compatible_with(&PluginApiVersion::new(1, 0)));
    }
}
