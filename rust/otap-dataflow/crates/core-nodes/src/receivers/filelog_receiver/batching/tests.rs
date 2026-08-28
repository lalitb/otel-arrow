// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Array, UInt16Array};
use otap_df_pdata::{
    otlp::{ProtoBuffer, ProtoBytesEncoder, logs::LogsProtoBytesEncoder},
    proto::opentelemetry::{
        collector::logs::v1::ExportLogsServiceRequest,
        common::v1::{KeyValue, any_value::Value},
        logs::v1::LogRecord,
    },
};
use prost::Message;

use super::*;
use crate::receivers::filelog_receiver::config::{
    configured_logical_record_size, logical_bool_value_len, logical_int_value_len,
    logical_string_value_len,
};
use crate::receivers::filelog_receiver::framing::SourceRange;

fn file_id(seed: u64) -> FileId {
    let mut bytes = [0; 16];
    bytes[8..].copy_from_slice(&seed.to_be_bytes());
    FileId(bytes)
}

fn settings(max_records: u32, max_bytes: u64, max_flush_period: Duration) -> BatchSettings {
    BatchSettings::for_test(
        BatchConfig {
            max_records,
            max_bytes,
            max_flush_period,
        },
        MetadataConfig::default(),
        MaxLogSizeBehavior::Split,
        OnDecodeError::PreserveRaw,
    )
}

fn batch_with_settings(settings: BatchSettings) -> OpenBatch {
    OpenBatch::from_settings(settings).expect("test settings are valid")
}

fn test_window(end_offset: u64) -> CommittedFrontierWindow {
    let window_len = end_offset.min(64) as usize;
    CommittedFrontierWindow::new(end_offset, vec![0u8; window_len]).unwrap()
}

fn test_guard(committed_offset: u64) -> CommittedFrontierGuard {
    let window_len = committed_offset.min(64) as usize;
    CommittedFrontierGuard::compute(committed_offset, &vec![0u8; window_len]).unwrap()
}

fn input(seed: u64, start: u64, end: u64, base: u64, ready_at: Instant) -> RecordInput {
    let path = PathBuf::from(format!("/var/log/file-{seed}.log"));
    RecordInput {
        framed: FramedRecord {
            body: FramedBody::Text(format!("record-{seed}-{start}")),
            body_source_range: SourceRange { start, end },
            frame_source_range: SourceRange { start, end },
            checkpoint_end: end,
            resulting_resume: FramingResume::Clean,
            decode_outcome: DecodeOutcome::Clean,
            flush_reason: None,
            fragment: None,
            truncated: false,
            discarded_source_bytes: 0,
            checkpoint_window: test_window(end),
        },
        file_id: file_id(seed),
        progress_base: ProgressBase {
            file_epoch: 1,
            committed_offset: base,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 0,
            committed_frontier_guard: test_guard(base),
        },
        matched_path: path.clone(),
        resolved_path: path,
        observed_time_unix_nano: 100,
        last_seen_time_unix_nano: 100,
        ready_at,
        record_number: Some(start),
    }
}

fn empty_input(seed: u64, start: u64, end: u64, base: u64, ready_at: Instant) -> RecordInput {
    let mut record = input(seed, start, end, base, ready_at);
    record.framed.body = FramedBody::Text(String::new());
    record.framed.body_source_range = SourceRange { start, end: start };
    record
}

fn append(batch: &mut OpenBatch, record: RecordInput) -> Option<SealReason> {
    match batch.try_append(record).expect("append succeeds") {
        BatchAppendOutcome::Appended { seal } => seal,
        BatchAppendOutcome::SealBefore { reason, .. } => {
            panic!("unexpected pre-append seal: {reason:?}")
        }
    }
}

fn decode(batch: &LogicalBatch) -> ExportLogsServiceRequest {
    let mut records = batch.outbound_records();
    let mut encoder = LogsProtoBytesEncoder::new();
    let mut buffer = ProtoBuffer::default();
    encoder
        .encode(&mut records, &mut buffer)
        .expect("OTAP logs encode to OTLP");
    ExportLogsServiceRequest::decode(buffer.as_ref()).expect("OTLP logs decode")
}

fn attr<'a>(record: &'a LogRecord, key: &str) -> Option<&'a Value> {
    record
        .attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
}

fn string_attr<'a>(record: &'a LogRecord, key: &str) -> Option<&'a str> {
    match attr(record, key) {
        Some(Value::StringValue(value)) => Some(value),
        _ => None,
    }
}

fn percent_path(bytes: &[u8]) -> String {
    let mut result = ENCODED_PATH_PREFIX.to_owned();
    for byte in bytes {
        result.push_str(&format!("%{byte:02X}"));
    }
    result
}

/// Scenario: text, exact bytes, and newline-preserving multiline bodies share
/// one file batch with distinct observed timestamps.
/// Guarantees: OTLP round-trip preserves every body, emits exact
/// resource/scope defaults, leaves semantic fields unset, and never exposes
/// file identity or host attributes.
#[test]
fn projects_bodies_defaults_and_scope_without_identity_leakage() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
    let mut text = input(1, 0, 1, 0, now);
    text.framed.body = FramedBody::Text("hello".to_owned());
    text.observed_time_unix_nano = 11;
    let mut bytes = input(1, 1, 2, 0, now);
    bytes.framed.body = FramedBody::Bytes(vec![0xff, 0x00]);
    bytes.observed_time_unix_nano = 12;
    let mut multiline = input(1, 2, 3, 0, now);
    multiline.framed.body = FramedBody::Text("first\nsecond".to_owned());
    multiline.observed_time_unix_nano = 13;

    let _ = append(&mut batch, text);
    let _ = append(&mut batch, bytes);
    let _ = append(&mut batch, multiline);
    let logical = batch.finish().unwrap();
    let request = decode(&logical);

    assert_eq!(request.resource_logs.len(), 1);
    let resource_logs = &request.resource_logs[0];
    assert!(resource_logs.schema_url.is_empty());
    let resource = resource_logs.resource.as_ref().unwrap();
    assert!(resource.attributes.is_empty());
    assert_eq!(resource.dropped_attributes_count, 0);
    assert_eq!(resource_logs.scope_logs.len(), 1);
    let scope_logs = &resource_logs.scope_logs[0];
    assert!(scope_logs.schema_url.is_empty());
    let scope = scope_logs.scope.as_ref().unwrap();
    assert_eq!(scope.name, "otap-df-core-nodes/filelog");
    assert_eq!(scope.version, env!("CARGO_PKG_VERSION"));
    assert!(scope.attributes.is_empty());
    assert_eq!(scope.dropped_attributes_count, 0);

    let records = &scope_logs.log_records;
    assert_eq!(records.len(), 3);
    assert!(matches!(
        records[0].body.as_ref().and_then(|body| body.value.as_ref()),
        Some(Value::StringValue(value)) if value == "hello"
    ));
    assert!(matches!(
        records[1].body.as_ref().and_then(|body| body.value.as_ref()),
        Some(Value::BytesValue(value)) if value == &[0xff, 0x00]
    ));
    assert!(matches!(
        records[2].body.as_ref().and_then(|body| body.value.as_ref()),
        Some(Value::StringValue(value)) if value == "first\nsecond"
    ));
    for (record, observed) in records.iter().zip([11, 12, 13]) {
        assert_eq!(record.time_unix_nano, 0);
        assert_eq!(record.observed_time_unix_nano, observed);
        assert_eq!(record.severity_number, 0);
        assert!(record.severity_text.is_empty());
        assert!(record.trace_id.is_empty());
        assert!(record.span_id.is_empty());
        assert_eq!(record.flags, 0);
        assert!(record.event_name.is_empty());
        assert_eq!(record.dropped_attributes_count, 0);
        assert_eq!(record.attributes.len(), 2);
        assert!(
            record
                .attributes
                .iter()
                .all(|attribute| !attribute.key.contains("file_id")
                    && !attribute.key.starts_with("host."))
        );
    }
}

