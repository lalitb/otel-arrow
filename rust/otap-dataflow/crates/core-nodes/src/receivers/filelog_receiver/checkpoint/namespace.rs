// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Stable version-1 checkpoint namespace derivation.
//!
//! A raw `checkpoint.id` is never used as a filesystem component. The exact
//! validated UTF-8 bytes are encoded as lowercase hexadecimal under the
//! versioned `filelog/@v1` namespace root. This keeps the mapping injective
//! on case-insensitive filesystems and makes logical IDs such as `.` and
//! `..` ordinary encoded components.

use std::path::{Component, Path, PathBuf};

/// Directory below the engine state directory that owns filelog state.
pub const FILELOG_NAMESPACE_DIRECTORY: &str = "filelog";
/// Versioned filelog checkpoint namespace-layout directory.
pub const CHECKPOINT_NAMESPACE_VERSION: &str = "@v1";
/// Common maximum byte length of one filesystem component.
pub const CHECKPOINT_NAMESPACE_COMPONENT_MAX_BYTES: usize = 255;
/// Maximum accepted raw `checkpoint.id` length.
///
/// Each exact input byte expands to two lowercase hexadecimal bytes, so 127
/// raw bytes encode to 254 bytes while 128 would encode to 256 and exceed
/// the common 255-byte component bound.
pub const CHECKPOINT_NAMESPACE_ID_MAX_BYTES: usize = CHECKPOINT_NAMESPACE_COMPONENT_MAX_BYTES / 2;

/// A checkpoint namespace derivation or raw-ID validation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointNamespaceError {
    /// The raw namespace ID was empty.
    #[error("checkpoint.id must not be empty")]
    EmptyId,
    /// The raw namespace ID contained a byte outside the accepted ASCII
    /// alphabet.
    #[error(
        "checkpoint.id byte 0x{byte:02x} at offset {offset} is not an ASCII alphanumeric, '_', \
         '-', or '.'"
    )]
    InvalidIdByte {
        /// Zero-based byte offset of the rejected byte.
        offset: usize,
        /// Rejected byte.
        byte: u8,
    },
    /// The exact raw ID would expand past the filesystem component bound.
    #[error(
        "checkpoint.id is {raw_len} bytes, exceeding the 127-byte maximum: lowercase hexadecimal \
         encoding would exceed the 255-byte namespace component bound"
    )]
    IdTooLong {
        /// Raw UTF-8 byte length supplied by the caller.
        raw_len: usize,
    },
}

/// One validated version-1 filelog checkpoint namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointNamespace {
    raw_id: String,
    encoded_component: String,
    directory: PathBuf,
}

impl CheckpointNamespace {
    /// Validates `checkpoint_id` and derives its namespace below
    /// `engine_state_dir`.
    pub fn derive(
        engine_state_dir: impl AsRef<Path>,
        checkpoint_id: &str,
    ) -> Result<Self, CheckpointNamespaceError> {
        Self::validate_id(checkpoint_id)?;
        let encoded_component = hex::encode(checkpoint_id.as_bytes());
        debug_assert!(encoded_component.len() <= CHECKPOINT_NAMESPACE_COMPONENT_MAX_BYTES);

        let directory = strip_leading_curdir(
            &engine_state_dir
                .as_ref()
                .join(FILELOG_NAMESPACE_DIRECTORY)
                .join(CHECKPOINT_NAMESPACE_VERSION)
                .join(&encoded_component),
        );
        Ok(Self {
            raw_id: checkpoint_id.to_owned(),
            encoded_component,
            directory,
        })
    }

    /// Validates the exact raw `checkpoint.id` accepted by the version-1
    /// runtime and administration APIs.
    pub fn validate_id(checkpoint_id: &str) -> Result<(), CheckpointNamespaceError> {
        if checkpoint_id.is_empty() {
            return Err(CheckpointNamespaceError::EmptyId);
        }
        if checkpoint_id.len() > CHECKPOINT_NAMESPACE_ID_MAX_BYTES {
            return Err(CheckpointNamespaceError::IdTooLong {
                raw_len: checkpoint_id.len(),
            });
        }
        if let Some((offset, byte)) = checkpoint_id
            .bytes()
            .enumerate()
            .find(|(_, byte)| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(CheckpointNamespaceError::InvalidIdByte { offset, byte });
        }
        Ok(())
    }

