// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::time::{Duration, Instant};

use super::{
    CommittedFrontierWindow, DecodeOutcome, FlushReason, FramedBody, FramedRecord, Framer,
    FramerError, fragment_id,
};
use crate::receivers::filelog_receiver::{
    Config, Encoding, MaxLogSizeBehavior, OnDecodeError,
    checkpoint::{CommittedFrontierGuard, FileId, FramingResume},
    config::RuntimeConfig,
};

const FILE_ID: FileId = FileId::from_bytes([
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
]);

/// Test-only zero-filled committed-frontier seed window: a deterministic,
/// obviously-fake window for tests that only exercise framing behavior and
/// do not exercise real continuity evidence.
fn zero_window(end_offset: u64) -> CommittedFrontierWindow {
    let window_len = end_offset.min(64) as usize;
    CommittedFrontierWindow::new(end_offset, vec![0u8; window_len]).unwrap()
}

#[derive(Clone)]
struct TestSettings {
    encoding: Encoding,
    policy: OnDecodeError,
    behavior: MaxLogSizeBehavior,
    max_line: u64,
    max_record: u64,
    start_pattern: Option<&'static str>,
    end_pattern: Option<&'static str>,
    max_lines: u32,
    flush_period: Duration,
}

impl Default for TestSettings {
    fn default() -> Self {
        Self {
            encoding: Encoding::Utf8,
            policy: OnDecodeError::PreserveRaw,
            behavior: MaxLogSizeBehavior::Split,
            max_line: 64,
            max_record: 64,
            start_pattern: None,
            end_pattern: None,
            max_lines: 32,
            flush_period: Duration::from_millis(10),
        }
    }
}

fn runtime(settings: &TestSettings) -> RuntimeConfig {
    let mut config = Config {
        include: vec!["/var/log/filelog-framer-test/*.log".to_owned()],
        encoding: settings.encoding,
        on_decode_error: settings.policy,
        ..Config::default()
    };
    config.framing.max_line_bytes = settings.max_line;
    config.framing.max_record_bytes = settings.max_record;
    config.framing.max_log_size_behavior = settings.behavior;
    config.framing.max_multiline_lines = settings.max_lines;
    config.framing.force_flush_period = settings.flush_period;
    config.framing.multiline.line_start_pattern = settings.start_pattern.map(ToOwned::to_owned);
    config.framing.multiline.line_end_pattern = settings.end_pattern.map(ToOwned::to_owned);
    RuntimeConfig::from_config(config, "framer-test").expect("test settings must validate")
}

fn framer(settings: &TestSettings, now: Instant) -> Framer {
    Framer::new(
        FILE_ID,
        7,
        &runtime(settings),
        0,
        FramingResume::Clean,
        true,
        zero_window(0),
        now,
    )
    .expect("test framer must construct")
}

fn resumed_framer(
    settings: &TestSettings,
    committed: u64,
    record_start: u64,
    index: u32,
    now: Instant,
) -> Result<Framer, FramerError> {
    resumed_framer_with_end(settings, committed, record_start, 0, index, now)
}

fn resumed_framer_with_end(
    settings: &TestSettings,
    committed: u64,
    record_start: u64,
    record_end: u64,
    index: u32,
    now: Instant,
) -> Result<Framer, FramerError> {
    Framer::from_runtime(
        FILE_ID,
        7,
        &runtime(settings),
        committed,
        FramingResume::Continuation {
            record_start_offset: record_start,
            record_end_offset: record_end,
            next_fragment_index: index,
        },
        false,
        zero_window(committed),
        now,
    )
}

fn feed(framer: &mut Framer, input: &[u8], now: Instant) -> Vec<FramedRecord> {
    let mut outputs = Vec::new();
    let mut cursor = 0;
    while cursor < input.len() {
        let step = framer
            .step(&input[cursor..], now)
            .expect("test input must frame");
        assert!(
            step.consumed != 0 || step.output.is_some(),
            "nonempty input must make progress"
        );
        cursor += step.consumed;
        if let Some(output) = step.output {
            outputs.push(output);
        }
    }
    loop {
        let step = framer.step(&[], now).expect("zero-input drain must frame");
        if let Some(output) = step.output {
            outputs.push(output);
        } else {
            break;
        }
    }
    outputs
}

fn feed_chunks(framer: &mut Framer, chunks: &[&[u8]], now: Instant) -> Vec<FramedRecord> {
    let mut outputs = Vec::new();
    for chunk in chunks {
        outputs.extend(feed(framer, chunk, now));
    }
    outputs
}

fn text(record: &FramedRecord) -> &str {
    match &record.body {
        FramedBody::Text(body) => body,
        FramedBody::Bytes(body) => panic!("expected text body, got {body:?}"),
    }
}

fn bytes(record: &FramedRecord) -> &[u8] {
    match &record.body {
        FramedBody::Bytes(body) => body,
        FramedBody::Text(body) => panic!("expected bytes body, got {body:?}"),
    }
}

/// Scenario: Newline framing receives LF, CRLF, an empty line, an embedded CR, and NUL.
/// Guarantees: Only final LF is syntax; CR, NUL, empty bodies, ranges, and checkpoints are preserved.
#[test]
fn newline_framing_preserves_cr_empty_and_nul_data() {
    let now = Instant::now();
    let mut framer = framer(&TestSettings::default(), now);
    let outputs = feed(&mut framer, b"a\nb\r\n\nc\rd\0\n", now);

    assert_eq!(
        outputs.iter().map(text).collect::<Vec<_>>(),
        vec!["a", "b\r", "", "c\rd\0"]
    );
    assert_eq!(
        outputs[0].body_source_range,
        super::SourceRange { start: 0, end: 1 }
    );
    assert_eq!(
        outputs[0].frame_source_range,
        super::SourceRange { start: 0, end: 2 }
    );
    assert_eq!(
        outputs[2].body_source_range,
        super::SourceRange { start: 5, end: 5 }
    );
    assert_eq!(outputs[2].checkpoint_end, 6);
    assert_eq!(outputs[3].owned_progress_range().end, 11);
}

/// Scenario: UTF-16LE starts with a matching BOM and contains a decoded LF delimiter.
/// Guarantees: The body and record start exclude the BOM, while source offsets and first progress cross it.
#[test]
fn utf16_bom_and_delimiter_keep_source_offsets() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Utf16Le,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, &[0xff, 0xfe, b'A', 0, b'\n', 0], now);

    assert_eq!(outputs.len(), 1);
    assert_eq!(text(&outputs[0]), "A");
    assert_eq!(
        outputs[0].body_source_range,
        super::SourceRange { start: 2, end: 4 }
    );
    assert_eq!(
        outputs[0].frame_source_range,
        super::SourceRange { start: 0, end: 6 }
    );
    assert_eq!(outputs[0].checkpoint_end, 6);
}

