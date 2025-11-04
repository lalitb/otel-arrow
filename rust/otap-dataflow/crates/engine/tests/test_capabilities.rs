// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Tests for component capabilities and fanout behavior.

use otap_df_engine::Capabilities;

#[test]
fn test_capabilities_default() {
    // Default capabilities should be readonly (non-mutating)
    let caps = Capabilities::default();
    assert!(
        !caps.mutates_data,
        "Default capabilities should be readonly"
    );
}

#[test]
fn test_capabilities_mutating() {
    // Components can declare they mutate data
    let caps = Capabilities { mutates_data: true };
    assert!(caps.mutates_data, "Mutating capabilities should be true");
}

#[test]
fn test_capabilities_readonly() {
    // Components can explicitly declare they're readonly
    let caps = Capabilities {
        mutates_data: false,
    };
    assert!(!caps.mutates_data, "Readonly capabilities should be false");
}

#[test]
fn test_capabilities_equality() {
    let caps1 = Capabilities { mutates_data: true };
    let caps2 = Capabilities { mutates_data: true };
    let caps3 = Capabilities {
        mutates_data: false,
    };

    assert_eq!(caps1, caps2, "Same capabilities should be equal");
    assert_ne!(caps1, caps3, "Different capabilities should not be equal");
}

#[test]
fn test_capabilities_clone() {
    let caps1 = Capabilities { mutates_data: true };
    let caps2 = caps1; // Copy trait should work

    assert_eq!(caps1, caps2, "Cloned capabilities should be equal");
}

#[test]
fn test_capabilities_copy() {
    let caps = Capabilities { mutates_data: true };
    let copied = caps;

    // Both should be usable (Copy trait)
    assert!(caps.mutates_data);
    assert!(copied.mutates_data);
}