/// Scenario: a split preserve-raw fragment carries every enabled source and
/// error marker, including a resolved path and an oversize-boundary flush.
/// Guarantees: every conditional key has the specified OTLP type and value,
/// fragment ranges are half-open body ranges, and record number is omitted
/// from every fragment.
#[test]
fn projects_all_split_and_decode_attributes_with_exact_types() {
    let metadata = MetadataConfig {
        include_file_path_resolved: true,
        include_file_record_offset: true,
        include_file_record_number: true,
    };
    let settings = BatchSettings::for_test(
        BatchConfig {
            max_records: 10,
            max_bytes: 1 << 20,
            max_flush_period: Duration::from_secs(1),
        },
        metadata,
        MaxLogSizeBehavior::Split,
        OnDecodeError::PreserveRaw,
    );
    let now = Instant::now();
    let mut record = input(2, 0, 21, 0, now);
    record.matched_path = PathBuf::from("/matched/app.log");
    record.resolved_path = PathBuf::from("/real/app.log");
    record.framed.body = FramedBody::Bytes(b"raw".to_vec());
    record.framed.body_source_range = SourceRange { start: 10, end: 20 };
    let expected_fragment_id = fragment_id(record.file_id, record.progress_base.file_epoch, 10);
    record.framed.fragment = Some(FragmentMetadata {
        id: expected_fragment_id.clone(),
        index: 0,
        last: false,
    });
    record.framed.resulting_resume = FramingResume::Continuation {
        record_start_offset: 10,
        record_end_offset: 0,
        next_fragment_index: 1,
    };
    record.framed.decode_outcome = DecodeOutcome::PreserveRaw { count: 7 };
    record.framed.flush_reason = Some(FlushReason::OversizeLineBoundary);
    record.record_number = Some(99);

    let mut batch = batch_with_settings(settings);
    let _ = append(&mut batch, record);
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];

    assert_eq!(
        string_attr(log, ATTR_KEY_LOG_FILE_PATH),
        Some("/matched/app.log")
    );
    assert_eq!(string_attr(log, ATTR_KEY_LOG_FILE_NAME), Some("app.log"));
    assert_eq!(
        string_attr(log, ATTR_KEY_PATH_RESOLVED),
        Some("/real/app.log")
    );
    assert_eq!(string_attr(log, ATTR_KEY_RECORD_OFFSET), Some("10"));
    assert!(attr(log, ATTR_KEY_RECORD_NUMBER).is_none());
    assert_eq!(
        string_attr(log, ATTR_KEY_FRAGMENT_ID),
        Some(expected_fragment_id.as_str())
    );
    assert!(matches!(
        attr(log, ATTR_KEY_FRAGMENT_INDEX),
        Some(Value::IntValue(0))
    ));
    assert!(matches!(
        attr(log, ATTR_KEY_FRAGMENT_LAST),
        Some(Value::BoolValue(false))
    ));
    assert_eq!(string_attr(log, ATTR_KEY_FRAGMENT_SOURCE_START), Some("10"));
    assert_eq!(string_attr(log, ATTR_KEY_FRAGMENT_SOURCE_END), Some("20"));
    assert_eq!(
        string_attr(log, ATTR_KEY_FLUSH_REASON),
        Some("oversize_line_boundary")
    );
    assert_eq!(
        string_attr(log, ATTR_KEY_DECODE_ERROR_POLICY),
        Some("preserve_raw")
    );
    assert_eq!(string_attr(log, ATTR_KEY_DECODE_ERROR_COUNT), Some("7"));
    assert!(attr(log, ATTR_KEY_RECORD_TRUNCATED).is_none());
}

/// Scenario: a truncated replacement-decoded record enables offset and
/// worker-local number metadata.
/// Guarantees: truncation is Bool, offset/number/error count are decimal
/// Strings, decode policy is `replace`, and discarded-byte count is not
/// exposed.
#[test]
fn projects_truncate_number_and_replacement_evidence() {
    let metadata = MetadataConfig {
        include_file_record_offset: true,
        include_file_record_number: true,
        ..MetadataConfig::default()
    };
    let settings = BatchSettings::for_test(
        BatchConfig {
            max_records: 10,
            max_bytes: 1 << 20,
            max_flush_period: Duration::from_secs(1),
        },
        metadata,
        MaxLogSizeBehavior::Truncate,
        OnDecodeError::Replace,
    );
    let now = Instant::now();
    let mut record = input(3, 0, 8, 0, now);
    record.framed.body = FramedBody::Text("prefix\u{fffd}".to_owned());
    record.framed.body_source_range = SourceRange { start: 5, end: 6 };
    record.framed.truncated = true;
    record.framed.discarded_source_bytes = 2;
    record.framed.decode_outcome = DecodeOutcome::Replacements { count: 4 };
    record.framed.flush_reason = Some(FlushReason::Timeout);
    record.record_number = Some(7);

    let mut batch = batch_with_settings(settings);
    let _ = append(&mut batch, record);
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];

    assert_eq!(string_attr(log, ATTR_KEY_RECORD_OFFSET), Some("5"));
    assert_eq!(string_attr(log, ATTR_KEY_RECORD_NUMBER), Some("7"));
    assert!(matches!(
        attr(log, ATTR_KEY_RECORD_TRUNCATED),
        Some(Value::BoolValue(true))
    ));
    assert_eq!(string_attr(log, ATTR_KEY_FLUSH_REASON), Some("timeout"));
    assert_eq!(
        string_attr(log, ATTR_KEY_DECODE_ERROR_POLICY),
        Some("replace")
    );
    assert_eq!(string_attr(log, ATTR_KEY_DECODE_ERROR_COUNT), Some("4"));
    assert!(
        log.attributes
            .iter()
            .all(|attribute| !attribute.key.contains("discarded"))
    );
}