/// Scenario: Raw mode receives BOM-looking bytes and a NUL before LF.
/// Guarantees: Raw framing strips nothing and emits every nonterminal source byte exactly.
#[test]
fn raw_mode_never_strips_bom_or_nul() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 16,
        max_record: 16,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, &[0xef, 0xbb, 0xbf, 0, b'\n'], now);

    assert_eq!(outputs.len(), 1);
    assert_eq!(bytes(&outputs[0]), &[0xef, 0xbb, 0xbf, 0]);
    assert_eq!(
        outputs[0].body_source_range,
        super::SourceRange { start: 0, end: 4 }
    );
}

/// Scenario: One malformed UTF-8 unit is framed under preserve_raw, replace, and fail.
/// Guarantees: Exact evidence, replacement counts, and structured fatal errors follow policy.
#[test]
fn every_decode_policy_is_observable() {
    let now = Instant::now();
    let source = [b'a', 0xff, b'b', b'\n'];

    let mut preserve = framer(&TestSettings::default(), now);
    let output = feed(&mut preserve, &source, now).remove(0);
    assert_eq!(bytes(&output), &source[..3]);
    assert_eq!(
        output.decode_outcome,
        DecodeOutcome::PreserveRaw { count: 1 }
    );

    let replace_settings = TestSettings {
        policy: OnDecodeError::Replace,
        ..TestSettings::default()
    };
    let mut replace = framer(&replace_settings, now);
    let output = feed(&mut replace, &source, now).remove(0);
    assert_eq!(text(&output), "a\u{fffd}b");
    assert_eq!(
        output.decode_outcome,
        DecodeOutcome::Replacements { count: 1 }
    );

    let fail_settings = TestSettings {
        policy: OnDecodeError::Fail,
        ..TestSettings::default()
    };
    let mut fail = framer(&fail_settings, now);
    let error = fail.step(&source, now).expect_err("fail policy must stop");
    assert!(matches!(
        error,
        FramerError::Decode(super::DecodeError::FatalMalformed {
            range: super::SourceRange { start: 1, end: 2 },
            ..
        })
    ));
}

/// Scenario: A raw end-pattern matches an invalid UTF-8 byte in the complete physical line.
/// Guarantees: Regex matching uses exact raw bytes and internal LF separators remain in the bytes body.
#[test]
fn raw_pattern_matches_invalid_utf8_without_lossy_conversion() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        end_pattern: Some(r"^\xFF$"),
        max_line: 16,
        max_record: 32,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"head\n\xff\n", now);

    assert_eq!(outputs.len(), 1);
    assert_eq!(bytes(&outputs[0]), b"head\n\xff");
}

/// Scenario: A text multiline matcher is given bytes that violate the framer's decoded UTF-8 invariant.
/// Guarantees: The original UTF-8 validation failure is surfaced as a structured FramerError rather than treated as no match.
#[test]
fn text_matcher_utf8_failure_is_structured() {
    let now = Instant::now();
    let settings = TestSettings {
        start_pattern: Some("^START"),
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    framer.line.decoded.push(0xff);

    let error = framer
        .step(b"\n", now)
        .expect_err("invalid matcher input must fail");
    assert!(matches!(
        error,
        FramerError::MatcherUtf8 { source } if source.valid_up_to() == 0
    ));
}

/// Scenario: Start-pattern mode sees a pre-match line and reads the next matching line before emitting.
/// Guarantees: Pre-match fallback is newline-framed, counted, and start lookahead preserves the prior separator.
#[test]
fn start_pattern_pre_match_and_read_ahead_are_ordered() {
    let now = Instant::now();
    let settings = TestSettings {
        start_pattern: Some("^START"),
        max_line: 32,
        max_record: 64,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);

    let first = feed(&mut framer, b"pre\nSTART one\n cont\n", now);
    assert_eq!(first.len(), 1);
    assert_eq!(text(&first[0]), "pre");
    assert_eq!(framer.pattern_not_matched_count(), 1);

    let second = feed(&mut framer, b"START two\n", now);
    assert_eq!(second.len(), 1);
    assert_eq!(text(&second[0]), "START one\n cont");
    assert_eq!(second[0].checkpoint_end, 20);
    assert_eq!(framer.next_expected_input_offset(), 30);
    assert_eq!(framer.pending_source_start(), Some(20));
}

/// Scenario: Consecutive start-pattern record bodies each exactly fill max_record_bytes.
/// Guarantees: Each trailing LF is an immediate forced boundary that emits an ordinary Clean record before later source is decoded, under both oversize policies.
#[test]
fn start_pattern_exact_bound_emits_at_forced_boundary() {
    let now = Instant::now();
    for behavior in [MaxLogSizeBehavior::Split, MaxLogSizeBehavior::Truncate] {
        let settings = TestSettings {
            encoding: Encoding::Raw,
            behavior,
            max_line: 4,
            max_record: 4,
            start_pattern: Some("^S"),
            ..TestSettings::default()
        };
        let mut framer = framer(&settings, now);
        let outputs = feed(&mut framer, b"S123\nS456\n", now);

        assert_eq!(outputs.len(), 2);
        assert_eq!(bytes(&outputs[0]), b"S123");
        assert_eq!(bytes(&outputs[1]), b"S456");
        assert_eq!(outputs[0].resulting_resume, FramingResume::Clean);
        assert_eq!(outputs[1].resulting_resume, FramingResume::Clean);
        assert!(outputs[0].fragment.is_none());
        assert!(outputs[1].fragment.is_none());
        assert!(!outputs[0].truncated);
        assert!(!outputs[1].truncated);
        assert_eq!(outputs[0].discarded_source_bytes, 0);
        assert_eq!(outputs[1].discarded_source_bytes, 0);
        assert_eq!(outputs[0].checkpoint_end, 5);
        assert_eq!(outputs[1].checkpoint_end, 10);
    }
}

/// Scenario: An exactly bounded start-pattern body is followed by a nonmatching line.
/// Guarantees: The size bound closes the prior record cleanly at its LF and re-enters bounded pre-match fallback for the next line.
#[test]
fn start_pattern_exact_bound_forces_clean_boundary_before_nonmatch() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 4,
        max_record: 4,
        start_pattern: Some("^S"),
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"S123\nnext\n", now);

    assert_eq!(outputs.len(), 2);
    assert_eq!(bytes(&outputs[0]), b"S123");
    assert_eq!(bytes(&outputs[1]), b"next");
    assert!(outputs[0].fragment.is_none());
    assert!(outputs[1].fragment.is_none());
    assert_eq!(framer.pattern_not_matched_count(), 1);
}

