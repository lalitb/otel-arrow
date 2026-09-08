// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Minimal native-only upstream global values.

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactArchitecture;

/// Kernel-version-specific offsets discovered from the upstream task probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KernelStackLayout {
    /// Byte offset of `task_struct.stack`.
    pub task_stack_offset: u32,
    /// Byte offset of syscall-entry `pt_regs` within the kernel stack.
    pub stack_ptregs_offset: u32,
}

/// Load-time values consumed by native sampling programs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeGlobals {
    /// Enable bounded kernel debug output.
    pub with_debug_output: u32,
    /// Drop error-only traces in kernel space.
    pub filter_error_frames: u8,
    /// Disable Go custom-label extraction.
    pub go_labels_disabled: u8,
    /// Drop idle PID 0 samples.
    pub filter_idle_frames: u8,
    /// Inverse ARM pointer-authentication mask.
    pub inverse_pac_mask: u64,
    /// Disable the optional VMA helper path in native-only mode.
    pub vma_lookup_enabled: u8,
    /// Sampling origin ID, which must be non-zero.
    pub origin_id_sampling: u16,
    /// Disable minimum-process-age filtering.
    pub filter_min_process_age_ns: u64,
    /// Disable PID namespace translation by default.
    pub pid_ns_translation_enabled: u8,
    /// Target PID namespace device when translation is enabled.
    pub target_pid_ns_dev: u64,
    /// Target PID namespace inode when translation is enabled.
    pub target_pid_ns_inode: u64,
    /// Discovered offset of `task_struct.stack`.
    pub task_stack_offset: u32,
    /// Discovered syscall-entry register offset within the kernel stack.
    pub stack_ptregs_offset: u32,
}

impl NativeGlobals {
    /// Returns conservative values for user-mode native unwinding.
    #[must_use]
    pub fn minimal(architecture: ArtifactArchitecture, inverse_pac_mask: Option<u64>) -> Self {
        Self {
            with_debug_output: 0,
            filter_error_frames: 0,
            go_labels_disabled: 1,
            filter_idle_frames: 1,
            inverse_pac_mask: match architecture {
                ArtifactArchitecture::Amd64 => u64::MAX,
                ArtifactArchitecture::Arm64 => inverse_pac_mask.unwrap_or(0),
                ArtifactArchitecture::Unknown => 0,
            },
            vma_lookup_enabled: 0,
            origin_id_sampling: 1,
            filter_min_process_age_ns: 0,
            pid_ns_translation_enabled: 0,
            target_pid_ns_dev: 0,
            target_pid_ns_inode: 0,
            task_stack_offset: 0,
            stack_ptregs_offset: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Native-only amd64 configuration is constructed without kernel probing.
    /// Guarantees: Sampling has a non-zero origin and managed-runtime labels and
    /// optional VMA helpers are disabled.
    #[test]
    fn amd64_minimal_globals_disable_optional_paths() {
        let globals = NativeGlobals::minimal(ArtifactArchitecture::Amd64, None);
        assert_eq!(globals.origin_id_sampling, 1);
        assert_eq!(globals.go_labels_disabled, 1);
        assert_eq!(globals.vma_lookup_enabled, 0);
        assert_eq!(globals.inverse_pac_mask, u64::MAX);
    }
}