/// Scenario: a clean unsplit record uses default-disabled metadata.
/// Guarantees: only matched path and file name are emitted; clean decode,
/// resolved path, offsets, numbers, flush, split, and truncate markers are
/// all absent.
#[test]
fn clean_default_projection_omits_every_conditional_attribute() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(2, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, input(4, 0, 1, 0, now));
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];

    assert_eq!(log.attributes.len(), 2);
    assert!(attr(log, ATTR_KEY_PATH_RESOLVED).is_none());
    assert!(attr(log, ATTR_KEY_PATH_ENCODING).is_none());
    assert!(attr(log, ATTR_KEY_RECORD_OFFSET).is_none());
    assert!(attr(log, ATTR_KEY_RECORD_NUMBER).is_none());
    assert!(attr(log, ATTR_KEY_FLUSH_REASON).is_none());
    assert!(attr(log, ATTR_KEY_DECODE_ERROR_POLICY).is_none());
    assert!(attr(log, ATTR_KEY_DECODE_ERROR_COUNT).is_none());
}

/// Scenario: each framing flush reason is projected independently.
/// Guarantees: the public String values are exactly `max_lines`, `timeout`,
/// `oversize_line_boundary`, `rotation`, and `drain`.
#[test]
fn flush_reasons_use_frozen_string_values() {
    let cases = [
        (FlushReason::MaxLines, "max_lines"),
        (FlushReason::Timeout, "timeout"),
        (FlushReason::OversizeLineBoundary, "oversize_line_boundary"),
        (FlushReason::Rotation, "rotation"),
        (FlushReason::Drain, "drain"),
    ];
    let now = Instant::now();
    for (index, (reason, expected)) in cases.into_iter().enumerate() {
        let mut record = input(index as u64 + 10, 0, 1, 0, now);
        record.framed.flush_reason = Some(reason);
        let mut batch = batch_with_settings(settings(1, 1 << 20, Duration::from_secs(1)));
        let _ = append(&mut batch, record);
        let request = decode(&batch.finish().unwrap());
        let log = &request.resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(string_attr(log, ATTR_KEY_FLUSH_REASON), Some(expected));
    }
}

#[cfg(unix)]
/// Scenario: one valid path literally begins with the encoding prefix while
/// another contains invalid UTF-8 bytes.
/// Guarantees: valid UTF-8 remains literal without a discriminator; invalid
/// Unix bytes use uppercase percent encoding for every byte and emit
/// `percent-v1`.
#[test]
fn unix_path_encoding_is_reversible_and_literal_prefix_is_unambiguous() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let now = Instant::now();
    let literal = PathBuf::from("/logs/filelog-percent:%FF.log");
    let mut literal_record = input(20, 0, 1, 0, now);
    literal_record.matched_path = literal.clone();
    literal_record.resolved_path = literal;
    let raw = b"/logs/\xff.log";
    let encoded_path = PathBuf::from(OsString::from_vec(raw.to_vec()));
    let mut encoded_record = input(21, 0, 1, 0, now);
    encoded_record.matched_path = encoded_path.clone();
    encoded_record.resolved_path = encoded_path;

    let mut batch = batch_with_settings(settings(2, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, literal_record);
    let _ = append(&mut batch, encoded_record);
    let request = decode(&batch.finish().unwrap());
    let logs = &request.resource_logs[0].scope_logs[0].log_records;

    assert_eq!(
        string_attr(&logs[0], ATTR_KEY_LOG_FILE_PATH),
        Some("/logs/filelog-percent:%FF.log")
    );
    assert!(attr(&logs[0], ATTR_KEY_PATH_ENCODING).is_none());
    assert_eq!(
        string_attr(&logs[1], ATTR_KEY_LOG_FILE_PATH),
        Some(percent_path(raw).as_str())
    );
    assert_eq!(
        string_attr(&logs[1], ATTR_KEY_LOG_FILE_NAME),
        Some(percent_path(b"\xff.log").as_str())
    );
    assert_eq!(
        string_attr(&logs[1], ATTR_KEY_PATH_ENCODING),
        Some("percent-v1")
    );
}