/// Scenario: An exactly bounded start-pattern record is followed by a partial nonmatching line before timeout, rotation, or drain.
/// Guarantees: The retained LF closes the prior record cleanly before the partial line is accepted, and every lifecycle hook flushes that line independently under split and truncate.
#[test]
fn start_pattern_exact_bound_precedes_partial_lifecycle_flushes() {
    let now = Instant::now();
    for behavior in [MaxLogSizeBehavior::Split, MaxLogSizeBehavior::Truncate] {
        for reason in [
            FlushReason::Timeout,
            FlushReason::Rotation,
            FlushReason::Drain,
        ] {
            let settings = TestSettings {
                encoding: Encoding::Raw,
                behavior,
                max_line: 4,
                max_record: 4,
                start_pattern: Some("^S"),
                ..TestSettings::default()
            };
            let mut framer = framer(&settings, now);
            let first = feed(&mut framer, b"S123\nx", now);

            assert_eq!(first.len(), 1);
            assert_eq!(bytes(&first[0]), b"S123");
            assert_eq!(first[0].resulting_resume, FramingResume::Clean);
            assert_eq!(first[0].flush_reason, None);
            assert!(first[0].fragment.is_none());
            assert!(!first[0].truncated);

            let tail = match reason {
                FlushReason::Timeout => {
                    framer.observe_eof(now).unwrap();
                    framer
                        .poll_timeout(now + settings.flush_period)
                        .unwrap()
                        .output
                }
                FlushReason::Rotation => framer.flush_rotation(now).unwrap().output,
                FlushReason::Drain => framer.flush_drain(now).unwrap().output,
                FlushReason::MaxLines | FlushReason::OversizeLineBoundary => {
                    unreachable!("only lifecycle reasons are in the test table")
                }
            }
            .expect("the partial next line must flush independently");

            assert_eq!(bytes(&tail), b"x");
            assert_eq!(tail.resulting_resume, FramingResume::Clean);
            assert_eq!(tail.flush_reason, Some(reason));
            assert!(tail.fragment.is_none());
            assert!(!tail.truncated);
        }
    }
}

/// Scenario: An exactly bounded ASCII start-pattern record is followed by malformed source under the fail policy.
/// Guarantees: Retained source-ordered work emits the complete clean record through its LF before decoding the following malformed byte fails, under both oversize policies.
#[test]
fn start_pattern_exact_bound_precedes_following_decode_failure() {
    let now = Instant::now();
    for behavior in [MaxLogSizeBehavior::Split, MaxLogSizeBehavior::Truncate] {
        let settings = TestSettings {
            encoding: Encoding::Ascii,
            policy: OnDecodeError::Fail,
            behavior,
            max_line: 1,
            max_record: 1,
            start_pattern: Some("^S"),
            ..TestSettings::default()
        };
        let mut framer = framer(&settings, now);
        let input = b"S\n\xff";

        let first = framer
            .step(input, now)
            .expect("the prior complete record must emit before later decoding");
        assert_eq!(first.consumed, 2);
        let output = first.output.expect("the exact-bound record must emit");
        assert_eq!(text(&output), "S");
        assert_eq!(output.checkpoint_end, 2);
        assert_eq!(output.resulting_resume, FramingResume::Clean);
        assert!(output.fragment.is_none());
        assert!(!output.truncated);

        let error = framer
            .step(&input[first.consumed..], now)
            .expect_err("the following malformed byte must still fail");
        assert!(matches!(
            error,
            FramerError::Decode(super::DecodeError::FatalMalformed {
                range: super::SourceRange { start: 2, end: 3 },
                ..
            })
        ));
    }
}

/// Scenario: End-pattern mode receives a CRLF terminator and earlier physical lines.
/// Guarantees: The regex sees CR as data and the emitted body retains every internal LF and terminal CR.
#[test]
fn end_pattern_sees_cr_and_preserves_internal_separators() {
    let now = Instant::now();
    let settings = TestSettings {
        end_pattern: Some(r"^END\r$"),
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"first\nsecond\nEND\r\n", now);

    assert_eq!(outputs.len(), 1);
    assert_eq!(text(&outputs[0]), "first\nsecond\nEND\r");
    assert_eq!(outputs[0].checkpoint_end, 18);
}

/// Scenario: End-pattern match and max_multiline_lines occur on the same physical line.
/// Guarantees: End-pattern completion wins the tie, while an unmatched record reports max_lines.
#[test]
fn end_match_wins_max_lines_tie() {
    let now = Instant::now();
    let settings = TestSettings {
        end_pattern: Some("^END$"),
        max_lines: 2,
        ..TestSettings::default()
    };
    let mut matched = framer(&settings, now);
    let matched_output = feed(&mut matched, b"a\nEND\n", now).remove(0);
    assert_eq!(text(&matched_output), "a\nEND");
    assert_eq!(matched_output.flush_reason, None);

    let mut capped = framer(&settings, now);
    let capped_output = feed(&mut capped, b"a\nb\n", now).remove(0);
    assert_eq!(text(&capped_output), "a\nb");
    assert_eq!(capped_output.flush_reason, Some(FlushReason::MaxLines));
}

/// Scenario: A partial line reaches EOF with force flushing enabled.
/// Guarantees: Explicit EOF arms one non-postponed idle deadline, whose expiry emits Clean progress at a complete decoder boundary.
#[test]
fn timeout_flushes_partial_line_to_clean_progress() {
    let start = Instant::now();
    let settings = TestSettings {
        flush_period: Duration::from_millis(10),
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, start);
    assert!(feed(&mut framer, b"partial", start).is_empty());
    assert_eq!(framer.deadline(), None);
    framer.observe_eof(start).unwrap();
    assert_eq!(
        framer.deadline(),
        start.checked_add(Duration::from_millis(10))
    );
    assert!(
        framer
            .poll_timeout(start + Duration::from_millis(9))
            .unwrap()
            .output
            .is_none()
    );
    framer
        .observe_eof(start + Duration::from_millis(9))
        .unwrap();
    assert_eq!(
        framer.deadline(),
        start.checked_add(Duration::from_millis(10))
    );

    let flushed = framer
        .poll_timeout(start + Duration::from_millis(10))
        .unwrap()
        .output
        .expect("timeout must flush");
    assert_eq!(text(&flushed), "partial");
    assert_eq!(flushed.flush_reason, Some(FlushReason::Timeout));
    assert_eq!(flushed.resulting_resume, FramingResume::Clean);
    assert_eq!(flushed.checkpoint_end, 7);
}

