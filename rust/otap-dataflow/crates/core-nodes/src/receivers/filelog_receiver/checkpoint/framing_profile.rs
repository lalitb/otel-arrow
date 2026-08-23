// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Canonical serialization and SHA-256 digest for the framing and identity
//! resumption-compatibility contract.
//!
//! See `docs/filelog-checkpoint-format.md`,
//! "Framing-profile canonical serialization and digest", for the exact byte
//! layout and the two published compatibility vectors this module must
//! continue to reproduce exactly.

use sha2::{Digest, Sha256};

use super::error::EncodeError;
use super::primitives::{ByteWriter, FRAMING_PATTERN_MAX_BYTES};

const DOMAIN_PREFIX: &[u8] = b"otel-arrow-filelog-framing-profile-v2\0";

/// Configured character encoding (decode-before-framing contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingEncoding {
    /// UTF-8, the default.
    Utf8,
    /// ASCII.
    Ascii,
    /// UTF-16, little-endian.
    Utf16Le,
    /// UTF-16, big-endian.
    Utf16Be,
    /// Raw bytes; no character validation.
    Raw,
}

impl FramingEncoding {
    fn to_wire(self) -> u8 {
        match self {
            FramingEncoding::Utf8 => 0x01,
            FramingEncoding::Ascii => 0x02,
            FramingEncoding::Utf16Le => 0x03,
            FramingEncoding::Utf16Be => 0x04,
            FramingEncoding::Raw => 0x05,
        }
    }
}

/// Configured oversize-record policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxLogSizeBehavior {
    /// Preserve all input by emitting bounded fragments.
    Split,
    /// Emit the bounded prefix and discard the remainder.
    Truncate,
}

impl MaxLogSizeBehavior {
    fn to_wire(self) -> u8 {
        match self {
            MaxLogSizeBehavior::Split => 0x01,
            MaxLogSizeBehavior::Truncate => 0x02,
        }
    }
}

/// Configured multiline boundary mode. Zero or one of a start/end pattern;
/// setting neither selects newline framing (the default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultilineMode {
    /// Newline framing; no pattern.
    Newline,
    /// Buffer until the next start-pattern match, which begins the next
    /// record.
    StartPattern {
        /// Versioned RE2-compatible regex profile number.
        regex_profile_version: u16,
        /// The configured pattern source.
        pattern: String,
    },
    /// Buffer until an end-pattern match, which is included in the record.
    EndPattern {
        /// Versioned RE2-compatible regex profile number.
        regex_profile_version: u16,
        /// The configured pattern source.
        pattern: String,
    },
}

impl MultilineMode {
    fn to_wire(&self) -> (u8, u16, &[u8]) {
        match self {
            MultilineMode::Newline => (0x00, 0, b""),
            MultilineMode::StartPattern {
                regex_profile_version,
                pattern,
            } => (0x01, *regex_profile_version, pattern.as_bytes()),
            MultilineMode::EndPattern {
                regex_profile_version,
                pattern,
            } => (0x02, *regex_profile_version, pattern.as_bytes()),
        }
    }
}

/// The subset of Appendix C configuration that affects identity matching,
/// record boundaries, or deterministic replay, exactly as enumerated in
/// "Framing-profile canonical serialization and digest".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramingProfileParams {
    /// Versioned fingerprint algorithm/profile.
    pub fingerprint_profile_version: u16,
    /// Configured fingerprint evidence window.
    pub fingerprint_bytes: u16,
    /// Configured prefix skipped before fingerprint evidence.
    pub ignored_header_bytes: u32,
    /// Configured character encoding.
    pub encoding: FramingEncoding,
    /// Configured multiline boundary mode.
    pub multiline_mode: MultilineMode,
    /// Physical-line buffer bound, in bytes.
    pub max_line_bytes: u64,
    /// Logical-record body bound, in bytes.
    pub max_record_bytes: u64,
    /// Oversize-record policy.
    pub max_log_size_behavior: MaxLogSizeBehavior,
    /// Bounded multiline line-count limit.
    pub max_multiline_lines: u32,
    /// Idle partial-flush period, in milliseconds; `0` disables idle
    /// flushing.
    pub force_flush_period_millis: u64,
}