#[cfg(unix)]
/// Scenario: live Unix path evidence is exactly at and one byte above the
/// durable format's 4,096-byte advisory-path stored maximum.
/// Guarantees: the OTAP provenance attribute always carries the complete
/// live native path losslessly -- the durable stored-byte cap bounds only
/// checkpointed `AdvisoryPath` evidence, never a live provenance attribute
/// -- so both lengths expand to prefix plus three bytes per native byte
/// and succeed.
#[test]
fn unix_path_encoding_is_unbounded_for_live_full_paths() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let now = Instant::now();
    let maximum_raw =
        vec![0xff; super::super::checkpoint::primitives::ADVISORY_PATH_STORED_MAX_BYTES];
    let maximum_path = PathBuf::from(OsString::from_vec(maximum_raw.clone()));
    let mut maximum = empty_input(22, 0, 1, 0, now);
    maximum.matched_path = maximum_path.clone();
    maximum.resolved_path = maximum_path;
    let mut batch = batch_with_settings(settings(1, 64 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, maximum);
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(
        string_attr(log, ATTR_KEY_LOG_FILE_PATH).unwrap().len(),
        ENCODED_PATH_PREFIX.len() + 3 * maximum_raw.len()
    );

    let over_raw =
        vec![0xff; super::super::checkpoint::primitives::ADVISORY_PATH_STORED_MAX_BYTES + 1];
    let over_path = PathBuf::from(OsString::from_vec(over_raw.clone()));
    let mut over = empty_input(23, 0, 1, 0, now);
    over.matched_path = over_path.clone();
    over.resolved_path = over_path;
    let mut batch = batch_with_settings(settings(1, 64 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, over);
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(
        string_attr(log, ATTR_KEY_LOG_FILE_PATH).unwrap().len(),
        ENCODED_PATH_PREFIX.len() + 3 * over_raw.len()
    );
}

#[cfg(windows)]
/// Scenario: a Windows matched path contains an unpaired UTF-16 surrogate.
/// Guarantees: projection percent-encodes every little-endian UTF-16
/// code-unit byte (`AdvisoryPathKind::WindowsUtf16Le`'s native byte order)
/// and marks the String with the `percent-v1` discriminator.
#[test]
fn windows_unpaired_surrogate_uses_utf16le_percent_encoding() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let units = [u16::from(b'C'), u16::from(b':'), u16::from(b'\\'), 0xd800];
    let path = PathBuf::from(OsString::from_wide(&units));
    let mut record = empty_input(24, 0, 1, 0, Instant::now());
    record.matched_path = path.clone();
    record.resolved_path = path;
    let expected: Vec<u8> = units.into_iter().flat_map(u16::to_le_bytes).collect();
    let mut batch = batch_with_settings(settings(1, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, record);
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(
        string_attr(log, ATTR_KEY_LOG_FILE_PATH),
        Some(percent_path(&expected).as_str())
    );
    assert_eq!(string_attr(log, ATTR_KEY_PATH_ENCODING), Some("percent-v1"));
}

#[cfg(windows)]
/// Scenario: unpaired-surrogate Windows path evidence is exactly at and one
/// code unit above the durable format's 4,096-byte advisory-path stored
/// maximum.
/// Guarantees: the OTAP provenance attribute always carries the complete
/// live native path losslessly regardless of the durable stored-byte cap,
/// so 2,048 and 2,049 UTF-16 units both receive the full prefix-plus-`%HH`
/// expansion and succeed.
#[test]
fn windows_path_encoding_is_unbounded_for_live_full_paths() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let now = Instant::now();
    let units = vec![0xd800; 2_048];
    let path = PathBuf::from(OsString::from_wide(&units));
    let mut maximum = empty_input(25, 0, 1, 0, now);
    maximum.matched_path = path.clone();
    maximum.resolved_path = path;
    let mut batch = batch_with_settings(settings(1, 64 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, maximum);
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(
        string_attr(log, ATTR_KEY_LOG_FILE_PATH).unwrap().len(),
        ENCODED_PATH_PREFIX.len() + 3 * 4_096
    );

    let over_units = vec![0xd800; 2_049];
    let over_path = PathBuf::from(OsString::from_wide(&over_units));
    let mut over = empty_input(26, 0, 1, 0, now);
    over.matched_path = over_path.clone();
    over.resolved_path = over_path;
    let mut batch = batch_with_settings(settings(1, 64 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, over);
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(
        string_attr(log, ATTR_KEY_LOG_FILE_PATH).unwrap().len(),
        ENCODED_PATH_PREFIX.len() + 3 * 4_098
    );
}

/// Scenario: a fully populated split projection is sized both before Arrow
/// append and after OTLP round-trip.
/// Guarantees: enumerating projected key/value logical lengths reproduces
/// the batch's exact runtime sum, and the shared configuration worst case
/// covers it.
#[test]
fn runtime_attribute_enumeration_matches_shared_logical_size() {
    let metadata = MetadataConfig {
        include_file_path_resolved: true,
        include_file_record_offset: true,
        include_file_record_number: true,
    };
    let batch_config = BatchConfig {
        max_records: 10,
        max_bytes: 1 << 20,
        max_flush_period: Duration::from_secs(1),
    };
    let settings = BatchSettings::for_test(
        batch_config,
        metadata,
        MaxLogSizeBehavior::Split,
        OnDecodeError::PreserveRaw,
    );
    let mut record = input(30, 10, 14, 10, Instant::now());
    record.framed.body = FramedBody::Bytes(b"body".to_vec());
    record.progress_base.framing_resume = FramingResume::Continuation {
        record_start_offset: 0,
        record_end_offset: 0,
        next_fragment_index: u32::MAX,
    };
    record.framed.fragment = Some(FragmentMetadata {
        id: fragment_id(record.file_id, record.progress_base.file_epoch, 0),
        index: u32::MAX,
        last: true,
    });
    record.framed.decode_outcome = DecodeOutcome::PreserveRaw { count: u64::MAX };
    record.framed.flush_reason = Some(FlushReason::OversizeLineBoundary);

    let mut batch = batch_with_settings(settings);
    let _ = append(&mut batch, record);
    let runtime_size = batch.logical_bytes();
    let logical = batch.finish().unwrap();
    let request = decode(&logical);
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];
    let mut sizes = Vec::new();
    for KeyValue { key, value } in &log.attributes {
        let value = value
            .as_ref()
            .and_then(|value| value.value.as_ref())
            .expect("filelog attributes always have values");
        let value_bytes = match value {
            Value::StringValue(value) => logical_string_value_len(value).unwrap(),
            Value::IntValue(value) => logical_int_value_len(*value),
            Value::BoolValue(value) => logical_bool_value_len(*value),
            other => panic!("unexpected projected attribute type: {other:?}"),
        };
        sizes.push(LogicalAttributeSize::new(key, value_bytes).unwrap());
    }
    let enumerated = checked_logical_record_size(4, sizes).unwrap();
    assert_eq!(runtime_size, enumerated);

    let configured = configured_logical_record_size(
        4,
        &metadata,
        MaxLogSizeBehavior::Split,
        OnDecodeError::PreserveRaw,
    )
    .unwrap();
    assert!(configured >= runtime_size);
}

/// Scenario: a practical three-record batch and the two edges of the `u16`
/// log-ID domain are inspected directly.
/// Guarantees: IDs are zero-based, count remains `u32`, ID 65,534 is valid,
/// and count 65,535 cannot be converted into another record ID.
#[test]
fn log_ids_are_zero_based_with_explicit_u16_cap_logic() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(3, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, input(31, 0, 1, 0, now));
    let _ = append(&mut batch, input(31, 1, 2, 0, now));
    assert_eq!(
        append(&mut batch, input(31, 2, 3, 0, now)),
        Some(SealReason::RecordCount)
    );
    assert_eq!(batch.record_count(), 3u32);
    let logical = batch.finish().unwrap();
    let root = logical.records().get(ArrowPayloadType::Logs).unwrap();
    let ids = root
        .column_by_name("id")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt16Array>()
        .unwrap();
    assert_eq!(
        (0..ids.len())
            .map(|index| ids.value(index))
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(log_id_for_count(0).unwrap(), 0);
    assert_eq!(log_id_for_count(65_534).unwrap(), 65_534);
    assert!(matches!(
        log_id_for_count(65_535),
        Err(BatchError::InvalidLogId {
            record_count: 65_535
        })
    ));
}

/// Scenario: record-count settings and append attempts are below, exactly at,
/// and above their bound.
/// Guarantees: invalid configured caps fail, the exact record seals after
/// append, and a later record is returned unchanged.
#[test]
fn record_count_bound_accepts_exact_and_refuses_above() {
    assert!(matches!(
        OpenBatch::from_settings(settings(0, 1 << 20, Duration::from_secs(1))),
        Err(BatchError::InvalidSettings { .. })
    ));
    assert!(matches!(
        OpenBatch::from_settings(settings(65_536, 1 << 20, Duration::from_secs(1))),
        Err(BatchError::InvalidSettings { .. })
    ));

    let now = Instant::now();
    let mut batch = batch_with_settings(settings(2, 1 << 20, Duration::from_secs(1)));
    assert_eq!(append(&mut batch, input(32, 0, 1, 0, now)), None);
    assert_eq!(
        append(&mut batch, input(32, 1, 2, 0, now)),
        Some(SealReason::RecordCount)
    );
    let refused = input(32, 2, 3, 0, now);
    let expected = refused.clone();
    assert_eq!(
        batch.try_append(refused).unwrap(),
        BatchAppendOutcome::SealBefore {
            record: expected,
            reason: SealReason::RecordCount
        }
    );
}

fn one_record_logical_size(record: RecordInput) -> u64 {
    let mut probe = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut probe, record);
    probe.logical_bytes()
}