/// Scenario: Partial flushing is disabled and drain observes an unterminated line.
/// Guarantees: Drain reports recoverable pending state without committing or dropping source bytes.
#[test]
fn disabled_partial_flush_reports_pending_on_drain() {
    let now = Instant::now();
    let settings = TestSettings {
        flush_period: Duration::ZERO,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    assert!(feed(&mut framer, b"pending", now).is_empty());
    assert_eq!(framer.deadline(), None);

    let drain = framer.flush_drain(now).unwrap();
    assert!(drain.output.is_none());
    assert!(drain.pending);
    assert_eq!(framer.pending_source_start(), Some(0));
}

/// Scenario: Timeout occurs after a complete byte and an incomplete UTF-8 scalar prefix were consumed.
/// Guarantees: EOF-gated timeout stops the checkpoint before the incomplete scalar, which begins the next Clean record when completed.
#[test]
fn timeout_never_commits_incomplete_decoder_unit() {
    let start = Instant::now();
    let mut framer = framer(&TestSettings::default(), start);
    assert!(feed(&mut framer, &[b'a', 0xe2, 0x82], start).is_empty());
    framer.observe_eof(start).unwrap();

    let first = framer
        .poll_timeout(start + Duration::from_millis(10))
        .unwrap()
        .output
        .expect("complete prefix must flush");
    assert_eq!(text(&first), "a");
    assert_eq!(first.checkpoint_end, 1);
    assert_eq!(framer.next_expected_input_offset(), 3);

    let second = feed(
        &mut framer,
        &[0xac, b'\n'],
        start + Duration::from_millis(11),
    );
    assert_eq!(second.len(), 1);
    assert_eq!(text(&second[0]), "\u{20ac}");
    assert_eq!(
        second[0].body_source_range,
        super::SourceRange { start: 1, end: 4 }
    );
    assert_eq!(
        second[0].frame_source_range,
        super::SourceRange { start: 1, end: 5 }
    );
}

/// Scenario: EOF-gated timeout sees only an incomplete encoded scalar and has no complete body prefix.
/// Guarantees: Polling reports pending without output and disarms the expired deadline until another EOF observation.
#[test]
fn timeout_with_only_incomplete_unit_does_not_busy_poll() {
    let start = Instant::now();
    let mut framer = framer(&TestSettings::default(), start);
    assert!(feed(&mut framer, &[0xe2, 0x82], start).is_empty());
    framer.observe_eof(start).unwrap();

    let poll = framer
        .poll_timeout(start + Duration::from_millis(10))
        .unwrap();
    assert!(poll.output.is_none());
    assert!(poll.pending);
    assert_eq!(framer.deadline(), None);
    assert_eq!(framer.pending_source_start(), Some(0));
}

/// Scenario: A raw physical line crosses the individual line/record bound under split.
/// Guarantees: Every body byte is preserved in bounded indexed fragments and the final LF yields Clean state.
#[test]
fn physical_line_split_preserves_all_bytes() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 4,
        max_record: 8,
        behavior: MaxLogSizeBehavior::Split,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"abcdef\n", now);

    assert_eq!(outputs.len(), 2);
    assert_eq!(bytes(&outputs[0]), b"abcd");
    assert_eq!(bytes(&outputs[1]), b"ef");
    let first = outputs[0].fragment.as_ref().unwrap();
    let second = outputs[1].fragment.as_ref().unwrap();
    assert_eq!(first.index, 0);
    assert!(!first.last);
    assert_eq!(second.index, 1);
    assert!(second.last);
    assert_eq!(first.id, second.id);
    assert_eq!(
        outputs[0].resulting_resume,
        FramingResume::Continuation {
            record_start_offset: 0,
            record_end_offset: 0,
            next_fragment_index: 1
        }
    );
    assert_eq!(outputs[1].resulting_resume, FramingResume::Clean);
}

/// Scenario: A decoded scalar would cross a text fragment bound.
/// Guarantees: Split occurs before the scalar and no emitted text fragment cuts a Unicode scalar.
#[test]
fn text_split_never_cuts_a_decoded_scalar() {
    let now = Instant::now();
    let settings = TestSettings {
        policy: OnDecodeError::Replace,
        max_line: 4,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, "a\u{20ac}b\n".as_bytes(), now);

    assert_eq!(outputs.len(), 2);
    assert_eq!(text(&outputs[0]), "a\u{20ac}");
    assert_eq!(text(&outputs[1]), "b");
    assert!(text(&outputs[0]).len() <= 4);
    assert!(text(&outputs[1]).len() <= 4);
}

/// Scenario: A raw physical line crosses its bound under truncate.
/// Guarantees: Only a bounded prefix is retained, discarded source bytes are exact, and LF is not counted.
#[test]
fn physical_line_truncate_counts_discarded_body_bytes() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 4,
        max_record: 8,
        behavior: MaxLogSizeBehavior::Truncate,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let output = feed(&mut framer, b"abcdef\n", now).remove(0);

    assert_eq!(bytes(&output), b"abcd");
    assert!(output.truncated);
    assert_eq!(output.discarded_source_bytes, 2);
    assert_eq!(
        output.body_source_range,
        super::SourceRange { start: 0, end: 4 }
    );
    assert_eq!(
        output.frame_source_range,
        super::SourceRange { start: 0, end: 7 }
    );
}

/// Scenario: Replacement and preserve-raw malformed units occur only after truncation starts.
/// Guarantees: Discarded decode evidence remains counted while the bounded prefix representation follows policy.
#[test]
fn truncate_reports_decode_outcomes_from_discarded_tail() {
    let now = Instant::now();
    for (policy, expected) in [
        (
            OnDecodeError::Replace,
            DecodeOutcome::Replacements { count: 1 },
        ),
        (
            OnDecodeError::PreserveRaw,
            DecodeOutcome::PreserveRaw { count: 1 },
        ),
    ] {
        let settings = TestSettings {
            policy,
            behavior: MaxLogSizeBehavior::Truncate,
            max_line: 4,
            max_record: 8,
            ..TestSettings::default()
        };
        let mut framer = framer(&settings, now);
        let output = feed(&mut framer, b"abcd\xffx\n", now).remove(0);
        assert_eq!(output.decode_outcome, expected);
        assert_eq!(output.discarded_source_bytes, 2);
        match policy {
            OnDecodeError::Replace => assert_eq!(text(&output), "abcd"),
            OnDecodeError::PreserveRaw => assert_eq!(text(&output), "abcd"),
            OnDecodeError::Fail => unreachable!("fail is not in the test table"),
        }
    }
}

/// Scenario: Preserve-raw UTF-16 truncation retains only clean units while a malformed surrogate and a clean unit occur in the discarded tail.
/// Guarantees: The retained prefix remains Text, while the discarded malformed unit is counted in the decode outcome and its exact source bytes are included in discarded progress.
#[test]
fn discarded_utf16_malformed_tail_does_not_change_clean_prefix_to_bytes() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Utf16Le,
        policy: OnDecodeError::PreserveRaw,
        behavior: MaxLogSizeBehavior::Truncate,
        max_line: 4,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let output = feed(
        &mut framer,
        &[b'A', 0, b'B', 0, 0x00, 0xd8, b'C', 0, b'\n', 0],
        now,
    )
    .remove(0);

    assert_eq!(text(&output), "AB");
    assert_eq!(
        output.decode_outcome,
        DecodeOutcome::PreserveRaw { count: 1 }
    );
    assert!(output.truncated);
    assert_eq!(output.discarded_source_bytes, 4);
    assert_eq!(
        output.body_source_range,
        super::SourceRange { start: 0, end: 4 }
    );
    assert_eq!(
        output.frame_source_range,
        super::SourceRange { start: 0, end: 10 }
    );
}

/// Scenario: Preserve-raw truncation scans a complete valid UTF-16 line before emitting its retained prefix.
/// Guarantees: A clean final malformed count selects decoded Text even though exact source shadows were retained while scanning.
#[test]
fn clean_preserve_raw_truncate_emits_text() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Utf16Le,
        policy: OnDecodeError::PreserveRaw,
        behavior: MaxLogSizeBehavior::Truncate,
        max_line: 4,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let output = feed(&mut framer, &[b'A', 0, b'B', 0, b'C', 0, b'\n', 0], now).remove(0);

    assert_eq!(text(&output), "AB");
    assert_eq!(output.decode_outcome, DecodeOutcome::Clean);
    assert!(output.truncated);
    assert_eq!(output.discarded_source_bytes, 2);
}