impl FramingProfileParams {
    /// Produces the exact canonical byte sequence fed to the digest.
    ///
    /// Returns [`EncodeError`] if the configured pattern exceeds
    /// `FRAMING_PATTERN_MAX_BYTES`.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, EncodeError> {
        let mut out = ByteWriter::new();
        out.write_bytes(DOMAIN_PREFIX);
        out.write_u16(self.fingerprint_profile_version);
        out.write_u16(self.fingerprint_bytes);
        out.write_u32(self.ignored_header_bytes);
        out.write_u8(self.encoding.to_wire());
        let (multiline_kind, regex_profile_version, pattern) = self.multiline_mode.to_wire();
        out.write_u8(multiline_kind);
        out.write_u16(regex_profile_version);
        out.write_var_bytes(
            "framing_profile.pattern",
            pattern,
            FRAMING_PATTERN_MAX_BYTES,
        )?;
        out.write_u64(self.max_line_bytes);
        out.write_u64(self.max_record_bytes);
        out.write_u8(self.max_log_size_behavior.to_wire());
        out.write_u32(self.max_multiline_lines);
        out.write_u64(self.force_flush_period_millis);
        Ok(out.into_bytes())
    }

    /// Computes the SHA-256 framing-profile digest over
    /// [`Self::canonical_bytes`].
    pub fn digest(&self) -> Result<[u8; 32], EncodeError> {
        let bytes = self.canonical_bytes()?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        Ok(hasher.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_newline_profile() -> FramingProfileParams {
        FramingProfileParams {
            fingerprint_profile_version: 1,
            fingerprint_bytes: 1000,
            ignored_header_bytes: 0,
            encoding: FramingEncoding::Utf8,
            multiline_mode: MultilineMode::Newline,
            max_line_bytes: 1_048_576,
            max_record_bytes: 1_048_576,
            max_log_size_behavior: MaxLogSizeBehavior::Split,
            max_multiline_lines: 500,
            force_flush_period_millis: 500,
        }
    }

    /// Scenario: computing the digest of the default newline-framing
    /// profile, matching the first published compatibility vector in
    /// `docs/filelog-checkpoint-format.md`.
    /// Guarantees: the canonical serialization length and SHA-256 digest
    /// exactly match the checked-in specification vector, so an accidental
    /// change to field order, widths, or the domain prefix is caught here.
    #[test]
    fn matches_published_default_profile_vector() {
        let profile = default_newline_profile();
        let bytes = profile.canonical_bytes().unwrap();
        assert_eq!(bytes.len(), 81);
        let digest = profile.digest().unwrap();
        assert_eq!(
            hex::encode(digest),
            "46c818a27f8cb6281a903e2a54c4fd72b38dd6e4d4ac30b1e33fb1e26ff2aaae"
        );
    }

    /// Scenario: computing the digest of an end-pattern multiline profile,
    /// matching the second published compatibility vector.
    /// Guarantees: changing the multiline mode and pattern changes both the
    /// canonical length and the digest deterministically and exactly, and
    /// the two profiles never collide.
    #[test]
    fn matches_published_end_pattern_profile_vector() {
        let mut profile = default_newline_profile();
        profile.multiline_mode = MultilineMode::EndPattern {
            regex_profile_version: 1,
            pattern: "^END request$".to_owned(),
        };
        let bytes = profile.canonical_bytes().unwrap();
        assert_eq!(bytes.len(), 94);
        let digest = profile.digest().unwrap();
        assert_eq!(
            hex::encode(digest),
            "7c5c808319692ce9d0b8c7a50772f53e236d60a98e50fd30e2c5cf073e7da179"
        );
        assert_ne!(digest, default_newline_profile().digest().unwrap());
    }

    /// Scenario: only the fingerprint evidence window or ignored-header
    /// count changes while every framing setting remains unchanged.
    /// Guarantees: identity configuration changes produce different durable
    /// compatibility digests and therefore cannot silently reuse checkpoints.
    #[test]
    fn identity_profile_changes_digest() {
        let base = default_newline_profile();
        let mut changed_window = base.clone();
        changed_window.fingerprint_bytes += 1;
        let mut changed_header = base.clone();
        changed_header.ignored_header_bytes = 1;

        assert_ne!(base.digest().unwrap(), changed_window.digest().unwrap());
        assert_ne!(base.digest().unwrap(), changed_header.digest().unwrap());
    }

    /// Scenario: a configured multiline pattern longer than
    /// `FRAMING_PATTERN_MAX_BYTES`.
    /// Guarantees: computing the canonical bytes fails with a structured
    /// `EncodeError` instead of silently truncating the pattern (which would
    /// make two different configurations collide on the same digest).
    #[test]
    fn oversized_pattern_is_rejected() {
        let mut profile = default_newline_profile();
        profile.multiline_mode = MultilineMode::StartPattern {
            regex_profile_version: 1,
            pattern: "a".repeat(FRAMING_PATTERN_MAX_BYTES + 1),
        };
        assert!(matches!(
            profile.canonical_bytes(),
            Err(EncodeError::FieldTooLong { .. })
        ));
    }
}
