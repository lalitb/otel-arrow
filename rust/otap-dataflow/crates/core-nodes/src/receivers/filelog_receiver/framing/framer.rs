// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded synchronous framing over the streaming source decoder.

use std::{str::Utf8Error, time::Instant};

use sha2::{Digest, Sha256};
use thiserror::Error;

use super::decoder::{
    DecodeError, DecodeEvent, DecodedValue, SourceBytes, SourceRange, StreamDecoder,
};
use crate::receivers::filelog_receiver::{
    Encoding, MaxLogSizeBehavior, OnDecodeError,
    checkpoint::{FileId, FramingResume},
    config::{CompiledMultilinePattern, RuntimeConfig, peak_framer_payload_bytes},
};

const FRAGMENT_ID_DOMAIN: &[u8] = b"otel-arrow-filelog-fragment-v1\0";
const MAX_DECODED_UNIT_BYTES: usize = 4;

/// The emitted OTAP body representation selected by framing and decode policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FramedBody {
    /// Valid decoded text encoded as UTF-8.
    Text(String),
    /// Raw mode, exact malformed evidence, or a preserve-raw split sequence.
    Bytes(Vec<u8>),
}

/// The observable decode result for one emitted frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecodeOutcome {
    /// Every represented source unit decoded cleanly.
    Clean,
    /// Malformed units were replaced with U+FFFD.
    Replacements {
        /// Number of replacement events represented by this frame.
        count: u64,
    },
    /// Exact source bytes were preserved because malformed units were seen.
    PreserveRaw {
        /// Number of malformed units represented by this frame.
        count: u64,
    },
}

/// A bounded-state flush or boundary reason attached to an emitted frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FlushReason {
    /// The configured multiline physical-line cap was reached.
    MaxLines,
    /// The configured EOF-gated idle period expired.
    Timeout,
    /// A prior multiline buffer was released before an oversized line.
    OversizeLineBoundary,
    /// A rotation-finalization hook released recoverable pending content.
    Rotation,
    /// A receiver-drain hook released recoverable pending content.
    Drain,
}

/// Stable correlation data for one fragment of an oversized logical record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FragmentMetadata {
    /// Lowercase hexadecimal SHA-256 fragment correlation id.
    pub(crate) id: String,
    /// Zero-based fragment index.
    pub(crate) index: u32,
    /// Whether this fragment terminates the split sequence.
    pub(crate) last: bool,
}

/// One framed body and its source-byte checkpoint contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FramedRecord {
    /// OTAP body value.
    pub(crate) body: FramedBody,
    /// Source bytes transformed into `body`, excluding framing-only bytes.
    pub(crate) body_source_range: SourceRange,
    /// Contiguous source progress owned by this output.
    ///
    /// This can begin before `body_source_range` for a stripped BOM and can
    /// end after it for a terminal LF or a truncated tail.
    pub(crate) frame_source_range: SourceRange,
    /// Ack-gated source offset resulting from this output.
    pub(crate) checkpoint_end: u64,
    /// Durable framing state paired with `checkpoint_end`.
    pub(crate) resulting_resume: FramingResume,
    /// Decode outcome represented by this output.
    pub(crate) decode_outcome: DecodeOutcome,
    /// Optional reason for a bounded or lifecycle-triggered flush.
    pub(crate) flush_reason: Option<FlushReason>,
    /// Split correlation metadata.
    pub(crate) fragment: Option<FragmentMetadata>,
    /// Whether the body is a retained prefix of discarded source content.
    pub(crate) truncated: bool,
    /// Exact number of discarded source body bytes.
    pub(crate) discarded_source_bytes: u64,
}

impl FramedRecord {
    /// Returns the contiguous checkpoint delta owned by this output.
    #[must_use]
    pub(crate) const fn owned_progress_range(&self) -> SourceRange {
        self.frame_source_range
    }
}

/// Progress and at most one frame produced by [`Framer::step`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FramerStep {
    /// Bytes consumed from the caller-owned prefix.
    pub(crate) consumed: usize,
    /// At most one source-ordered output.
    pub(crate) output: Option<FramedRecord>,
    /// Whether recoverable uncommitted source or framing state remains.
    pub(crate) pending: bool,
}

/// Result of an idle, rotation, or drain poll.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FlushStep {
    /// At most one source-ordered output.
    pub(crate) output: Option<FramedRecord>,
    /// Whether recoverable uncommitted source or framing state remains.
    pub(crate) pending: bool,
}