/// Scenario: Preserve-raw truncation retains a malformed UTF-8 unit and discards later clean units.
/// Guarantees: The retained body remains exact Bytes and the final malformed count includes retained evidence.
#[test]
fn preserve_raw_truncate_with_retained_malformed_unit_emits_exact_bytes() {
    let now = Instant::now();
    let settings = TestSettings {
        policy: OnDecodeError::PreserveRaw,
        behavior: MaxLogSizeBehavior::Truncate,
        max_line: 4,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let output = feed(&mut framer, b"a\xffbc\n", now).remove(0);

    assert_eq!(bytes(&output), b"a\xff");
    assert_eq!(
        output.decode_outcome,
        DecodeOutcome::PreserveRaw { count: 1 }
    );
    assert_eq!(output.discarded_source_bytes, 2);
}

/// Scenario: Valid UTF-16 text has a smaller decoded UTF-8 length than exact source length.
/// Guarantees: Preserve-raw prospective sizing uses the larger source shadow and all split bodies remain exact bytes.
#[test]
fn preserve_raw_split_uses_max_of_decoded_and_source_lengths() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Utf16Le,
        policy: OnDecodeError::PreserveRaw,
        max_line: 4,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, &[b'A', 0, b'B', 0, b'C', 0, b'\n', 0], now);

    assert_eq!(outputs.len(), 2);
    assert_eq!(bytes(&outputs[0]), &[b'A', 0, b'B', 0]);
    assert_eq!(bytes(&outputs[1]), &[b'C', 0]);
    assert!(
        outputs
            .iter()
            .all(|output| output.decode_outcome == DecodeOutcome::Clean)
    );
}

/// Scenario: max_record_bytes is tighter than max_line_bytes for one raw physical line.
/// Guarantees: The individual physical-line limit is the minimum of both configured byte bounds.
#[test]
fn individual_line_uses_minimum_of_line_and_record_bounds() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 10,
        max_record: 4,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"abcdef\n", now);

    assert_eq!(outputs.len(), 2);
    assert_eq!(bytes(&outputs[0]), b"abcd");
    assert_eq!(bytes(&outputs[1]), b"ef");
}

/// Scenario: A multiline record crosses max_record_bytes when one bounded trigger line is attached.
/// Guarantees: Split emits the prior separator-bearing buffer nonfinal and the trigger line final at its LF.
#[test]
fn aggregate_multiline_split_terminates_at_trigger_line_lf() {
    let now = Instant::now();
    let settings = TestSettings {
        end_pattern: Some("^END$"),
        max_line: 8,
        max_record: 8,
        behavior: MaxLogSizeBehavior::Split,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"abc\ndefgh\nlater\n", now);

    assert_eq!(outputs.len(), 2);
    assert_eq!(bytes(&outputs[0]), b"abc\n");
    assert_eq!(bytes(&outputs[1]), b"defgh");
    assert!(!outputs[0].fragment.as_ref().unwrap().last);
    assert!(outputs[1].fragment.as_ref().unwrap().last);
    assert_eq!(outputs[0].checkpoint_end, 4);
    assert_eq!(outputs[1].checkpoint_end, 10);
    assert_eq!(framer.pending_source_start(), Some(10));
}

/// Scenario: A clean preserve-raw multiline record crosses max_record_bytes under truncate.
/// Guarantees: The longest whole-unit Text prefix is emitted only at trigger LF and the remaining body is counted.
#[test]
fn aggregate_multiline_truncate_waits_for_trigger_boundary() {
    let now = Instant::now();
    let settings = TestSettings {
        end_pattern: Some("^END$"),
        max_line: 8,
        max_record: 8,
        behavior: MaxLogSizeBehavior::Truncate,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let output = feed(&mut framer, b"abc\ndefgh\n", now).remove(0);

    assert_eq!(text(&output), "abc\ndefg");
    assert!(output.truncated);
    assert_eq!(output.discarded_source_bytes, 1);
    assert_eq!(output.checkpoint_end, 10);
}

/// Scenario: A buffered start-pattern record is followed by an oversized physical line.
/// Guarantees: The earlier Clean record is emitted first with oversize_line_boundary, then the line fragments in order.
#[test]
fn buffered_multiline_is_emitted_before_oversized_line() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        start_pattern: Some("^S$"),
        max_line: 4,
        max_record: 16,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"S\nx\nabcdef\n", now);

    assert_eq!(outputs.len(), 3);
    assert_eq!(bytes(&outputs[0]), b"S\nx");
    assert_eq!(
        outputs[0].flush_reason,
        Some(FlushReason::OversizeLineBoundary)
    );
    assert!(outputs[0].fragment.is_none());
    assert_eq!(bytes(&outputs[1]), b"abcd");
    assert_eq!(bytes(&outputs[2]), b"ef");
    assert_eq!(outputs[0].checkpoint_end, 4);
    assert_eq!(outputs[1].frame_source_range.start, 4);
}

/// Scenario: A split is retried live and reconstructed after restart at a mid-line continuation.
/// Guarantees: SHA-256 id, index, last marker, and bodies are stable without exposing the raw file id.
#[test]
fn fragment_identity_and_mid_line_resume_are_stable() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 4,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut live = framer(&settings, now);
    let live_outputs = feed(&mut live, b"abcdef\n", now);
    let expected_id = fragment_id(FILE_ID, 7, 0);
    assert_eq!(live_outputs[0].fragment.as_ref().unwrap().id, expected_id);
    assert_eq!(expected_id.len(), 64);
    assert!(
        expected_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );

    let mut resumed = resumed_framer(&settings, 4, 0, 1, now).unwrap();
    let resumed_output = feed(&mut resumed, b"ef\n", now).remove(0);
    let metadata = resumed_output.fragment.as_ref().unwrap();
    assert_eq!(bytes(&resumed_output), b"ef");
    assert_eq!(metadata.id, expected_id);
    assert_eq!(metadata.index, 1);
    assert!(metadata.last);
}

/// Scenario: Aggregate overflow resumes exactly at the trigger physical-line start.
/// Guarantees: Continuation ignores multiline regex and closes at the first LF using the supplied next index.
#[test]
fn continuation_at_trigger_line_start_closes_at_first_lf() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        end_pattern: Some("^END$"),
        max_line: 8,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut resumed = resumed_framer(&settings, 4, 0, 1, now).unwrap();
    let output = feed(&mut resumed, b"defgh\nlater\n", now).remove(0);

    assert_eq!(bytes(&output), b"defgh");
    assert_eq!(output.fragment.as_ref().unwrap().index, 1);
    assert!(output.fragment.as_ref().unwrap().last);
    assert_eq!(output.checkpoint_end, 10);
    assert_eq!(resumed.pending_source_start(), Some(10));
}