/// Scenario: one record and a two-record batch exercise byte budgets one byte
/// below, exactly at, and above their exact logical sums.
/// Guarantees: a single oversize record is terminal, equality is accepted
/// and seals, and a nonempty batch returns an overflowing next record
/// unchanged.
#[test]
fn logical_byte_bound_handles_below_equal_and_above_exactly() {
    let now = Instant::now();
    let first = input(33, 0, 1, 0, now);
    let size = one_record_logical_size(first.clone());

    let mut too_small = batch_with_settings(settings(10, size - 1, Duration::from_secs(1)));
    assert!(matches!(
        too_small.try_append(first.clone()),
        Err(BatchError::RecordTooLarge {
            logical_bytes,
            max_bytes
        }) if logical_bytes == size && max_bytes == size - 1
    ));
    assert_eq!(too_small.record_count(), 0);

    let mut exact = batch_with_settings(settings(10, size, Duration::from_secs(1)));
    assert_eq!(
        append(&mut exact, first.clone()),
        Some(SealReason::LogicalBytes)
    );

    let second = input(33, 1, 2, 0, now);
    let mut below_two = batch_with_settings(settings(10, size * 2 - 1, Duration::from_secs(1)));
    let _ = append(&mut below_two, first.clone());
    let expected = second.clone();
    assert_eq!(
        below_two.try_append(second.clone()).unwrap(),
        BatchAppendOutcome::SealBefore {
            record: expected,
            reason: SealReason::LogicalBytes
        }
    );

    let mut exact_two = batch_with_settings(settings(10, size * 2, Duration::from_secs(1)));
    let _ = append(&mut exact_two, first);
    assert_eq!(
        append(&mut exact_two, second),
        Some(SealReason::LogicalBytes)
    );
}

/// Scenario: an empty-body log's exact logical size equals the whole batch
/// byte budget.
/// Guarantees: an empty body is still a real log record, is accepted and
/// sealed at equality, and no empty logical batch is emitted.
#[test]
fn empty_body_at_exact_byte_limit_is_a_nonempty_batch() {
    let now = Instant::now();
    let record = empty_input(34, 0, 1, 0, now);
    let size = one_record_logical_size(record.clone());
    let mut batch = batch_with_settings(settings(10, size, Duration::from_secs(1)));
    assert_eq!(append(&mut batch, record), Some(SealReason::LogicalBytes));
    let logical = batch.finish().unwrap();
    assert_eq!(logical.record_count(), 1);

    let empty = batch_with_settings(settings(10, size, Duration::from_secs(1)));
    assert!(matches!(empty.finish(), Err(BatchError::EmptyBatch)));
}

/// Scenario: one sparse batch receives no later input, then a second record
/// becomes ready exactly at its first-record deadline.
/// Guarantees: the deadline never rearms, idle polling reports due without a
/// sleep, and the exact-deadline record is returned unchanged.
#[test]
fn first_record_deadline_flushes_idle_and_refuses_exact_deadline() {
    let start = Instant::now();
    let period = Duration::from_secs(10);
    let mut batch = batch_with_settings(settings(10, 1 << 20, period));
    assert_eq!(batch.deadline(), None);
    assert!(!batch.is_flush_due(start + period));
    let _ = append(&mut batch, input(35, 0, 1, 0, start));
    assert_eq!(batch.deadline(), Some(start + period));
    assert!(!batch.is_flush_due(start + period - Duration::from_nanos(1)));
    assert!(batch.is_flush_due(start + period));

    let refused = input(35, 1, 2, 0, start + period);
    let expected = refused.clone();
    assert_eq!(
        batch.try_append(refused).unwrap(),
        BatchAppendOutcome::SealBefore {
            record: expected,
            reason: SealReason::Deadline
        }
    );
    assert_eq!(batch.deadline(), Some(start + period));

    let mut late_batch = batch_with_settings(settings(10, 1 << 20, period));
    let _ = append(&mut late_batch, input(135, 0, 1, 0, start));
    assert!(matches!(
        late_batch
            .try_append(input(
                135,
                1,
                2,
                0,
                start + period + Duration::from_nanos(1)
            ))
            .unwrap(),
        BatchAppendOutcome::SealBefore {
            reason: SealReason::Deadline,
            ..
        }
    ));
}

/// Scenario: a first record uses an unrepresentable deadline and later
/// records move the worker clock backward.
/// Guarantees: Instant overflow and nonmonotonic readiness are terminal and
/// neither attempt mutates Arrow or batch counters.
#[test]
fn deadline_overflow_and_nonmonotonic_ready_time_fail_closed() {
    let now = Instant::now();
    let mut overflow = batch_with_settings(settings(10, 1 << 20, Duration::MAX));
    assert!(matches!(
        overflow.try_append(input(36, 0, 1, 0, now)),
        Err(BatchError::DeadlineOverflow)
    ));
    assert_eq!(overflow.record_count(), 0);

    let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(10)));
    let _ = append(&mut batch, input(36, 0, 1, 0, now + Duration::from_secs(2)));
    assert!(matches!(
        batch.try_append(input(36, 1, 2, 0, now + Duration::from_secs(1))),
        Err(BatchError::NonMonotonicReadyAt)
    ));
    assert_eq!(batch.record_count(), 1);
}

/// Scenario: observed time is negative and then the greatest accepted `i64`,
/// while last-seen metadata independently uses `u64::MAX`.
/// Guarantees: timestamp-domain validation rejects negative values, preserves
/// the accepted observed time, and does not conflate it with last-seen time.
#[test]
fn observed_and_last_seen_timestamp_domains_are_independent() {
    let now = Instant::now();
    let mut negative = input(37, 0, 1, 0, now);
    negative.observed_time_unix_nano = -1;
    let mut batch = batch_with_settings(settings(2, 1 << 20, Duration::from_secs(1)));
    assert!(matches!(
        batch.try_append(negative),
        Err(BatchError::InvalidRecord { .. })
    ));
    assert_eq!(batch.record_count(), 0);

    let mut maximum = input(37, 0, 1, 0, now);
    maximum.observed_time_unix_nano = i64::MAX;
    maximum.last_seen_time_unix_nano = u64::MAX;
    let _ = append(&mut batch, maximum);
    let logical = batch.finish().unwrap();
    assert_eq!(logical.deltas()[0].last_seen_time_unix_nano(), u64::MAX);
    let request = decode(&logical);
    assert_eq!(
        request.resource_logs[0].scope_logs[0].log_records[0].observed_time_unix_nano,
        i64::MAX as u64
    );
}