/// A terminal framing failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum FramerError {
    /// Streaming source decoding failed.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// Durable continuation index zero is impossible.
    #[error("continuation next_fragment_index must be greater than zero")]
    ContinuationIndexZero,
    /// Durable continuation must refer to a record that began before the
    /// committed continuation boundary.
    #[error(
        "continuation record start {record_start_offset} must be before committed offset {committed_offset}"
    )]
    ContinuationRecordStart {
        /// Durable original record start.
        record_start_offset: u64,
        /// Current committed offset.
        committed_offset: u64,
    },
    /// Durable continuation state is impossible under truncate policy.
    #[error("continuation framing state requires max_log_size_behavior=split")]
    ContinuationRequiresSplit,
    /// BOM handling may be enabled only at source offset zero.
    #[error("new stream start requires committed offset zero, got {committed_offset}")]
    InvalidNewStreamOffset {
        /// Nonzero offset incorrectly marked as a new stream.
        committed_offset: u64,
    },
    /// A validated runtime limit did not fit the current platform.
    #[error("{field} does not fit usize")]
    LimitDoesNotFitUsize {
        /// Runtime field that could not be represented.
        field: &'static str,
    },
    /// Runtime multiline mode and compiled matcher were inconsistent.
    #[error("validated runtime multiline matcher is missing or inconsistent")]
    InvalidRuntimeMatcher,
    /// A text matcher received bytes that violate the decoded UTF-8 invariant.
    #[error("multiline matcher input is not valid UTF-8")]
    MatcherUtf8 {
        /// The original UTF-8 validation failure.
        #[source]
        source: Utf8Error,
    },
    /// Checked offset, length, count, or capacity arithmetic overflowed.
    #[error("framer arithmetic overflow while computing {context}")]
    ArithmeticOverflow {
        /// Operation whose checked arithmetic failed.
        context: &'static str,
    },
    /// A bounded payload allocation failed.
    #[error("framer allocation failed while growing {context}")]
    Allocation {
        /// Buffer whose allocation failed.
        context: &'static str,
    },
    /// A source event was not contiguous with retained state.
    #[error("framer source discontinuity: expected {expected}, got {actual}")]
    SourceDiscontinuity {
        /// Expected source offset.
        expected: u64,
        /// Actual event offset.
        actual: u64,
    },
    /// Emitting a nonfinal fragment would make durable continuation
    /// unrepresentable.
    #[error("cannot emit nonfinal fragment at index u32::MAX")]
    FragmentIndexOverflow,
    /// The host `Instant` domain could not represent a configured deadline.
    #[error("framing deadline exceeds the host Instant domain")]
    DeadlineOverflow,
    /// A private state invariant was violated.
    #[error("framer invariant violated: {context}")]
    Invariant {
        /// Static description of the violated invariant.
        context: &'static str,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FramingMode {
    Newline,
    StartPattern,
    EndPattern,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PayloadKind {
    Raw,
    Text,
    PreserveRawText,
}

impl PayloadKind {
    const fn keeps_source_shadow(self) -> bool {
        matches!(self, Self::PreserveRawText)
    }

    fn measure(self, decoded_len: usize, source_len: usize) -> usize {
        match self {
            Self::Raw => source_len,
            Self::Text => decoded_len,
            Self::PreserveRawText => decoded_len.max(source_len),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Unit {
    range: SourceRange,
    source: SourceBytes,
    decoded: [u8; MAX_DECODED_UNIT_BYTES],
    decoded_len: u8,
    malformed: bool,
}

impl Unit {
    fn from_event(event: DecodeEvent) -> Result<Option<Self>, FramerError> {
        let DecodeEvent::Unit {
            range,
            source,
            value,
            malformed,
        } = event
        else {
            return Ok(None);
        };
        let mut decoded = [0; MAX_DECODED_UNIT_BYTES];
        let decoded_len = match value {
            DecodedValue::Scalar(value) => {
                let encoded = value.encode_utf8(&mut decoded);
                u8::try_from(encoded.len()).map_err(|_| FramerError::Invariant {
                    context: "UTF-8 scalar width exceeds four bytes",
                })?
            }
            DecodedValue::RawByte(value) => {
                decoded[0] = value;
                1
            }
        };
        Ok(Some(Self {
            range,
            source,
            decoded,
            decoded_len,
            malformed,
        }))
    }

    fn decoded_slice(&self) -> &[u8] {
        &self.decoded[..usize::from(self.decoded_len)]
    }

    fn is_lf(self) -> bool {
        self.decoded_len == 1 && self.decoded[0] == b'\n'
    }

    fn source_len(self) -> usize {
        self.source.as_slice().len()
    }
}

#[derive(Debug)]
struct Payload {
    decoded: Vec<u8>,
    source: Vec<u8>,
    source_len: usize,
    range: SourceRange,
    malformed_count: u64,
}

impl Payload {
    fn new(start: u64, _keep_source_shadow: bool) -> Self {
        Self {
            decoded: Vec::new(),
            source: Vec::new(),
            source_len: 0,
            range: SourceRange { start, end: start },
            malformed_count: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.source_len == 0
    }

    fn measure(&self, kind: PayloadKind) -> usize {
        kind.measure(self.decoded.len(), self.source_len)
    }

    fn prospective_measure(&self, unit: Unit, kind: PayloadKind) -> Result<usize, FramerError> {
        let decoded_len = self
            .decoded
            .len()
            .checked_add(usize::from(unit.decoded_len))
            .ok_or(FramerError::ArithmeticOverflow {
                context: "prospective decoded payload length",
            })?;
        let source_len = self.source_len.checked_add(unit.source_len()).ok_or(
            FramerError::ArithmeticOverflow {
                context: "prospective source payload length",
            },
        )?;
        Ok(kind.measure(decoded_len, source_len))
    }

    fn combined_measure(&self, other: &Self, kind: PayloadKind) -> Result<usize, FramerError> {
        let decoded_len = self.decoded.len().checked_add(other.decoded.len()).ok_or(
            FramerError::ArithmeticOverflow {
                context: "combined decoded payload length",
            },
        )?;
        let source_len = self.source_len.checked_add(other.source_len).ok_or(
            FramerError::ArithmeticOverflow {
                context: "combined source payload length",
            },
        )?;
        Ok(kind.measure(decoded_len, source_len))
    }

    fn append_unit(&mut self, unit: Unit, kind: PayloadKind) -> Result<(), FramerError> {
        if self.range.end != unit.range.start {
            return Err(FramerError::SourceDiscontinuity {
                expected: self.range.end,
                actual: unit.range.start,
            });
        }
        reserve(
            &mut self.decoded,
            usize::from(unit.decoded_len),
            "decoded payload",
        )?;
        self.decoded.extend_from_slice(unit.decoded_slice());
        if kind.keeps_source_shadow() {
            reserve(&mut self.source, unit.source_len(), "source shadow")?;
            self.source.extend_from_slice(unit.source.as_slice());
        }
        self.source_len = self.source_len.checked_add(unit.source_len()).ok_or(
            FramerError::ArithmeticOverflow {
                context: "source payload length",
            },
        )?;
        if unit.malformed {
            self.malformed_count =
                self.malformed_count
                    .checked_add(1)
                    .ok_or(FramerError::ArithmeticOverflow {
                        context: "malformed unit count",
                    })?;
        }
        self.range.end = unit.range.end;
        Ok(())
    }

    fn append_payload(&mut self, mut other: Self, kind: PayloadKind) -> Result<(), FramerError> {
        if self.range.end != other.range.start {
            return Err(FramerError::SourceDiscontinuity {
                expected: self.range.end,
                actual: other.range.start,
            });
        }
        reserve(
            &mut self.decoded,
            other.decoded.len(),
            "combined decoded payload",
        )?;
        self.decoded.append(&mut other.decoded);
        if kind.keeps_source_shadow() {
            reserve(
                &mut self.source,
                other.source.len(),
                "combined source shadow",
            )?;
            self.source.append(&mut other.source);
        }
        self.source_len = self.source_len.checked_add(other.source_len).ok_or(
            FramerError::ArithmeticOverflow {
                context: "combined source payload length",
            },
        )?;
        self.malformed_count = self
            .malformed_count
            .checked_add(other.malformed_count)
            .ok_or(FramerError::ArithmeticOverflow {
                context: "combined malformed unit count",
            })?;
        self.range.end = other.range.end;
        Ok(())
    }

    fn remove_suffix(&mut self, unit: Unit, kind: PayloadKind) -> Result<(), FramerError> {
        if self.range.end != unit.range.end {
            return Err(FramerError::Invariant {
                context: "terminal delimiter does not end the payload",
            });
        }
        let decoded_len = usize::from(unit.decoded_len);
        if !self.decoded.ends_with(unit.decoded_slice()) {
            return Err(FramerError::Invariant {
                context: "decoded terminal delimiter is not a payload suffix",
            });
        }
        let retained_decoded_len =
            self.decoded
                .len()
                .checked_sub(decoded_len)
                .ok_or(FramerError::Invariant {
                    context: "terminal delimiter decoded length exceeds payload",
                })?;
        self.decoded.truncate(retained_decoded_len);
        if kind.keeps_source_shadow() {
            if !self.source.ends_with(unit.source.as_slice()) {
                return Err(FramerError::Invariant {
                    context: "source terminal delimiter is not a payload suffix",
                });
            }
            let retained_source_len =
                self.source
                    .len()
                    .checked_sub(unit.source_len())
                    .ok_or(FramerError::Invariant {
                        context: "terminal delimiter source shadow exceeds payload",
                    })?;
            self.source.truncate(retained_source_len);
        }
        self.source_len =
            self.source_len
                .checked_sub(unit.source_len())
                .ok_or(FramerError::Invariant {
                    context: "terminal delimiter source length exceeds payload",
                })?;
        if unit.malformed {
            self.malformed_count =
                self.malformed_count
                    .checked_sub(1)
                    .ok_or(FramerError::Invariant {
                        context: "terminal delimiter malformed count exceeds payload",
                    })?;
        }
        self.range.end = unit.range.start;
        Ok(())
    }

    fn retain_prefix(&mut self, fit: LineFit, kind: PayloadKind) -> Result<(), FramerError> {
        let represented_source_len = fit
            .end
            .checked_sub(self.range.start)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(FramerError::ArithmeticOverflow {
                context: "fitting line-prefix source range",
            })?;
        if fit.decoded_len > self.decoded.len()
            || fit.source_len > self.source_len
            || fit.source_len != represented_source_len
            || fit.malformed_count > self.malformed_count
            || fit.end < self.range.start
            || fit.end > self.range.end
        {
            return Err(FramerError::Invariant {
                context: "fitting line prefix exceeds the physical line",
            });
        }
        if !matches!(kind, PayloadKind::Raw)
            && std::str::from_utf8(&self.decoded[..fit.decoded_len]).is_err()
        {
            return Err(FramerError::Invariant {
                context: "fitting line prefix cuts a decoded scalar",
            });
        }
        self.decoded.truncate(fit.decoded_len);
        if kind.keeps_source_shadow() {
            if fit.source_len > self.source.len() {
                return Err(FramerError::Invariant {
                    context: "fitting line prefix exceeds its source shadow",
                });
            }
            self.source.truncate(fit.source_len);
        }
        self.source_len = fit.source_len;
        self.malformed_count = fit.malformed_count;
        self.range.end = fit.end;
        Ok(())
    }
}

fn reserve(vec: &mut Vec<u8>, additional: usize, context: &'static str) -> Result<(), FramerError> {
    vec.try_reserve(additional)
        .map_err(|_| FramerError::Allocation { context })
}

#[derive(Debug)]
struct CompleteLine {
    content: Payload,
    delimiter: Unit,
    record_fit: Option<LineFit>,
}

#[derive(Clone, Copy, Debug)]
struct LineFit {
    decoded_len: usize,
    source_len: usize,
    malformed_count: u64,
    end: u64,
}

#[derive(Debug)]
struct BufferedRecord {
    payload: Payload,
    line_count: u32,
    terminal_delimiter: Unit,
}

#[derive(Debug)]
struct SplitState {
    record_start: u64,
    index: u32,
    current: Payload,
    emit_current_nonfinal: bool,
}

#[derive(Debug)]
struct TruncateState {
    prefix: Payload,
    discarded_source_bytes: u64,
    discarded_malformed_count: u64,
}

#[derive(Debug)]
enum OversizeState {
    Split(SplitState),
    Truncate(TruncateState),
}

/// Synchronous, worker-owned decoder and bounded logical-record framer.
///
/// Peak heap payload allocation is conservatively bounded by:
///
/// `4 * copies * (min(max_line_bytes, max_record_bytes) + max_record_bytes)
///  + 16 * copies + 16`
///
/// where `copies` is two only for text `preserve_raw` (decoded UTF-8 plus
/// exact source shadow), and one otherwise. The factor four covers allocator
/// growth slack and the transient overlap of old and new allocations during
/// reallocation. The fixed terms cover minimum small-vector allocations and
/// one pending decoded/source unit. Decoder state, counters, regex state, and
/// offsets are constant-size. An output moved to the caller is no longer
/// retained by the framer.
#[derive(Debug)]
pub(crate) struct Framer {
    file_id: FileId,
    file_epoch: u32,
    encoding: Encoding,
    decode_policy: OnDecodeError,
    payload_kind: PayloadKind,
    mode: FramingMode,
    matcher: Option<CompiledMultilinePattern>,
    max_record_bytes: usize,
    line_limit: usize,
    max_multiline_lines: u32,
    oversize_behavior: MaxLogSizeBehavior,
    force_flush_period: std::time::Duration,
    decoder: StreamDecoder,
    next_frame_start: u64,
    line: Payload,
    line_record_fit: Option<LineFit>,
    record: Option<BufferedRecord>,
    complete_line: Option<CompleteLine>,
    pending_unit: Option<Unit>,
    oversize: Option<OversizeState>,
    pattern_not_matched: u64,
    deadline: Option<Instant>,
}

impl Framer {
    /// Creates a framer from one already validated runtime configuration.
    ///
    /// The runtime's compiled matcher is cloned; its source is never
    /// recompiled or revalidated here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        file_id: FileId,
        file_epoch: u32,
        runtime: &RuntimeConfig,
        committed_offset: u64,
        resume: FramingResume,
        new_stream_start: bool,
        now: Instant,
    ) -> Result<Self, FramerError> {
        if new_stream_start && committed_offset != 0 {
            return Err(FramerError::InvalidNewStreamOffset { committed_offset });
        }
        if !runtime.framing.force_flush_period.is_zero() {
            let _ = now
                .checked_add(runtime.framing.force_flush_period)
                .ok_or(FramerError::DeadlineOverflow)?;
        }
        let max_line_bytes = usize::try_from(runtime.framing.max_line_bytes).map_err(|_| {
            FramerError::LimitDoesNotFitUsize {
                field: "framing.max_line_bytes",
            }
        })?;
        let max_record_bytes = usize::try_from(runtime.framing.max_record_bytes).map_err(|_| {
            FramerError::LimitDoesNotFitUsize {
                field: "framing.max_record_bytes",
            }
        })?;
        let mode = if runtime.framing.multiline.line_start_pattern.is_some() {
            FramingMode::StartPattern
        } else if runtime.framing.multiline.line_end_pattern.is_some() {
            FramingMode::EndPattern
        } else {
            FramingMode::Newline
        };
        let matcher = runtime.compiled_multiline_pattern.clone();
        if matches!(mode, FramingMode::Newline) != matcher.is_none() {
            return Err(FramerError::InvalidRuntimeMatcher);
        }
        if let Some(matcher) = &matcher {
            let variant_matches = matches!(
                (runtime.encoding, matcher),
                (Encoding::Raw, CompiledMultilinePattern::Raw(_))
                    | (
                        Encoding::Utf8 | Encoding::Ascii | Encoding::Utf16Le | Encoding::Utf16Be,
                        CompiledMultilinePattern::Text(_)
                    )
            );
            if !variant_matches {
                return Err(FramerError::InvalidRuntimeMatcher);
            }
        }

        let payload_kind = match (runtime.encoding, runtime.on_decode_error) {
            (Encoding::Raw, _) => PayloadKind::Raw,
            (_, OnDecodeError::PreserveRaw) => PayloadKind::PreserveRawText,
            (_, OnDecodeError::Replace | OnDecodeError::Fail) => PayloadKind::Text,
        };
        let line_limit = max_line_bytes.min(max_record_bytes);
        let mut oversize = None;
        match resume {
            FramingResume::Clean => {}
            FramingResume::Continuation {
                record_start_offset,
                next_fragment_index,
            } => {
                if runtime.framing.max_log_size_behavior != MaxLogSizeBehavior::Split {
                    return Err(FramerError::ContinuationRequiresSplit);
                }
                if next_fragment_index == 0 {
                    return Err(FramerError::ContinuationIndexZero);
                }
                if record_start_offset >= committed_offset {
                    return Err(FramerError::ContinuationRecordStart {
                        record_start_offset,
                        committed_offset,
                    });
                }
                oversize = Some(OversizeState::Split(SplitState {
                    record_start: record_start_offset,
                    index: next_fragment_index,
                    current: Payload::new(committed_offset, payload_kind.keeps_source_shadow()),
                    emit_current_nonfinal: false,
                }));
            }
        }
        let decoder = StreamDecoder::new(
            runtime.encoding,
            runtime.on_decode_error,
            committed_offset,
            new_stream_start,
        );

        Ok(Self {
            file_id,
            file_epoch,
            encoding: runtime.encoding,
            decode_policy: runtime.on_decode_error,
            payload_kind,
            mode,
            matcher,
            max_record_bytes,
            line_limit,
            max_multiline_lines: runtime.framing.max_multiline_lines,
            oversize_behavior: runtime.framing.max_log_size_behavior,
            force_flush_period: runtime.framing.force_flush_period,
            decoder,
            next_frame_start: committed_offset,
            line: Payload::new(committed_offset, payload_kind.keeps_source_shadow()),
            line_record_fit: None,
            record: None,
            complete_line: None,
            pending_unit: None,
            oversize,
            pattern_not_matched: 0,
            deadline: None,
        })
    }

    /// Alias emphasizing that construction consumes validated runtime state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_runtime(
        file_id: FileId,
        file_epoch: u32,
        runtime: &RuntimeConfig,
        committed_offset: u64,
        resume: FramingResume,
        new_stream_start: bool,
        now: Instant,
    ) -> Result<Self, FramerError> {
        Self::new(
            file_id,
            file_epoch,
            runtime,
            committed_offset,
            resume,
            new_stream_start,
            now,
        )
    }

    /// Consumes a caller-owned input prefix and returns at most one frame.
    ///
    /// The caller advances its slice by `consumed` and calls again. Empty
    /// input drains decoder replay, a retained complete line, or deferred
    /// split work. Input consumption can run ahead of the returned
    /// checkpoint because bounded start-pattern lookahead and decoder state
    /// remain owned by this framer.
    pub(crate) fn step(&mut self, input: &[u8], now: Instant) -> Result<FramerStep, FramerError> {
        if !input.is_empty() {
            self.deadline = None;
        }
        self.step_internal(input, now, true)
    }

    /// Observes source EOF and arms the idle period when pending state exists.
    pub(crate) fn observe_eof(&mut self, now: Instant) -> Result<(), FramerError> {
        if self.force_flush_period.is_zero() || !self.has_pending() {
            self.deadline = None;
        } else if self.deadline.is_none() {
            self.deadline = Some(
                now.checked_add(self.force_flush_period)
                    .ok_or(FramerError::DeadlineOverflow)?,
            );
        }
        Ok(())
    }

    /// Returns the next EOF-gated idle deadline, or `None` when unarmed.
    #[must_use]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        if self.has_pending() {
            self.deadline
        } else {
            None
        }
    }

    /// Polls deferred work and an armed EOF-gated idle timeout without input.
    pub(crate) fn poll_timeout(&mut self, now: Instant) -> Result<FlushStep, FramerError> {
        let step = self.step_internal(&[], now, true)?;
        Ok(FlushStep {
            output: step.output,
            pending: step.pending,
        })
    }

    /// Applies the rotation partial-flush hook.
    ///
    /// When partial flushing is disabled, recoverable state is reported as
    /// pending and is neither committed nor dropped.
    pub(crate) fn flush_rotation(&mut self, now: Instant) -> Result<FlushStep, FramerError> {
        self.flush_hook(now, FlushReason::Rotation)
    }

    /// Applies the receiver-drain partial-flush hook.
    ///
    /// When partial flushing is disabled, recoverable state is reported as
    /// pending and is neither committed nor dropped.
    pub(crate) fn flush_drain(&mut self, now: Instant) -> Result<FlushStep, FramerError> {
        self.flush_hook(now, FlushReason::Drain)
    }

    /// Returns the source offset expected for the next caller-owned byte.
    #[must_use]
    pub(crate) const fn next_expected_input_offset(&self) -> u64 {
        self.decoder.next_expected_input_offset()
    }

    /// Returns the earliest recoverable uncommitted source offset.
    #[must_use]
    pub(crate) fn pending_source_start(&self) -> Option<u64> {
        self.has_pending().then_some(self.next_frame_start)
    }

    /// Returns the bounded cumulative count of complete start-mode lines
    /// observed before a matching start line.
    ///
    /// The telemetry-oriented counter saturates at `u64::MAX`.
    #[must_use]
    pub(crate) const fn pattern_not_matched_count(&self) -> u64 {
        self.pattern_not_matched
    }

    /// Returns the conservative peak payload-allocation formula.
    pub(crate) fn peak_payload_capacity_bound(&self) -> Result<usize, FramerError> {
        let copies = if self.payload_kind.keeps_source_shadow() {
            2
        } else {
            1
        };
        peak_framer_payload_bytes(self.line_limit, self.max_record_bytes, copies).ok_or(
            FramerError::ArithmeticOverflow {
                context: "peak framer payload allocation bound",
            },
        )
    }

    fn step_internal(
        &mut self,
        input: &[u8],
        now: Instant,
        honor_timeout: bool,
    ) -> Result<FramerStep, FramerError> {
        let mut consumed = 0usize;
        loop {
            if let Some(output) = self.drive_retained()? {
                return Ok(self.step_result(consumed, Some(output)));
            }

            let timeout_due =
                honor_timeout && self.deadline.is_some_and(|deadline| now >= deadline);
            if timeout_due {
                let step = self
                    .decoder
                    .next(self.decoder.next_expected_input_offset(), &[])?;
                if let Some(event) = step.event {
                    if let Some(output) = self.process_decode_event(event)? {
                        return Ok(self.step_result(consumed, Some(output)));
                    }
                    continue;
                }
                if let Some(output) = self.flush_partial(FlushReason::Timeout)? {
                    if self.oversize.is_none() {
                        self.deadline = None;
                    }
                    return Ok(self.step_result(consumed, Some(output)));
                }
                if input.is_empty() {
                    self.deadline = None;
                    return Ok(self.step_result(consumed, None));
                }
            }

            let remaining = input.get(consumed..).ok_or(FramerError::Invariant {
                context: "decoder consumed beyond the caller-owned input",
            })?;
            let decode_step = self
                .decoder
                .next(self.decoder.next_expected_input_offset(), remaining)?;
            if decode_step.consumed != 0 {
                consumed = consumed.checked_add(decode_step.consumed).ok_or(
                    FramerError::ArithmeticOverflow {
                        context: "step consumed byte count",
                    },
                )?;
                self.deadline = None;
            }
            if let Some(event) = decode_step.event {
                if let Some(output) = self.process_decode_event(event)? {
                    return Ok(self.step_result(consumed, Some(output)));
                }
                continue;
            }

            if decode_step.consumed == 0 {
                if remaining.is_empty() {
                    return Ok(self.step_result(consumed, None));
                }
                return Err(FramerError::Invariant {
                    context: "decoder made no progress on nonempty input",
                });
            }
        }
    }

    fn flush_hook(&mut self, now: Instant, reason: FlushReason) -> Result<FlushStep, FramerError> {
        let step = self.step_internal(&[], now, false)?;
        if step.output.is_some() {
            return Ok(FlushStep {
                output: step.output,
                pending: step.pending,
            });
        }
        if self.force_flush_period.is_zero() {
            return Ok(FlushStep {
                output: None,
                pending: self.has_pending(),
            });
        }
        let output = self.flush_partial(reason)?;
        if output.is_some() {
            self.deadline = None;
        }
        Ok(FlushStep {
            output,
            pending: self.has_pending(),
        })
    }

    fn step_result(&self, consumed: usize, output: Option<FramedRecord>) -> FramerStep {
        FramerStep {
            consumed,
            output,
            pending: self.has_pending(),
        }
    }

    fn drive_retained(&mut self) -> Result<Option<FramedRecord>, FramerError> {
        if self.mode == FramingMode::StartPattern
            && self.line.is_empty()
            && self.record.as_ref().is_some_and(|record| {
                record.payload.measure(self.payload_kind) > self.max_record_bytes
            })
        {
            let record = self.record.take().ok_or(FramerError::Invariant {
                context: "exact-bound start record disappeared before lookahead",
            })?;
            return self.finish_buffered_record(record, None);
        }
        if let Some(OversizeState::Split(state)) = self.oversize.as_ref()
            && state.emit_current_nonfinal
        {
            return self.emit_current_split_nonfinal();
        }
        if let Some(unit) = self.pending_unit.take() {
            return self.process_unit(unit);
        }
        if let Some(line) = self.complete_line.take() {
            return self.handle_complete_line(line);
        }
        Ok(None)
    }

    fn process_decode_event(
        &mut self,
        event: DecodeEvent,
    ) -> Result<Option<FramedRecord>, FramerError> {
        if let DecodeEvent::StrippedBom { range } = event {
            if self.line.is_empty()
                && self.record.is_none()
                && self.oversize.is_none()
                && self.line.range.end == range.start
            {
                self.line.range = SourceRange {
                    start: range.end,
                    end: range.end,
                };
                return Ok(None);
            }
            return Err(FramerError::Invariant {
                context: "stripped BOM arrived after framing state began",
            });
        }
        let unit = Unit::from_event(event)?.ok_or(FramerError::Invariant {
            context: "unit conversion discarded a unit event",
        })?;
        self.process_unit(unit)
    }

    fn process_unit(&mut self, unit: Unit) -> Result<Option<FramedRecord>, FramerError> {
        if self.oversize.is_some() {
            return self.process_oversize_unit(unit);
        }
        if unit.is_lf() {
            if self.line.range.end != unit.range.start {
                return Err(FramerError::SourceDiscontinuity {
                    expected: self.line.range.end,
                    actual: unit.range.start,
                });
            }
            let next_line = Payload::new(unit.range.end, self.payload_kind.keeps_source_shadow());
            let content = std::mem::replace(&mut self.line, next_line);
            let record_fit = self.line_record_fit.take();
            return self.handle_complete_line(CompleteLine {
                content,
                delimiter: unit,
                record_fit,
            });
        }
        let prospective = self.line.prospective_measure(unit, self.payload_kind)?;
        if prospective > self.line_limit {
            return self.begin_physical_oversize(unit);
        }
        self.update_line_record_fit(unit)?;
        self.line.append_unit(unit, self.payload_kind)?;
        Ok(None)
    }

    fn update_line_record_fit(&mut self, unit: Unit) -> Result<(), FramerError> {
        let Some(record) = &self.record else {
            return Ok(());
        };
        let decoded_len = record
            .payload
            .decoded
            .len()
            .checked_add(self.line.decoded.len())
            .and_then(|value| value.checked_add(usize::from(unit.decoded_len)))
            .ok_or(FramerError::ArithmeticOverflow {
                context: "logical prefix decoded length",
            })?;
        let source_len = record
            .payload
            .source_len
            .checked_add(self.line.source_len)
            .and_then(|value| value.checked_add(unit.source_len()))
            .ok_or(FramerError::ArithmeticOverflow {
                context: "logical prefix source length",
            })?;
        if self.payload_kind.measure(decoded_len, source_len) <= self.max_record_bytes {
            let line_decoded_len = self
                .line
                .decoded
                .len()
                .checked_add(usize::from(unit.decoded_len))
                .ok_or(FramerError::ArithmeticOverflow {
                    context: "fitting line-prefix decoded length",
                })?;
            let line_source_len = self.line.source_len.checked_add(unit.source_len()).ok_or(
                FramerError::ArithmeticOverflow {
                    context: "fitting line-prefix source length",
                },
            )?;
            let malformed_count = self
                .line
                .malformed_count
                .checked_add(u64::from(unit.malformed))
                .ok_or(FramerError::ArithmeticOverflow {
                    context: "fitting line-prefix malformed count",
                })?;
            self.line_record_fit = Some(LineFit {
                decoded_len: line_decoded_len,
                source_len: line_source_len,
                malformed_count,
                end: unit.range.end,
            });
        }
        Ok(())
    }

    fn begin_physical_oversize(
        &mut self,
        overflow_unit: Unit,
    ) -> Result<Option<FramedRecord>, FramerError> {
        let line_end = self.line.range.end;
        if line_end != overflow_unit.range.start {
            return Err(FramerError::SourceDiscontinuity {
                expected: line_end,
                actual: overflow_unit.range.start,
            });
        }
        let prefix = std::mem::replace(
            &mut self.line,
            Payload::new(line_end, self.payload_kind.keeps_source_shadow()),
        );
        self.line_record_fit = None;
        if prefix.is_empty() {
            return Err(FramerError::Invariant {
                context: "one decoded unit exceeds a validated physical-line limit",
            });
        }
        self.pending_unit = Some(overflow_unit);
        self.oversize = Some(match self.oversize_behavior {
            MaxLogSizeBehavior::Split => {
                let record_start = prefix.range.start;
                OversizeState::Split(SplitState {
                    record_start,
                    index: 0,
                    current: prefix,
                    emit_current_nonfinal: true,
                })
            }
            MaxLogSizeBehavior::Truncate => OversizeState::Truncate(TruncateState {
                prefix,
                discarded_source_bytes: 0,
                discarded_malformed_count: 0,
            }),
        });

        if let Some(record) = self.record.take() {
            return self.finish_buffered_record(record, Some(FlushReason::OversizeLineBoundary));
        }
        self.drive_retained()
    }

    fn process_oversize_unit(&mut self, unit: Unit) -> Result<Option<FramedRecord>, FramerError> {
        if unit.is_lf() {
            return self.finish_oversize(unit);
        }
        match self.oversize.as_mut().ok_or(FramerError::Invariant {
            context: "oversize unit processing requires oversize state",
        })? {
            OversizeState::Split(state) => {
                let prospective = state.current.prospective_measure(unit, self.payload_kind)?;
                if prospective > self.line_limit {
                    if state.current.is_empty() {
                        return Err(FramerError::Invariant {
                            context: "one decoded unit exceeds a validated split limit",
                        });
                    }
                    state.emit_current_nonfinal = true;
                    self.pending_unit = Some(unit);
                    return self.emit_current_split_nonfinal();
                }
                state.current.append_unit(unit, self.payload_kind)?;
                Ok(None)
            }
            OversizeState::Truncate(state) => {
                state.discarded_source_bytes = state
                    .discarded_source_bytes
                    .checked_add(u64::try_from(unit.source_len()).map_err(|_| {
                        FramerError::ArithmeticOverflow {
                            context: "discarded source unit length",
                        }
                    })?)
                    .ok_or(FramerError::ArithmeticOverflow {
                        context: "discarded source byte count",
                    })?;
                if unit.malformed {
                    state.discarded_malformed_count = state
                        .discarded_malformed_count
                        .checked_add(1)
                        .ok_or(FramerError::ArithmeticOverflow {
                            context: "discarded malformed unit count",
                        })?;
                }
                Ok(None)
            }
        }
    }

    fn finish_oversize(&mut self, delimiter: Unit) -> Result<Option<FramedRecord>, FramerError> {
        let state = self.oversize.take().ok_or(FramerError::Invariant {
            context: "oversize terminator requires oversize state",
        })?;
        self.line = Payload::new(delimiter.range.end, self.payload_kind.keeps_source_shadow());
        self.line_record_fit = None;
        self.record = None;
        match state {
            OversizeState::Split(state) => self
                .make_fragment(
                    state.current,
                    delimiter.range.end,
                    state.record_start,
                    state.index,
                    true,
                    None,
                    None,
                )
                .map(Some),
            OversizeState::Truncate(state) => self
                .make_record(
                    state.prefix,
                    delimiter.range.end,
                    FramingResume::Clean,
                    None,
                    true,
                    state.discarded_source_bytes,
                    state.discarded_malformed_count,
                    false,
                    None,
                )
                .map(Some),
        }
    }

    fn emit_current_split_nonfinal(&mut self) -> Result<Option<FramedRecord>, FramerError> {
        let (payload, record_start, index, next_index) = {
            let state = match self.oversize.as_mut() {
                Some(OversizeState::Split(state)) => state,
                _ => {
                    return Err(FramerError::Invariant {
                        context: "nonfinal fragment requires split state",
                    });
                }
            };
            if !state.emit_current_nonfinal {
                return Ok(None);
            }
            if state.index == u32::MAX {
                return Err(FramerError::FragmentIndexOverflow);
            }
            let next_index = state
                .index
                .checked_add(1)
                .ok_or(FramerError::FragmentIndexOverflow)?;
            let end = state.current.range.end;
            let payload = std::mem::replace(
                &mut state.current,
                Payload::new(end, self.payload_kind.keeps_source_shadow()),
            );
            let index = state.index;
            state.index = next_index;
            state.emit_current_nonfinal = false;
            (payload, state.record_start, index, next_index)
        };
        let frame_end = payload.range.end;
        self.make_fragment(
            payload,
            frame_end,
            record_start,
            index,
            false,
            Some(next_index),
            None,
        )
        .map(Some)
    }

    fn handle_complete_line(
        &mut self,
        line: CompleteLine,
    ) -> Result<Option<FramedRecord>, FramerError> {
        let matches = match self.mode {
            FramingMode::Newline => false,
            FramingMode::StartPattern | FramingMode::EndPattern => self
                .matcher
                .as_ref()
                .ok_or(FramerError::InvalidRuntimeMatcher)?
                .is_match(&line.content.decoded)
                .map_err(|source| FramerError::MatcherUtf8 { source })?,
        };
        match self.mode {
            FramingMode::Newline => self.finish_line(None, line, None),
            FramingMode::StartPattern => self.handle_start_line(line, matches),
            FramingMode::EndPattern => self.handle_end_line(line, matches),
        }
    }

    fn handle_start_line(
        &mut self,
        line: CompleteLine,
        matches: bool,
    ) -> Result<Option<FramedRecord>, FramerError> {
        if self.record.is_none() {
            if !matches {
                self.pattern_not_matched = self.pattern_not_matched.saturating_add(1);
                return self.finish_line(None, line, None);
            }
            let line_count = 1;
            if line_count >= self.max_multiline_lines {
                return self.finish_line(None, line, Some(FlushReason::MaxLines));
            }
            return self.buffer_line(None, line, line_count);
        }

        if matches {
            self.complete_line = Some(line);
            let record = self.record.take().ok_or(FramerError::Invariant {
                context: "start-pattern match lost its prior record",
            })?;
            return self.finish_buffered_record(record, None);
        }

        let record = self.record.take().ok_or(FramerError::Invariant {
            context: "start-pattern buffering lost its record",
        })?;
        if record.payload.measure(self.payload_kind) > self.max_record_bytes {
            self.complete_line = Some(line);
            return self.finish_buffered_record(record, None);
        }
        let line_count =
            record
                .line_count
                .checked_add(1)
                .ok_or(FramerError::ArithmeticOverflow {
                    context: "multiline physical-line count",
                })?;
        let body_measure = record
            .payload
            .combined_measure(&line.content, self.payload_kind)?;
        if body_measure > self.max_record_bytes {
            return self.handle_logical_overflow(Some(record), line);
        }
        if line_count >= self.max_multiline_lines {
            return self.finish_line(Some(record), line, Some(FlushReason::MaxLines));
        }
        self.buffer_line(Some(record), line, line_count)
    }

    fn handle_end_line(
        &mut self,
        line: CompleteLine,
        matches: bool,
    ) -> Result<Option<FramedRecord>, FramerError> {
        let record = self.record.take();
        let line_count = record.as_ref().map_or(Ok(1), |record| {
            record
                .line_count
                .checked_add(1)
                .ok_or(FramerError::ArithmeticOverflow {
                    context: "multiline physical-line count",
                })
        })?;
        let body_measure = match &record {
            Some(record) => record
                .payload
                .combined_measure(&line.content, self.payload_kind)?,
            None => line.content.measure(self.payload_kind),
        };
        if body_measure > self.max_record_bytes {
            return self.handle_logical_overflow(record, line);
        }
        if matches {
            return self.finish_line(record, line, None);
        }
        if line_count >= self.max_multiline_lines {
            return self.finish_line(record, line, Some(FlushReason::MaxLines));
        }
        self.buffer_line(record, line, line_count)
    }

    fn buffer_line(
        &mut self,
        record: Option<BufferedRecord>,
        line: CompleteLine,
        line_count: u32,
    ) -> Result<Option<FramedRecord>, FramerError> {
        let prior_decoded_len = record
            .as_ref()
            .map_or(0, |record| record.payload.decoded.len());
        let prior_source_len = record
            .as_ref()
            .map_or(0, |record| record.payload.source_len);
        let decoded_len = prior_decoded_len
            .checked_add(line.content.decoded.len())
            .and_then(|value| value.checked_add(usize::from(line.delimiter.decoded_len)))
            .ok_or(FramerError::ArithmeticOverflow {
                context: "buffered multiline decoded length",
            })?;
        let source_len = prior_source_len
            .checked_add(line.content.source_len)
            .and_then(|value| value.checked_add(line.delimiter.source_len()))
            .ok_or(FramerError::ArithmeticOverflow {
                context: "buffered multiline source length",
            })?;
        let body_measure = self.payload_kind.measure(
            prior_decoded_len
                .checked_add(line.content.decoded.len())
                .ok_or(FramerError::ArithmeticOverflow {
                    context: "buffered multiline body decoded length",
                })?,
            prior_source_len
                .checked_add(line.content.source_len)
                .ok_or(FramerError::ArithmeticOverflow {
                    context: "buffered multiline body source length",
                })?,
        );
        let with_delimiter = self.payload_kind.measure(decoded_len, source_len);
        if body_measure > self.max_record_bytes {
            return self.handle_logical_overflow(record, line);
        }
        if with_delimiter > self.max_record_bytes && self.mode != FramingMode::StartPattern {
            return self.finish_line(record, line, None);
        }
        let mut payload = match record {
            Some(record) => record.payload,
            None => Payload::new(
                line.content.range.start,
                self.payload_kind.keeps_source_shadow(),
            ),
        };
        payload.append_payload(line.content, self.payload_kind)?;
        payload.append_unit(line.delimiter, self.payload_kind)?;
        self.record = Some(BufferedRecord {
            payload,
            line_count,
            terminal_delimiter: line.delimiter,
        });
        Ok(None)
    }

    fn finish_line(
        &mut self,
        record: Option<BufferedRecord>,
        line: CompleteLine,
        reason: Option<FlushReason>,
    ) -> Result<Option<FramedRecord>, FramerError> {
        let mut payload = match record {
            Some(record) => record.payload,
            None => Payload::new(
                line.content.range.start,
                self.payload_kind.keeps_source_shadow(),
            ),
        };
        let body_measure = payload.combined_measure(&line.content, self.payload_kind)?;
        if body_measure > self.max_record_bytes {
            let prior = if payload.is_empty() {
                None
            } else {
                Some(BufferedRecord {
                    payload,
                    line_count: 0,
                    terminal_delimiter: line.delimiter,
                })
            };
            return self.handle_logical_overflow(prior, line);
        }
        payload.append_payload(line.content, self.payload_kind)?;
        self.make_record(
            payload,
            line.delimiter.range.end,
            FramingResume::Clean,
            reason,
            false,
            0,
            0,
            false,
            None,
        )
        .map(Some)
    }

    fn finish_buffered_record(
        &mut self,
        mut record: BufferedRecord,
        reason: Option<FlushReason>,
    ) -> Result<Option<FramedRecord>, FramerError> {
        let frame_end = record.terminal_delimiter.range.end;
        record
            .payload
            .remove_suffix(record.terminal_delimiter, self.payload_kind)?;
        self.make_record(
            record.payload,
            frame_end,
            FramingResume::Clean,
            reason,
            false,
            0,
            0,
            false,
            None,
        )
        .map(Some)
    }

    fn handle_logical_overflow(
        &mut self,
        record: Option<BufferedRecord>,
        line: CompleteLine,
    ) -> Result<Option<FramedRecord>, FramerError> {
        let CompleteLine {
            content,
            delimiter,
            record_fit,
        } = line;
        match self.oversize_behavior {
            MaxLogSizeBehavior::Split => {
                if let Some(record) = record {
                    let record_start = record.payload.range.start;
                    let prefix = record.payload;
                    let trigger_start = content.range.start;
                    self.pending_unit = Some(delimiter);
                    self.oversize = Some(OversizeState::Split(SplitState {
                        record_start,
                        index: 1,
                        current: content,
                        emit_current_nonfinal: false,
                    }));
                    self.make_fragment(prefix, trigger_start, record_start, 0, false, Some(1), None)
                        .map(Some)
                } else {
                    let record_start = content.range.start;
                    self.make_fragment(
                        content,
                        delimiter.range.end,
                        record_start,
                        0,
                        true,
                        None,
                        None,
                    )
                    .map(Some)
                }
            }
            MaxLogSizeBehavior::Truncate => {
                let (prefix, discarded_source_bytes, discarded_malformed_count) =
                    if let Some(record) = record {
                        self.logical_truncate_prefix(record.payload, content, record_fit)?
                    } else {
                        (content, 0, 0)
                    };
                self.make_record(
                    prefix,
                    delimiter.range.end,
                    FramingResume::Clean,
                    None,
                    true,
                    discarded_source_bytes,
                    discarded_malformed_count,
                    false,
                    None,
                )
                .map(Some)
            }
        }
    }

    fn logical_truncate_prefix(
        &self,
        mut prefix: Payload,
        mut trigger: Payload,
        fit: Option<LineFit>,
    ) -> Result<(Payload, u64, u64), FramerError> {
        let total_source_len = trigger.source_len;
        let total_malformed_count = trigger.malformed_count;
        let fit = fit.unwrap_or(LineFit {
            decoded_len: 0,
            source_len: 0,
            malformed_count: 0,
            end: trigger.range.start,
        });
        trigger.retain_prefix(fit, self.payload_kind)?;
        let retained_source_len = trigger.source_len;
        let retained_malformed_count = trigger.malformed_count;
        prefix.append_payload(trigger, self.payload_kind)?;
        let discarded_source_bytes = total_source_len
            .checked_sub(retained_source_len)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(FramerError::ArithmeticOverflow {
                context: "logical truncation discarded bytes",
            })?;
        let discarded_malformed_count = total_malformed_count
            .checked_sub(retained_malformed_count)
            .ok_or(FramerError::Invariant {
                context: "logical truncation retained malformed count exceeds trigger line",
            })?;
        Ok((prefix, discarded_source_bytes, discarded_malformed_count))
    }

    fn flush_partial(&mut self, reason: FlushReason) -> Result<Option<FramedRecord>, FramerError> {
        let delivered = self.decoder.highest_delivered_source_boundary();
        if matches!(
            self.oversize.as_ref(),
            Some(OversizeState::Split(state)) if state.current.is_empty()
        ) {
            return Ok(None);
        }
        if let Some(state) = self.oversize.take() {
            self.line = Payload::new(delivered, self.payload_kind.keeps_source_shadow());
            self.line_record_fit = None;
            return match state {
                OversizeState::Split(state) => self
                    .make_fragment(
                        state.current,
                        self.decoder.highest_delivered_source_boundary(),
                        state.record_start,
                        state.index,
                        true,
                        None,
                        Some(reason),
                    )
                    .map(Some),
                OversizeState::Truncate(state) => self
                    .make_record(
                        state.prefix,
                        self.decoder.highest_delivered_source_boundary(),
                        FramingResume::Clean,
                        Some(reason),
                        true,
                        state.discarded_source_bytes,
                        state.discarded_malformed_count,
                        false,
                        None,
                    )
                    .map(Some),
            };
        }

        let line = std::mem::replace(
            &mut self.line,
            Payload::new(delivered, self.payload_kind.keeps_source_shadow()),
        );
        let line_record_fit = self.line_record_fit.take();
        match (self.record.take(), line.is_empty()) {
            (None, true) => Ok(None),
            (None, false) => self
                .make_record(
                    line,
                    delivered,
                    FramingResume::Clean,
                    Some(reason),
                    false,
                    0,
                    0,
                    false,
                    None,
                )
                .map(Some),
            (Some(record), true) => self.finish_buffered_record(record, Some(reason)),
            (Some(record), false) => {
                let combined = record.payload.combined_measure(&line, self.payload_kind)?;
                if combined > self.max_record_bytes {
                    return self.flush_logical_overflow(
                        record,
                        line,
                        line_record_fit,
                        delivered,
                        reason,
                    );
                }
                let mut payload = record.payload;
                payload.append_payload(line, self.payload_kind)?;
                self.make_record(
                    payload,
                    delivered,
                    FramingResume::Clean,
                    Some(reason),
                    false,
                    0,
                    0,
                    false,
                    None,
                )
                .map(Some)
            }
        }
    }

    fn flush_logical_overflow(
        &mut self,
        record: BufferedRecord,
        line: Payload,
        line_record_fit: Option<LineFit>,
        delivered: u64,
        reason: FlushReason,
    ) -> Result<Option<FramedRecord>, FramerError> {
        match self.oversize_behavior {
            MaxLogSizeBehavior::Split => {
                let record_start = record.payload.range.start;
                self.oversize = Some(OversizeState::Split(SplitState {
                    record_start,
                    index: 1,
                    current: line,
                    emit_current_nonfinal: false,
                }));
                self.make_fragment(
                    record.payload,
                    record.terminal_delimiter.range.end,
                    record_start,
                    0,
                    false,
                    Some(1),
                    None,
                )
                .map(Some)
            }
            MaxLogSizeBehavior::Truncate => {
                let (prefix, discarded_source_bytes, discarded_malformed_count) =
                    self.logical_truncate_prefix(record.payload, line, line_record_fit)?;
                self.make_record(
                    prefix,
                    delivered,
                    FramingResume::Clean,
                    Some(reason),
                    true,
                    discarded_source_bytes,
                    discarded_malformed_count,
                    false,
                    None,
                )
                .map(Some)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_fragment(
        &mut self,
        payload: Payload,
        frame_end: u64,
        record_start: u64,
        index: u32,
        last: bool,
        expected_next_index: Option<u32>,
        flush_reason: Option<FlushReason>,
    ) -> Result<FramedRecord, FramerError> {
        if !last && index == u32::MAX {
            return Err(FramerError::FragmentIndexOverflow);
        }
        let resulting_resume = if last {
            FramingResume::Clean
        } else {
            let next_fragment_index = expected_next_index
                .or_else(|| index.checked_add(1))
                .ok_or(FramerError::FragmentIndexOverflow)?;
            FramingResume::Continuation {
                record_start_offset: record_start,
                next_fragment_index,
            }
        };
        let fragment = FragmentMetadata {
            id: fragment_id(self.file_id, self.file_epoch, record_start),
            index,
            last,
        };
        self.make_record(
            payload,
            frame_end,
            resulting_resume,
            flush_reason,
            false,
            0,
            0,
            true,
            Some(fragment),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn make_record(
        &mut self,
        payload: Payload,
        frame_end: u64,
        resulting_resume: FramingResume,
        flush_reason: Option<FlushReason>,
        truncated: bool,
        discarded_source_bytes: u64,
        extra_malformed_count: u64,
        force_preserve_bytes: bool,
        fragment: Option<FragmentMetadata>,
    ) -> Result<FramedRecord, FramerError> {
        if frame_end < payload.range.end {
            return Err(FramerError::Invariant {
                context: "checkpoint boundary precedes body source range",
            });
        }
        if self.next_frame_start > payload.range.start {
            return Err(FramerError::Invariant {
                context: "body source range precedes owned progress",
            });
        }
        let retained_malformed_count = payload.malformed_count;
        let malformed_count = retained_malformed_count
            .checked_add(extra_malformed_count)
            .ok_or(FramerError::ArithmeticOverflow {
                context: "output malformed unit count",
            })?;
        let decode_outcome = match (self.decode_policy, malformed_count) {
            (_, 0) => DecodeOutcome::Clean,
            (OnDecodeError::Replace, count) => DecodeOutcome::Replacements { count },
            (OnDecodeError::PreserveRaw, count) => DecodeOutcome::PreserveRaw { count },
            (OnDecodeError::Fail, _) => {
                return Err(FramerError::Invariant {
                    context: "fail decode policy retained a malformed unit",
                });
            }
        };
        let body = match self.encoding {
            Encoding::Raw => FramedBody::Bytes(payload.decoded),
            Encoding::Utf8 | Encoding::Ascii | Encoding::Utf16Le | Encoding::Utf16Be
                if self.decode_policy == OnDecodeError::PreserveRaw
                    && (force_preserve_bytes || retained_malformed_count != 0) =>
            {
                if payload.source.len() != payload.source_len {
                    return Err(FramerError::Invariant {
                        context: "preserve-raw source shadow is incomplete",
                    });
                }
                FramedBody::Bytes(payload.source)
            }
            Encoding::Utf8 | Encoding::Ascii | Encoding::Utf16Le | Encoding::Utf16Be => {
                FramedBody::Text(String::from_utf8(payload.decoded).map_err(|_| {
                    FramerError::Invariant {
                        context: "decoded text payload is not UTF-8",
                    }
                })?)
            }
        };
        let emitted_body_len = match &body {
            FramedBody::Text(body) => body.len(),
            FramedBody::Bytes(body) => body.len(),
        };
        if emitted_body_len > self.max_record_bytes {
            return Err(FramerError::Invariant {
                context: "emitted body exceeds max_record_bytes",
            });
        }
        let frame_source_range = SourceRange {
            start: self.next_frame_start,
            end: frame_end,
        };
        self.next_frame_start = frame_end;
        self.record = None;
        Ok(FramedRecord {
            body,
            body_source_range: payload.range,
            frame_source_range,
            checkpoint_end: frame_end,
            resulting_resume,
            decode_outcome,
            flush_reason,
            fragment,
            truncated,
            discarded_source_bytes,
        })
    }

    fn has_pending(&self) -> bool {
        self.record.is_some()
            || !self.line.is_empty()
            || self.complete_line.is_some()
            || self.pending_unit.is_some()
            || self.oversize.is_some()
            || self.next_frame_start < self.decoder.highest_delivered_source_boundary()
            || self.decoder.earliest_uncommittable_offset().is_some()
    }

    #[cfg(test)]
    fn retained_payload_capacity(&self) -> usize {
        fn payload_capacity(payload: &Payload) -> usize {
            payload
                .decoded
                .capacity()
                .saturating_add(payload.source.capacity())
        }
        let mut total = payload_capacity(&self.line);
        if let Some(record) = &self.record {
            total = total.saturating_add(payload_capacity(&record.payload));
        }
        if let Some(line) = &self.complete_line {
            total = total.saturating_add(payload_capacity(&line.content));
        }
        if let Some(state) = &self.oversize {
            total = total.saturating_add(match state {
                OversizeState::Split(state) => payload_capacity(&state.current),
                OversizeState::Truncate(state) => payload_capacity(&state.prefix),
            });
        }
        total
    }
}

fn fragment_id(file_id: FileId, file_epoch: u32, record_start: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FRAGMENT_ID_DOMAIN);
    hasher.update(file_id.0);
    hasher.update(file_epoch.to_be_bytes());
    hasher.update(record_start.to_be_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
#[path = "framer/tests.rs"]
mod tests;