/// Scenario: Preserve-raw splitting encounters malformed UTF-8 only after the first fragment was ready.
/// Guarantees: Every fragment in the open-ended sequence is bytes, preserving later malformed evidence exactly.
#[test]
fn preserve_raw_split_uses_bytes_before_later_malformed_evidence() {
    let now = Instant::now();
    let settings = TestSettings {
        max_line: 4,
        max_record: 8,
        policy: OnDecodeError::PreserveRaw,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"abcd\xff\n", now);

    assert_eq!(outputs.len(), 2);
    assert_eq!(bytes(&outputs[0]), b"abcd");
    assert_eq!(bytes(&outputs[1]), b"\xff");
    assert_eq!(outputs[0].decode_outcome, DecodeOutcome::Clean);
    assert_eq!(
        outputs[1].decode_outcome,
        DecodeOutcome::PreserveRaw { count: 1 }
    );
}

/// Scenario: The same UTF-16 multiline stream is supplied under every possible source-byte partition.
/// Guarantees: Bodies, source ranges, progress, and resume metadata are invariant under input partitioning.
#[test]
fn output_is_invariant_under_chunk_partitions() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Utf16Le,
        end_pattern: Some("^A$"),
        max_line: 16,
        max_record: 32,
        ..TestSettings::default()
    };
    let source = [0xff, 0xfe, b'A', 0, b'\n', 0];
    let mut whole = framer(&settings, now);
    let expected = feed(&mut whole, &source, now);

    for mask in 0..(1_u16 << (source.len() - 1)) {
        let mut chunks = Vec::new();
        let mut start = 0;
        for boundary in 0..source.len() - 1 {
            if mask & (1 << boundary) != 0 {
                chunks.push(&source[start..=boundary]);
                start = boundary + 1;
            }
        }
        chunks.push(&source[start..]);
        let mut partitioned = framer(&settings, now);
        let actual = feed_chunks(&mut partitioned, &chunks, now);
        assert_eq!(actual, expected, "partition mask {mask:#x}");
    }
}

/// Scenario: Continuation index u32::MAX needs another nonfinal fragment before LF.
/// Guarantees: The framer fails before emitting an unrepresentable continuation, while a final max index remains valid.
#[test]
fn fragment_index_overflow_fails_only_for_nonfinal_output() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 4,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut overflow = resumed_framer(&settings, 4, 0, u32::MAX, now).unwrap();
    let error = overflow
        .step(b"abcde", now)
        .expect_err("another fragment cannot be represented");
    assert_eq!(error, FramerError::FragmentIndexOverflow);

    let mut final_only = resumed_framer(&settings, 4, 0, u32::MAX, now).unwrap();
    let output = feed(&mut final_only, b"a\n", now).remove(0);
    assert_eq!(output.fragment.as_ref().unwrap().index, u32::MAX);
    assert!(output.fragment.as_ref().unwrap().last);
}

/// Scenario: Durable continuation has index zero or a record start at/after committed progress.
/// Guarantees: Both impossible checkpoint states fail construction with structured errors.
#[test]
fn impossible_continuation_states_are_rejected() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        ..TestSettings::default()
    };
    assert_eq!(
        resumed_framer(&settings, 10, 0, 0, now).unwrap_err(),
        FramerError::ContinuationIndexZero
    );
    assert_eq!(
        resumed_framer(&settings, 10, 10, 1, now).unwrap_err(),
        FramerError::ContinuationRecordStart {
            record_start_offset: 10,
            committed_offset: 10
        }
    );
}

/// Scenario: Durable continuation carries a nonzero known record end that
/// does not lie strictly beyond the committed offset it resumes at.
/// Guarantees: Both an end equal to and an end before the committed offset
/// fail construction with a structured error instead of being silently
/// accepted or reinterpreted as the scan-to-next-LF sentinel.
#[test]
fn resumed_continuation_rejects_a_record_end_not_after_the_committed_offset() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        ..TestSettings::default()
    };
    assert_eq!(
        resumed_framer_with_end(&settings, 10, 0, 10, 1, now).unwrap_err(),
        FramerError::ContinuationRecordEnd {
            record_end_offset: 10,
            committed_offset: 10
        }
    );
    assert_eq!(
        resumed_framer_with_end(&settings, 10, 0, 5, 1, now).unwrap_err(),
        FramerError::ContinuationRecordEnd {
            record_end_offset: 5,
            committed_offset: 10
        }
    );
}

/// Scenario: A resumed continuation carries a nonzero exact known record
/// end, and the remaining bytes contain an embedded LF strictly before it.
/// Guarantees: Normal newline termination is suppressed; the embedded LF is
/// ordinary bounded-fragment content, and only the fragment whose source
/// range ends exactly at the known end is `last`, with a resulting Clean
/// resume.
#[test]
fn resumed_continuation_with_known_end_ignores_embedded_lf_and_ends_exactly_at_boundary() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        ..TestSettings::default()
    };
    let mut framer = resumed_framer_with_end(&settings, 4, 0, 10, 1, now)
        .expect("resume with a known end must construct");
    let outputs = feed(&mut framer, b"ab\ncde", now);
    assert_eq!(outputs.len(), 1);
    let record = &outputs[0];
    assert_eq!(bytes(record), b"ab\ncde");
    assert_eq!(record.checkpoint_end, 10);
    let fragment = record
        .fragment
        .as_ref()
        .expect("a continuing fragment must carry split metadata");
    assert!(fragment.last);
    assert_eq!(fragment.index, 1);
    assert_eq!(record.resulting_resume, FramingResume::Clean);
}

/// Scenario: A resumed continuation's exact known end still requires more
/// than one bounded fragment (the remaining span exceeds `max_record_bytes`).
/// Guarantees: Every nonfinal fragment carries the same known end forward
/// unchanged (never resetting to the scan-to-next-LF sentinel), fragment
/// indices are stable and sequential, and only the fragment reaching the
/// exact end is `last` with a resulting Clean state.
#[test]
fn resumed_continuation_with_known_end_emits_multiple_bounded_fragments_before_the_final_one() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 4,
        max_record: 4,
        behavior: MaxLogSizeBehavior::Split,
        ..TestSettings::default()
    };
    // Resume at offset 4 with an exact known end 10 bytes further on
    // (offset 14): more than `max_record_bytes` (4) remains, so it must
    // still emit bounded nonfinal fragments before the final exact-end one.
    let mut framer = resumed_framer_with_end(&settings, 4, 0, 14, 1, now)
        .expect("resume with a known end must construct");
    let outputs = feed(&mut framer, b"abcdefghij", now);
    assert_eq!(outputs.len(), 3);
    assert_eq!(bytes(&outputs[0]), b"abcd");
    assert_eq!(bytes(&outputs[1]), b"efgh");
    assert_eq!(bytes(&outputs[2]), b"ij");
    let first = outputs[0].fragment.as_ref().unwrap();
    let second = outputs[1].fragment.as_ref().unwrap();
    let third = outputs[2].fragment.as_ref().unwrap();
    assert_eq!((first.index, first.last), (1, false));
    assert_eq!((second.index, second.last), (2, false));
    assert_eq!((third.index, third.last), (3, true));
    assert_eq!(first.id, second.id);
    assert_eq!(second.id, third.id);
    assert_eq!(
        outputs[0].resulting_resume,
        FramingResume::Continuation {
            record_start_offset: 0,
            record_end_offset: 14,
            next_fragment_index: 2,
        }
    );
    assert_eq!(
        outputs[1].resulting_resume,
        FramingResume::Continuation {
            record_start_offset: 0,
            record_end_offset: 14,
            next_fragment_index: 3,
        }
    );
    assert_eq!(outputs[2].resulting_resume, FramingResume::Clean);
    assert_eq!(outputs[2].checkpoint_end, 14);
}