/// Scenario: A1, B1, and A2 are interleaved while A's later last-seen value
/// is lower than its first.
/// Guarantees: file order may interleave, same-file progress merges
/// contiguously, the first durable base is preserved, and last-seen uses a
/// monotonic maximum.
#[test]
fn interleaved_files_merge_contiguous_progress_and_preserve_base() {
    let now = Instant::now();
    let mut a1 = input(40, 0, 1, 0, now);
    a1.last_seen_time_unix_nano = 50;
    let fragment_id = fragment_id(a1.file_id, a1.progress_base.file_epoch, 0);
    a1.framed.fragment = Some(FragmentMetadata {
        id: fragment_id.clone(),
        index: 0,
        last: false,
    });
    a1.framed.resulting_resume = FramingResume::Continuation {
        record_start_offset: 0,
        record_end_offset: 0,
        next_fragment_index: 1,
    };
    let mut b1 = input(41, 0, 1, 0, now);
    b1.last_seen_time_unix_nano = 30;
    let mut a2 = input(40, 1, 2, 0, now);
    a2.last_seen_time_unix_nano = 10;
    a2.framed.fragment = Some(FragmentMetadata {
        id: fragment_id,
        index: 1,
        last: false,
    });
    a2.framed.resulting_resume = FramingResume::Continuation {
        record_start_offset: 0,
        record_end_offset: 0,
        next_fragment_index: 2,
    };

    let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, a1);
    let _ = append(&mut batch, b1);
    let _ = append(&mut batch, a2);
    let logical = batch.finish().unwrap();
    assert_eq!(logical.record_count(), 3);
    assert_eq!(logical.deltas().len(), 2);
    let a = &logical.deltas()[0];
    assert_eq!(a.file_id(), file_id(40));
    assert_eq!(a.expected_file_epoch(), 1);
    assert_eq!(a.expected_committed_offset(), 0);
    assert_eq!(a.expected_framing_resume(), FramingResume::Clean);
    assert_eq!(a.final_offset(), 2);
    assert_eq!(
        a.final_framing_resume(),
        FramingResume::Continuation {
            record_start_offset: 0,
            record_end_offset: 0,
            next_fragment_index: 2
        }
    );
    assert_eq!(a.last_seen_time_unix_nano(), 50);
    let update = a
        .to_update_progress(ProgressBase {
            file_epoch: 1,
            committed_offset: 0,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 75,
            committed_frontier_guard: test_guard(0),
        })
        .unwrap();
    assert_eq!(update.expected_committed_offset, 0);
    assert_eq!(update.new_committed_offset, 2);
    assert_eq!(update.new_last_seen_time_unix_nano, 75);
}

/// Scenario: first-frame base, checkpoint end, later gap, and later overlap
/// each violate one contiguous-progress rule.
/// Guarantees: every malformed frontier is rejected before a log or delta is
/// appended.
#[test]
fn progress_rejects_first_base_checkpoint_gap_and_overlap() {
    let now = Instant::now();
    let mut wrong_base = input(42, 1, 2, 0, now);
    let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
    assert!(matches!(
        batch.try_append(wrong_base.clone()),
        Err(BatchError::InvalidProgress { .. })
    ));
    wrong_base.progress_base.committed_offset = 1;
    wrong_base.framed.checkpoint_end = 3;
    assert!(matches!(
        batch.try_append(wrong_base),
        Err(BatchError::InvalidProgress { .. })
    ));

    let _ = append(&mut batch, input(42, 0, 1, 0, now));
    assert!(matches!(
        batch.try_append(input(42, 2, 3, 0, now)),
        Err(BatchError::InvalidProgress { .. })
    ));
    assert!(matches!(
        batch.try_append(input(42, 0, 2, 0, now)),
        Err(BatchError::InvalidProgress { .. })
    ));
    assert_eq!(batch.record_count(), 1);
}

/// Scenario: later same-file records change epoch, base offset, and base
/// framing resume.
/// Guarantees: one batch delta remains tied to the first record's exact
/// epoch and durable offset/resume pair.
#[test]
fn progress_rejects_epoch_and_durable_base_changes() {
    let now = Instant::now();
    let first = input(43, 0, 1, 0, now);
    for mutation in 0..3 {
        let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
        let _ = append(&mut batch, first.clone());
        let mut later = input(43, 1, 2, 0, now);
        match mutation {
            0 => later.progress_base.file_epoch = 2,
            1 => later.progress_base.committed_offset = 1,
            2 => {
                later.progress_base.framing_resume = FramingResume::Continuation {
                    record_start_offset: 0,
                    record_end_offset: 0,
                    next_fragment_index: 1,
                };
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            batch.try_append(later),
            Err(BatchError::InvalidProgress { .. })
        ));
        assert_eq!(batch.record_count(), 1);
    }
}

/// Scenario: split metadata carries a wrong stable ID, skips its expected
/// index, disagrees with its resulting resume, or follows continuation with
/// an unsplit record.
/// Guarantees: every impossible split/resume transition is rejected before
/// Arrow or progress mutation.
#[test]
fn fragment_metadata_must_match_durable_resume_transitions() {
    let now = Instant::now();

    let mut wrong_id = input(143, 0, 1, 0, now);
    wrong_id.framed.fragment = Some(FragmentMetadata {
        id: "0".repeat(64),
        index: 0,
        last: true,
    });
    let mut batch = batch_with_settings(settings(2, 1 << 20, Duration::from_secs(1)));
    assert!(matches!(
        batch.try_append(wrong_id),
        Err(BatchError::InvalidRecord { .. })
    ));
    assert_eq!(batch.record_count(), 0);

    let mut skipped = input(143, 0, 1, 0, now);
    skipped.framed.fragment = Some(FragmentMetadata {
        id: fragment_id(skipped.file_id, skipped.progress_base.file_epoch, 0),
        index: 1,
        last: true,
    });
    assert!(matches!(
        batch.try_append(skipped),
        Err(BatchError::InvalidProgress { .. })
    ));

    let mut wrong_resume = input(143, 0, 1, 0, now);
    wrong_resume.framed.fragment = Some(FragmentMetadata {
        id: fragment_id(
            wrong_resume.file_id,
            wrong_resume.progress_base.file_epoch,
            0,
        ),
        index: 0,
        last: false,
    });
    assert!(matches!(
        batch.try_append(wrong_resume),
        Err(BatchError::InvalidProgress { .. })
    ));

    let mut missing_fragment = input(143, 1, 2, 1, now);
    missing_fragment.progress_base.framing_resume = FramingResume::Continuation {
        record_start_offset: 0,
        record_end_offset: 0,
        next_fragment_index: 1,
    };
    assert!(matches!(
        batch.try_append(missing_fragment),
        Err(BatchError::InvalidProgress { .. })
    ));
    assert_eq!(batch.record_count(), 0);
}