    /// Exact raw namespace ID.
    #[must_use]
    pub fn raw_id(&self) -> &str {
        &self.raw_id
    }

    /// Lowercase-hex filesystem component derived from the exact raw ID.
    #[must_use]
    pub fn encoded_component(&self) -> &str {
        &self.encoded_component
    }

    /// Derived `${engine.state_dir}/filelog/@v1/<checkpoint-id-hex>` path.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Consumes the value and returns its derived directory.
    #[must_use]
    pub fn into_directory(self) -> PathBuf {
        self.directory
    }
}

fn strip_leading_curdir(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    if matches!(components.peek(), Some(Component::CurDir)) {
        let _ = components.next();
    }
    components.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: mixed- and lowercase IDs are derived below the same
    /// versioned state root.
    /// Guarantees: exact UTF-8 bytes become lowercase hexadecimal and ASCII
    /// case remains distinguishable even on a case-insensitive filesystem.
    #[test]
    fn namespace_derivation_preserves_exact_id_bytes() {
        let mixed = CheckpointNamespace::derive("state", "AppLogs").unwrap();
        let lower = CheckpointNamespace::derive("state", "applogs").unwrap();

        assert_eq!(mixed.encoded_component(), "4170704c6f6773");
        assert_eq!(lower.encoded_component(), "6170706c6f6773");
        assert_eq!(
            mixed.directory(),
            Path::new("state/filelog/@v1/4170704c6f6773")
        );
        assert_eq!(
            lower.directory(),
            Path::new("state/filelog/@v1/6170706c6f6773")
        );
        assert_ne!(mixed.directory(), lower.directory());
    }

    /// Scenario: the logical checkpoint IDs `.` and `..` are derived as
    /// namespaces.
    /// Guarantees: neither value is interpreted as a filesystem traversal
    /// component; both are safely represented by their exact byte encoding.
    #[test]
    fn namespace_derivation_safely_encodes_dot_ids() {
        assert_eq!(
            CheckpointNamespace::derive("state", ".")
                .unwrap()
                .directory(),
            Path::new("state/filelog/@v1/2e")
        );
        assert_eq!(
            CheckpointNamespace::derive("state", "..")
                .unwrap()
                .directory(),
            Path::new("state/filelog/@v1/2e2e")
        );
    }

    /// Scenario: raw checkpoint IDs are exactly 127 and 128 ASCII bytes.
    /// Guarantees: the 254-byte encoded component is accepted and the
    /// 256-byte component is rejected before any filesystem access.
    #[test]
    fn namespace_derivation_enforces_hex_expansion_boundary() {
        let accepted =
            CheckpointNamespace::derive("state", &"a".repeat(127)).expect("127 bytes fit");
        assert_eq!(accepted.encoded_component().len(), 254);

        assert!(matches!(
            CheckpointNamespace::derive("state", &"a".repeat(128)),
            Err(CheckpointNamespaceError::IdTooLong { raw_len: 128 })
        ));
    }

    /// Scenario: a checkpoint ID is empty or contains bytes outside the
    /// accepted ASCII alphabet.
    /// Guarantees: validation returns typed errors instead of deriving an
    /// empty, Unicode-normalized, or path-shaped namespace.
    #[test]
    fn namespace_derivation_rejects_invalid_ids() {
        assert_eq!(
            CheckpointNamespace::derive("state", "").unwrap_err(),
            CheckpointNamespaceError::EmptyId
        );
        assert!(matches!(
            CheckpointNamespace::derive("state", "app/log"),
            Err(CheckpointNamespaceError::InvalidIdByte {
                offset: 3,
                byte: b'/'
            })
        ));
        assert!(matches!(
            CheckpointNamespace::derive("state", "caf\u{00e9}"),
            Err(CheckpointNamespaceError::InvalidIdByte { .. })
        ));
    }
}