/// Scenario: A decoded scalar spans a resumed continuation's exact known
/// record end.
/// Guarantees: The framer fails closed instead of splitting the scalar or
/// reinterpreting the boundary as scan-to-next-LF.
#[test]
fn resumed_continuation_with_known_end_fails_closed_on_unit_overshoot() {
    let now = Instant::now();
    let settings = TestSettings::default();
    let mut framer = resumed_framer_with_end(&settings, 4, 0, 6, 1, now)
        .expect("resume with a known end must construct");
    let err = framer
        .step("\u{20ac}".as_bytes(), now)
        .expect_err("a unit crossing the exact record end must fail closed");
    assert_eq!(
        err,
        FramerError::ContinuationRecordEndOverrun {
            record_end: 6,
            unit_end: 7,
        }
    );
}

/// Scenario: A resumed continuation with a known end observes only a
/// temporary EOF before the boundary is reached.
/// Guarantees: The framer never fabricates a final fragment at the idle
/// timeout; it remains pending and fully recoverable, exactly like the
/// scan-to-next-LF case.
#[test]
fn resumed_continuation_with_known_end_stays_pending_on_temporary_eof() {
    let start = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        flush_period: Duration::from_millis(10),
        ..TestSettings::default()
    };
    let mut framer = resumed_framer_with_end(&settings, 4, 0, 10, 1, start)
        .expect("resume with a known end must construct");
    assert!(feed(&mut framer, b"ab", start).is_empty());
    framer.observe_eof(start).unwrap();
    let poll = framer
        .poll_timeout(start + Duration::from_millis(10))
        .unwrap();
    assert!(poll.output.is_none());
    assert!(poll.pending);
    assert_eq!(framer.pending_source_start(), Some(4));
}

/// Scenario: Durable continuation is paired with truncate policy, or BOM probing is requested at a nonzero offset.
/// Guarantees: Both impossible runtime/checkpoint combinations fail before decoder or framing state is constructed.
#[test]
fn incompatible_resume_and_stream_start_states_are_rejected() {
    let now = Instant::now();
    let truncate = TestSettings {
        encoding: Encoding::Raw,
        behavior: MaxLogSizeBehavior::Truncate,
        ..TestSettings::default()
    };
    assert_eq!(
        resumed_framer(&truncate, 10, 0, 1, now).unwrap_err(),
        FramerError::ContinuationRequiresSplit
    );

    let settings = TestSettings {
        encoding: Encoding::Raw,
        ..TestSettings::default()
    };
    assert_eq!(
        Framer::new(
            FILE_ID,
            7,
            &runtime(&settings),
            10,
            FramingResume::Clean,
            true,
            zero_window(10),
            now,
        )
        .unwrap_err(),
        FramerError::InvalidNewStreamOffset {
            committed_offset: 10
        }
    );
}

/// Scenario: A restarted continuation observes EOF without receiving any new source bytes.
/// Guarantees: Construction is unarmed, and an expired EOF deadline reports pending without a zero-source final fragment or Clean transition.
#[test]
fn empty_resumed_continuation_timeout_stays_pending() {
    let start = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 4,
        max_record: 8,
        flush_period: Duration::from_millis(10),
        ..TestSettings::default()
    };
    let mut framer = resumed_framer(&settings, 4, 0, 1, start).unwrap();
    assert_eq!(framer.deadline(), None);
    framer.observe_eof(start).unwrap();
    assert_eq!(
        framer.deadline(),
        start.checked_add(Duration::from_millis(10))
    );

    let poll = framer
        .poll_timeout(start + Duration::from_millis(10))
        .unwrap();
    assert!(poll.output.is_none());
    assert!(poll.pending);
    assert_eq!(framer.deadline(), None);
    assert_eq!(
        framer.pending_source_start(),
        Some(4),
        "continuation state must remain recoverable"
    );

    let repeated = framer
        .poll_timeout(start + Duration::from_millis(20))
        .unwrap();
    assert!(repeated.output.is_none());
    assert!(repeated.pending);
    framer
        .observe_eof(start + Duration::from_millis(20))
        .unwrap();
    assert_eq!(
        framer.deadline(),
        start.checked_add(Duration::from_millis(30))
    );
}

/// Scenario: New tail bytes arrive for a resumed continuation exactly when its prior EOF deadline expires.
/// Guarantees: Nonempty input disarms the stale deadline and emits the real tail fragment instead of empty timeout output.
#[test]
fn resumed_continuation_input_at_old_deadline_wins_over_timeout() {
    let start = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 8,
        max_record: 8,
        flush_period: Duration::from_millis(10),
        ..TestSettings::default()
    };
    let mut framer = resumed_framer(&settings, 4, 0, 1, start).unwrap();
    framer.observe_eof(start).unwrap();

    let output = feed(&mut framer, b"tail\n", start + Duration::from_millis(10)).remove(0);
    assert_eq!(bytes(&output), b"tail");
    assert_eq!(output.flush_reason, None);
    assert_eq!(
        output.body_source_range,
        super::SourceRange { start: 4, end: 8 }
    );
    assert_eq!(
        output.frame_source_range,
        super::SourceRange { start: 4, end: 9 }
    );
    assert_eq!(output.resulting_resume, FramingResume::Clean);
    assert!(output.fragment.as_ref().unwrap().last);
}

/// Scenario: A newly constructed clean framer observes EOF on a truly empty source.
/// Guarantees: Empty EOF leaves no pending state, deadline, or output to poll.
#[test]
fn true_empty_eof_does_not_arm_or_emit() {
    let start = Instant::now();
    let mut framer = framer(&TestSettings::default(), start);

    framer.observe_eof(start).unwrap();
    assert_eq!(framer.deadline(), None);
    let poll = framer
        .poll_timeout(start + Duration::from_millis(10))
        .unwrap();
    assert!(poll.output.is_none());
    assert!(!poll.pending);
}

/// Scenario: Construction receives a nonzero force-flush period outside the host Instant domain.
/// Guarantees: DeadlineOverflow is returned during preflight before any decoder state can be created or advanced.
#[test]
fn construction_preflights_force_flush_deadline() {
    let now = Instant::now();
    let mut runtime = runtime(&TestSettings::default());
    runtime.framing.force_flush_period = Duration::MAX;

    let error = Framer::new(
        FILE_ID,
        7,
        &runtime,
        0,
        FramingResume::Clean,
        true,
        zero_window(0),
        now,
    )
    .expect_err("Duration::MAX must exceed the host Instant domain");
    assert_eq!(error, FramerError::DeadlineOverflow);
}