/// Scenario: an Ack conversion observes a durable epoch, offset, resume, and
/// last-seen value after the batch was built.
/// Guarantees: stale progress state cannot produce a checkpoint operation,
/// while a matching base preserves newer durable last-seen metadata.
#[test]
fn progress_conversion_revalidates_current_durable_state() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(1, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, input(144, 0, 1, 0, now));
    let logical = batch.finish().unwrap();
    let delta = &logical.deltas()[0];
    for stale in [
        ProgressBase {
            file_epoch: 2,
            committed_offset: 0,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 0,
            committed_frontier_guard: test_guard(0),
        },
        ProgressBase {
            file_epoch: 1,
            committed_offset: 1,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 0,
            committed_frontier_guard: test_guard(1),
        },
        ProgressBase {
            file_epoch: 1,
            committed_offset: 0,
            framing_resume: FramingResume::Continuation {
                record_start_offset: 0,
                record_end_offset: 0,
                next_fragment_index: 1,
            },
            last_seen_time_unix_nano: 0,
            committed_frontier_guard: test_guard(0),
        },
    ] {
        assert!(matches!(
            delta.to_update_progress(stale),
            Err(BatchError::InvalidProgress { .. })
        ));
    }

    let update = delta
        .to_update_progress(ProgressBase {
            file_epoch: 1,
            committed_offset: 0,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 500,
            committed_frontier_guard: test_guard(0),
        })
        .unwrap();
    assert_eq!(update.new_last_seen_time_unix_nano, 500);
}

/// Scenario: exactly 4,096 distinct files contribute one tiny record each,
/// followed by a 4,097th file.
/// Guarantees: the exact WAL operation cap requests immediate seal after the
/// accepted record, is never split, and returns the next owned record
/// unchanged.
#[test]
fn exact_wal_delta_cap_seals_without_splitting_transaction() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(5_000, 128 << 20, Duration::from_secs(1)));
    for seed in 0..MAX_DISTINCT_DELTAS as u64 {
        let seal = append(&mut batch, empty_input(seed + 1_000, 0, 1, 0, now));
        if seed + 1 == MAX_DISTINCT_DELTAS as u64 {
            assert_eq!(seal, Some(SealReason::DistinctFiles));
        } else {
            assert_eq!(seal, None);
        }
    }
    assert_eq!(batch.deltas.len(), MAX_DISTINCT_DELTAS);

    let refused = empty_input(9_999, 0, 1, 0, now);
    let expected = refused.clone();
    assert_eq!(
        batch.try_append(refused).unwrap(),
        BatchAppendOutcome::SealBefore {
            record: expected,
            reason: SealReason::DistinctFiles
        }
    );
}

/// Scenario: one file's batch delta is built from a single record, then a
/// second same-file record extends it (a "batch retry" reconstructing the
/// delta from a later, higher-offset frame).
/// Guarantees: `final_window` always exposes the exact window owned by the
/// most recently merged record, never a stale or first-record window, so a
/// caller installing it onto the reader always retains the latest real
/// evidence.
#[test]
fn final_window_tracks_the_most_recently_merged_record() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
    let first = input(60, 0, 1, 0, now);
    let first_window = first.framed.checkpoint_window.clone();
    let _ = append(&mut batch, first);
    assert_eq!(batch.deltas[0].final_window(), Some(&first_window));

    let second = input(60, 1, 2, 0, now);
    let second_window = second.framed.checkpoint_window.clone();
    assert_ne!(first_window, second_window);
    let _ = append(&mut batch, second);
    assert_eq!(batch.deltas[0].final_window(), Some(&second_window));
}

/// Scenario: a recordless (zero-delta) finalization is converted through
/// `to_update_progress`.
/// Guarantees: `final_window` is `None`, so a caller never replaces the
/// reader's already-retained committed-frontier window; the resulting
/// checkpoint operation reuses the durable guard bit-for-bit.
#[test]
fn zero_delta_finalize_exposes_no_window_to_install() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, input(61, 0, 1, 0, now));

    let direct = batch
        .finalize_file(
            file_id(62),
            ProgressFrontier {
                file_epoch: 1,
                offset: 5,
                framing_resume: FramingResume::Clean,
            },
            10,
        )
        .unwrap();
    let FinalizationOutcome::Direct(delta) = direct else {
        panic!("file without a batch record must return direct finalization")
    };
    assert_eq!(delta.final_window(), None);
}

/// Scenario: rotation finalization targets a file with an existing batch
/// delta and a file with no record in the batch.
/// Guarantees: matching existing progress merges finalization and rejects
/// later appends; absent progress returns a direct same-frontier operation
/// without making an empty OTAP batch.
#[test]
fn recordless_finalization_merges_or_returns_direct_delta() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, input(44, 0, 1, 0, now));
    assert_eq!(
        batch
            .finalize_file(
                file_id(44),
                ProgressFrontier {
                    file_epoch: 1,
                    offset: 1,
                    framing_resume: FramingResume::Clean
                },
                200
            )
            .unwrap(),
        FinalizationOutcome::Merged
    );
    assert!(batch.deltas[0].finalize());
    assert_eq!(batch.deltas[0].last_seen_time_unix_nano(), 200);
    assert!(matches!(
        batch.try_append(input(44, 1, 2, 0, now)),
        Err(BatchError::InvalidProgress { .. })
    ));

    let direct = batch
        .finalize_file(
            file_id(45),
            ProgressFrontier {
                file_epoch: 7,
                offset: 99,
                framing_resume: FramingResume::Continuation {
                    record_start_offset: 90,
                    record_end_offset: 0,
                    next_fragment_index: 3,
                },
            },
            300,
        )
        .unwrap();
    let FinalizationOutcome::Direct(delta) = direct else {
        panic!("file without a batch record must return direct finalization")
    };
    assert_eq!(delta.expected_committed_offset(), 99);
    assert_eq!(delta.final_offset(), 99);
    assert!(delta.finalize());
    let update = delta
        .to_update_progress(ProgressBase {
            file_epoch: 7,
            committed_offset: 99,
            framing_resume: FramingResume::Continuation {
                record_start_offset: 90,
                record_end_offset: 0,
                next_fragment_index: 3,
            },
            last_seen_time_unix_nano: 350,
            committed_frontier_guard: test_guard(99),
        })
        .unwrap();
    assert_eq!(
        update.expected_committed_offset,
        update.new_committed_offset
    );
    assert_eq!(update.new_last_seen_time_unix_nano, 350);
    assert!(update.finalize);
    // A recordless (zero-delta) finalize reuses the durable guard verbatim
    // instead of recomputing one, since the offset does not change.
    assert_eq!(update.new_committed_frontier_guard, test_guard(99));

    let empty = batch_with_settings(settings(1, 1 << 20, Duration::from_secs(1)));
    assert!(matches!(empty.finish(), Err(BatchError::EmptyBatch)));
}

