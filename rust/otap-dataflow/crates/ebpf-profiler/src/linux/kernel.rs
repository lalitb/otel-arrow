// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Linux kernel compatibility constants.

pub(crate) const EVENTS_MAP_NAME: &str = "EVENTS";
pub(crate) const EVENTS_MAP_ENTRIES: u32 = 4096;

pub(crate) fn program_name(include_kernel_stacks: bool) -> &'static str {
    if include_kernel_stacks {
        "profile_cpu_kernel"
    } else {
        "profile_cpu"
    }
}