/// Scenario: Start-pattern max_lines flush is followed by a complete nonmatching line.
/// Guarantees: The cap resets start mode to seeking, so the following line uses counted newline fallback.
#[test]
fn start_mode_returns_to_seeking_after_max_lines() {
    let now = Instant::now();
    let settings = TestSettings {
        start_pattern: Some("^S$"),
        max_lines: 2,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let outputs = feed(&mut framer, b"S\na\norphan\n", now);

    assert_eq!(outputs.len(), 2);
    assert_eq!(text(&outputs[0]), "S\na");
    assert_eq!(outputs[0].flush_reason, Some(FlushReason::MaxLines));
    assert_eq!(text(&outputs[1]), "orphan");
    assert_eq!(framer.pattern_not_matched_count(), 1);
}

/// Scenario: Truncate scans a very long raw tail without LF.
/// Guarantees: Retained capacities stay within the documented peak-allocation formula and discarded bytes are numeric, not buffered.
#[test]
fn truncate_tail_retention_is_bounded() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Raw,
        max_line: 8,
        max_record: 8,
        behavior: MaxLogSizeBehavior::Truncate,
        flush_period: Duration::ZERO,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let tail = vec![b'x'; 100_000];
    assert!(feed(&mut framer, &tail, now).is_empty());
    assert!(
        framer.retained_payload_capacity()
            <= framer
                .peak_payload_capacity_bound()
                .expect("capacity formula must fit")
    );

    let output = feed(&mut framer, b"\n", now).remove(0);
    assert_eq!(bytes(&output), b"xxxxxxxx");
    assert_eq!(output.discarded_source_bytes, 99_992);
}

/// Scenario: Preserve-raw payload vectors reallocate while physical-line and logical-record slots are both live.
/// Guarantees: The shared peak formula covers modeled old/new allocation overlap and exceeds the prior quiescent-only factor.
#[test]
fn payload_bound_covers_modeled_reallocation_peak() {
    let now = Instant::now();
    let settings = TestSettings {
        max_line: 8,
        max_record: 12,
        policy: OnDecodeError::PreserveRaw,
        ..TestSettings::default()
    };
    let framer = framer(&settings, now);
    let copies = 2usize;
    let live_payload = copies * (8 + 12);
    let fixed = 16 * copies + 16;
    let quiescent_capacity_with_growth_slack = 2 * live_payload + fixed;
    let modeled_reallocation_peak = 4 * live_payload + fixed;
    let bound = framer
        .peak_payload_capacity_bound()
        .expect("peak payload formula must fit");

    assert_eq!(bound, modeled_reallocation_peak);
    assert!(
        modeled_reallocation_peak > quiescent_capacity_with_growth_slack,
        "transient reallocation must require the additional factor"
    );
}

/// Scenario: Fail policy encounters malformed ASCII after truncation has begun discarding.
/// Guarantees: Decode Fail remains terminal even in discarded bytes and cannot silently hide malformed evidence.
#[test]
fn fail_policy_still_checks_discarded_truncate_tail() {
    let now = Instant::now();
    let settings = TestSettings {
        encoding: Encoding::Ascii,
        policy: OnDecodeError::Fail,
        behavior: MaxLogSizeBehavior::Truncate,
        max_line: 4,
        max_record: 8,
        ..TestSettings::default()
    };
    let mut framer = framer(&settings, now);
    let error = framer
        .step(b"abcdx\xff\n", now)
        .expect_err("malformed discarded source must fail");
    assert!(matches!(
        error,
        FramerError::Decode(super::DecodeError::FatalMalformed {
            range: super::SourceRange { start: 5, end: 6 },
            ..
        })
    ));
}

/// Scenario: Rotation and drain hooks see enabled partial flushing on separate partial records.
/// Guarantees: Each hook emits a reason-marked Clean record at the latest complete decoder boundary.
#[test]
fn rotation_and_drain_hooks_flush_when_enabled() {
    let now = Instant::now();
    let mut framer = framer(&TestSettings::default(), now);
    assert!(feed(&mut framer, b"one", now).is_empty());
    let rotation = framer
        .flush_rotation(now)
        .unwrap()
        .output
        .expect("rotation must flush");
    assert_eq!(rotation.flush_reason, Some(FlushReason::Rotation));
    assert_eq!(rotation.resulting_resume, FramingResume::Clean);

    assert!(feed(&mut framer, b"two", now).is_empty());
    let drain = framer
        .flush_drain(now)
        .unwrap()
        .output
        .expect("drain must flush");
    assert_eq!(drain.flush_reason, Some(FlushReason::Drain));
    assert_eq!(text(&drain), "two");
}

/// Scenario: a Framer is constructed at restart with a real, distinguishable
/// (non-zero-filled) committed-frontier seed window at a nonzero offset,
/// then frames a record whose owned span advances by fewer than 64 bytes.
/// Guarantees: the emitted record's checkpoint window is the exact real
/// `min(checkpoint_end, 64)` bytes ending at that new offset -- the seed's
/// retained tail combined with the newly consumed bytes -- never a
/// fabricated or zero-filled placeholder, and its guard is independently
/// reproducible from those exact bytes.
#[test]
fn checkpoint_window_combines_restart_seed_with_bytes_advanced_below_64() {
    let now = Instant::now();
    let settings = TestSettings::default();
    let seed_bytes: Vec<u8> = (0u8..64).collect();
    let seed = CommittedFrontierWindow::new(1000, seed_bytes.clone()).unwrap();
    let mut framer = Framer::new(
        FILE_ID,
        7,
        &runtime(&settings),
        1000,
        FramingResume::Clean,
        false,
        seed,
        now,
    )
    .expect("restart with a real seed window must construct");

    // Advance by 5 new bytes, well below the 64-byte window.
    let outputs = feed(&mut framer, b"abcd\n", now);
    let record = outputs.first().expect("newline framing must emit a record");
    assert_eq!(record.checkpoint_end, 1005);

    // Expected window: the last 59 seed bytes followed by the 5 newly
    // consumed bytes, ending at the new offset 1005.
    let mut expected_bytes = seed_bytes[5..].to_vec();
    expected_bytes.extend_from_slice(b"abcd\n");
    assert_eq!(expected_bytes.len(), 64);
    let expected = CommittedFrontierWindow::new(1005, expected_bytes).unwrap();
    assert_eq!(record.checkpoint_window, expected);
    assert_eq!(
        record.checkpoint_window.guard().unwrap(),
        expected.guard().unwrap()
    );
}

/// Scenario: a Framer restarts at offset zero and frames its first record.
/// Guarantees: the first checkpoint window is the exact empty window (no
/// preceding bytes exist), so its guard equals
/// `CommittedFrontierGuard::empty()`.
#[test]
fn checkpoint_window_at_offset_zero_is_exactly_empty() {
    let now = Instant::now();
    let mut framer = framer(&TestSettings::default(), now);
    let outputs = feed(&mut framer, b"abc\n", now);
    let record = outputs.first().expect("newline framing must emit a record");
    assert_eq!(record.checkpoint_end, 4);
    assert_eq!(record.checkpoint_window.bytes(), b"abc\n");
    assert_eq!(
        record.checkpoint_window.guard().unwrap(),
        CommittedFrontierGuard::compute(4, b"abc\n").unwrap()
    );
}