/// Scenario: merged finalization supplies the wrong epoch and then the wrong
/// offset/resume frontier.
/// Guarantees: lifecycle state cannot be attached to a stale or speculative
/// frontier.
#[test]
fn merged_finalization_requires_exact_epoch_and_frontier() {
    let now = Instant::now();
    let mut batch = batch_with_settings(settings(10, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut batch, input(46, 0, 1, 0, now));
    assert!(matches!(
        batch.finalize_file(
            file_id(46),
            ProgressFrontier {
                file_epoch: 2,
                offset: 1,
                framing_resume: FramingResume::Clean
            },
            1
        ),
        Err(BatchError::InvalidProgress { .. })
    ));
    assert!(matches!(
        batch.finalize_file(
            file_id(46),
            ProgressFrontier {
                file_epoch: 1,
                offset: 0,
                framing_resume: FramingResume::Clean
            },
            1
        ),
        Err(BatchError::InvalidProgress { .. })
    ));
    assert!(!batch.deltas[0].finalize());
}

/// Scenario: transactional numbering accepts unsplit records, a fragment
/// sequence, and a later unsplit record in one epoch.
/// Guarantees: unsplit records receive zero-based values, fragment zero
/// advances once, continued fragments do not, all fragments omit numbers,
/// and stale reservations cannot double-commit.
#[test]
fn record_numbers_handle_fragments_and_transactional_commit() {
    let mut table = RecordNumberTable::new(2).unwrap();
    let first = table.prepare(file_id(50), 1, None).unwrap();
    assert_eq!(first.record_number(), Some(0));
    assert_eq!(table.commit(first).unwrap(), Some(0));
    assert!(matches!(
        table.commit(first),
        Err(BatchError::StaleRecordNumberReservation { .. })
    ));

    let fragment_zero = table.prepare(file_id(50), 1, Some(0)).unwrap();
    assert_eq!(fragment_zero.record_number(), None);
    assert_eq!(table.commit(fragment_zero).unwrap(), None);
    let continuation = table.prepare(file_id(50), 1, Some(1)).unwrap();
    assert_eq!(continuation.record_number(), None);
    assert_eq!(table.commit(continuation).unwrap(), None);
    let next = table.prepare(file_id(50), 1, None).unwrap();
    assert_eq!(next.record_number(), Some(2));
    assert_eq!(table.commit(next).unwrap(), Some(2));
}

/// Scenario: a descriptor-equivalent pause leaves numbering state alive,
/// then epoch change and process recovery create new number domains.
/// Guarantees: descriptor lifecycle does not reset a file, epoch change does
/// reset it to zero, a fresh helper resets all files, and removing a tracked
/// identity releases its bounded capacity.
#[test]
fn record_numbers_survive_descriptor_lifecycle_but_reset_epoch_and_process() {
    let mut table = RecordNumberTable::new(1).unwrap();
    let first = table.prepare(file_id(51), 1, None).unwrap();
    let _ = table.commit(first).unwrap();
    let after_descriptor_close = table.prepare(file_id(51), 1, None).unwrap();
    assert_eq!(after_descriptor_close.record_number(), Some(1));
    let _ = table.commit(after_descriptor_close).unwrap();

    let new_epoch = table.prepare(file_id(51), 2, None).unwrap();
    assert_eq!(new_epoch.record_number(), Some(0));
    let _ = table.commit(new_epoch).unwrap();
    assert!(matches!(
        table.prepare(file_id(52), 1, None),
        Err(BatchError::RecordNumberCapacityExhausted { max_files: 1 })
    ));
    assert!(table.remove(file_id(51)));
    assert!(!table.remove(file_id(51)));
    assert_eq!(
        table.prepare(file_id(52), 1, None).unwrap().record_number(),
        Some(0)
    );

    let recovered = RecordNumberTable::new(1).unwrap();
    assert_eq!(
        recovered
            .prepare(file_id(51), 2, None)
            .unwrap()
            .record_number(),
        Some(0)
    );
}

/// Scenario: a finished logical batch is cloned for retained and outbound
/// use.
/// Guarantees: Arrow array Arcs and progress-delta Arc storage are shared;
/// cloning does not duplicate buffers or copy the delta vector.
#[test]
fn logical_batch_clone_and_outbound_view_are_shallow() {
    let now = Instant::now();
    let mut open = batch_with_settings(settings(2, 1 << 20, Duration::from_secs(1)));
    let _ = append(&mut open, input(60, 0, 1, 0, now));
    let batch = open.finish().unwrap();
    let clone = batch.clone();
    let outbound = batch.outbound_records();

    let original_root = batch.records().get(ArrowPayloadType::Logs).unwrap();
    let clone_root = clone.records().get(ArrowPayloadType::Logs).unwrap();
    let outbound_root = outbound.get(ArrowPayloadType::Logs).unwrap();
    assert!(Arc::ptr_eq(original_root.column(0), clone_root.column(0)));
    assert!(Arc::ptr_eq(
        original_root.column(0),
        outbound_root.column(0)
    ));
    let original_deltas = batch.shared_deltas();
    let clone_deltas = clone.shared_deltas();
    assert!(Arc::ptr_eq(&original_deltas, &clone_deltas));
}

/// Scenario: enabled record-number metadata is absent on an unsplit worker
/// input and present on a split input.
/// Guarantees: missing required unsplit numbering is rejected, while a
/// speculative fragment number is ignored so no fragment emits it.
#[test]
fn enabled_record_number_requires_unsplit_input_and_omits_fragments() {
    let metadata = MetadataConfig {
        include_file_record_number: true,
        ..MetadataConfig::default()
    };
    let settings = BatchSettings::for_test(
        BatchConfig {
            max_records: 2,
            max_bytes: 1 << 20,
            max_flush_period: Duration::from_secs(1),
        },
        metadata,
        MaxLogSizeBehavior::Split,
        OnDecodeError::PreserveRaw,
    );
    let now = Instant::now();
    let mut missing = input(61, 0, 1, 0, now);
    missing.record_number = None;
    let mut batch = batch_with_settings(settings.clone());
    assert!(matches!(
        batch.try_append(missing),
        Err(BatchError::InvalidRecord { .. })
    ));

    let mut fragment = input(61, 0, 1, 0, now);
    fragment.framed.fragment = Some(FragmentMetadata {
        id: fragment_id(
            fragment.file_id,
            fragment.progress_base.file_epoch,
            fragment.framed.body_source_range.start,
        ),
        index: 0,
        last: true,
    });
    fragment.record_number = Some(123);
    let mut batch = batch_with_settings(settings);
    let _ = append(&mut batch, fragment);
    let request = decode(&batch.finish().unwrap());
    let log = &request.resource_logs[0].scope_logs[0].log_records[0];
    assert!(attr(log, ATTR_KEY_RECORD_NUMBER).is_none());
}
