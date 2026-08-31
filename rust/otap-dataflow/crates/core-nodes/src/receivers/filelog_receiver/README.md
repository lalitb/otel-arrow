# Filelog Receiver

<!-- markdownlint-disable MD013 -->

## Metadata

- Type: `receiver:filelog` (`urn:otel:receiver:filelog`)
- Feature gate: Default
- Stability: Experimental

## Overview

The filelog receiver tails local regular files and emits OTAP log records. It
provides bounded discovery, decoding, multiline framing, batching, descriptor
use, retry state, and checkpoint storage. Source progress is persisted only
after the matching downstream Ack, so a restart does not intentionally skip
unacknowledged bytes.

Phase 1 is a single-instance source. It serializes one checkpoint namespace and
prevents overlapping file ownership inside one engine process, but it does not
coordinate independent engine processes or provide distributed fencing.

## Getting Started

Create the example directory and one log file:

```console
mkdir -p /tmp/otel-arrow-filelog
printf 'first line\n' >> /tmp/otel-arrow-filelog/app.log
```

Run the checked-in example with one engine core:

```console
cargo run --bin df_engine -- \
  --config configs/filelog-console.yaml \
  --num-cores 1
```

Append another line while the engine is running:

```console
printf 'second line\n' >> /tmp/otel-arrow-filelog/app.log
```

The console exporter Acks each printed batch. The receiver then advances the
durable checkpoint under `.otap-state/filelog/filelog-console/`.

## Configuration

A minimal node configuration is:

```yaml
type: receiver:filelog
config:
  include: ["/var/log/app/*.log"]
  start_at: end
  checkpoint:
    id: app-logs
```

`include` is required and accepts glob patterns. Excludes take precedence over
includes. A durable checkpoint always takes precedence over `start_at`.

The complete Phase 1 shape is:

```yaml
type: receiver:filelog
config:
  include: ["/var/log/app/*.log"]
  exclude: []
  recursive: true
  follow_symlinks: false
  max_recursion_depth: 64
  start_at: end
  discovery:
    reconcile_interval: 5s
    reconcile_jitter_percent: 10
  reader:
    eof_reprobe_interval: 250ms
  ignore_older_than: 0s
  identity:
    fingerprint_bytes: 1000
    ignored_header_bytes: 0
    on_recovery_mismatch: beginning
  encoding: utf-8
  on_decode_error: preserve_raw
  framing:
    max_line_bytes: 1MiB
    max_record_bytes: 1MiB
    max_log_size_behavior: split
    force_flush_period: 500ms
    multiline:
      regex_profile: re2-v1
      line_start_pattern: null
      line_end_pattern: null
    max_multiline_lines: 500
  limits:
    max_tracked_files: 10000
    max_pending_candidates: 10000
    max_open_files: 512
    max_read_bytes_per_turn: 128KiB
  batch:
    max_records: 1024
    max_bytes: 8MiB
    max_flush_period: 1s
  rotation:
    rotate_wait: 5s
    on_truncate: fail
  checkpoint:
    id: app-logs
    sync_interval: 0s
    compact_after_bytes: 64MiB
    compact_after_transactions: 10000
    retention: 7d
    ownership_timeout: 30s
    max_consecutive_failures: 5
  retry:
    max_attempts: 8
    initial_backoff: 100ms
    max_backoff: 5s
  on_nack: fail
  drain_timeout: 10s
```

Important policy values are:

| Field | Values | Behavior |
| --- | --- | --- |
| `start_at` | `beginning`, `end` | Selects the first offset only when no compatible checkpoint exists. |
| `identity.on_recovery_mismatch` | `beginning`, `skip_to_end`, `fail` | Controls a new identity created when prior evidence cannot be matched safely. |
| `encoding` | `utf-8`, `ascii`, `utf-16le`, `utf-16be`, `raw` | Selects source decoding and newline semantics. |
| `on_decode_error` | `preserve_raw`, `replace`, `fail` | Preserves exact malformed bytes, substitutes invalid units, or quarantines the file. |
| `framing.max_log_size_behavior` | `split`, `truncate` | Emits bounded fragments or a bounded truncated record and discards through the line boundary. |
| `rotation.on_truncate` | `fail`, `read_new` | Durably quarantines detectable truncation or explicitly resets to a new file epoch. |
| `on_nack` | `fail`, `drop_and_continue` | Fails the receiver or durably advances under an explicit loss policy after terminal delivery failure. |

Complete, untruncated paths that convert losslessly to text emit
`log.file.path` and `log.file.name`. Non-text or over-bound paths omit those
registered attributes and instead emit bounded
`otel.arrow.filelog.path.{kind,native,truncated,sha256}` evidence; `sha256` is
the plain SHA-256 of the complete native bytes and is present only when
`native` is truncated to its final 4,096 bytes.
Phase 1 has no resolved-path, generic record-offset, or record-number
configuration.

`discovery.reconcile_interval` is independently jittered after each completed
scan by `discovery.reconcile_jitter_percent`. `reader.eof_reprobe_interval`
rechecks only an admitted reader's validated handle and never triggers
filesystem traversal. Reconciliation accepts `100ms..=24h` with `0..=25`
percent jitter; EOF reprobe accepts `10ms..=1h`.

`checkpoint.compact_after_bytes` bounds the complete WAL, including its
56-byte header. It must be at least 16,777,312 bytes so a fresh WAL can hold
one maximum version-1 transaction. The store compacts before an append would
exceed either the byte or transaction threshold; equality is accepted.

`checkpoint.retention` uses continuous runtime-proven absence, not the
persisted last-seen timestamp. The first complete reconciliation that proves a
record absent starts a monotonic interval; restart, an incomplete scan, or
source reappearance resets that proof. An idle worker wakes at the checked
deadline and revalidates before one atomic filtered compaction. Quarantined and
runtime-owned records are not removed.

Set at most one of `framing.multiline.line_start_pattern` and
`framing.multiline.line_end_pattern`. Patterns use the versioned `re2-v1`
profile. A zero `force_flush_period` disables idle partial-record flushing.

Timestamp extraction and parsing are processor responsibilities. The filelog
receiver preserves source bytes and record boundaries but does not interpret
application timestamps.

## Delivery and Recovery

Capture, delivery, and recovery are separate:

- The worker captures source bytes into one bounded receiver-wide logical
  batch plus at most one fully framed carry-over record.
- The async receiver retains that batch until the matching Ack, retry
  exhaustion, or an explicit terminal-loss policy.
- A record that cannot enter a nonempty batch is retained byte-for-byte and
  emitted as the next batch after its predecessor resolves, before any source
  read. Source rewrite, truncation, rename, or removal cannot substitute it.
- Only a matching Ack or `drop_and_continue` advances durable progress.
- Drain emits an eligible carry-over after its predecessor. Direct shutdown
  releases unsent in-memory state without synthesizing progress.
- A crash before progress is durable replays from the prior checkpoint and can
  produce duplicates.

The receiver does not persist the in-flight Arrow batch and is not a durable
telemetry spool. When downstream is full, it stops reading; unread data remains
in the source files. If source retention removes those bytes before recovery,
the receiver cannot reconstruct them.

## Checkpoints and Quarantine

The namespace is:

```text
${engine.state_dir}/filelog/@v1/<lowercase-hex checkpoint.id bytes>/
```

If `checkpoint.id` is omitted, the engine derives a stable ID from the pipeline
and node placement. Set an explicit ID when placement names may change.
`${engine.state_dir}` expands from the engine state directory and defaults to
`.otap-state`. The `@v1` component is outside the earlier draft's accepted ID
alphabet, so the version directory cannot itself be a legacy flat namespace.
Every accepted ASCII ID byte is encoded as two lowercase hexadecimal digits,
so IDs that differ only by case cannot alias on case-insensitive filesystems.
IDs are limited to 127 bytes; Unix startup also enforces a narrower component
limit reported by the mounted state filesystem.

Treat the namespace as one storage-engine unit:

- Stop the owning receiver before backup, restore, relocation, or removal.
- Back up `CURRENT`, every referenced snapshot/WAL generation, and
  `ownership.lock` together. Do not edit individual files.
- Restore only onto the same local-filesystem semantics and validate the
  versioned format and framing profile before startup.
- Changing `checkpoint.id` starts a separate namespace and applies `start_at`;
  it is not an in-place migration.

Corruption, an unknown version, or an incompatible framing profile fails
closed. A torn final WAL tail is ignored only when it cannot form the complete
transaction length declared by its header.

Quarantine is durable and is not removed by ordinary retention. Restarting or
changing `rotation.on_truncate` does not clear an existing quarantine.
Phase 1 contains audited reset, keep-failed, and removal store operations, but
does not expose a live operator state-management API. A distribution that
exposes those operations must target the exact file ID and preserve the audit
reason. As a whole-receiver fallback, an operator may stop the receiver and
archive the complete namespace before starting from a new namespace, accepting
the duplicate or skip behavior selected by `start_at`.

The exact durable format is specified in
[`docs/filelog-checkpoint-format.md`](../../../../../docs/filelog-checkpoint-format.md).

## Rotation

Move/create rotation is the supported path. The receiver keeps the old native
handle open, captures compatible late writes through EOF plus `rotate_wait`,
and independently admits the replacement file. Writes arriving after
finalization can be missed.

At confirmed permanent rotation EOF, a nonempty unterminated frame is emitted
with `otel.arrow.filelog.terminal_unterminated = true`, even when ordinary idle
flush is disabled. Its progress and finalization remain behind matching
aggregate Ack. Incomplete input under `on_decode_error: fail` is quarantined
without advancing over the malformed unit.

Copy-truncate detection is best effort. A truncate-and-regrow cycle completed
between observations can be indistinguishable from an append, and bytes
destroyed before capture or recovery are unrecoverable. Prefer move/create
rotation.

## Telemetry

The fixed-cardinality metric set is `receiver.filelog`. It has no path, file
ID, pattern, checkpoint ID, or error-string dimensions.

High-signal operating metrics include:

| Metric | Unit | Description |
| --- | --- | --- |
| `receiver.filelog.lifecycle.failures` | `{failure}` | Terminal receiver failures. |
| `receiver.filelog.batches.acked` | `{batch}` | Matching downstream Acks. |
| `receiver.filelog.batches.nacked` | `{batch}` | Matching downstream Nacks. |
| `receiver.filelog.carry_over.records` | `{record}` | Fully framed records retained across one predecessor batch. |
| `receiver.filelog.retries.exhausted` | `{batch}` | Retained batches that exhausted their send budget. |
| `receiver.filelog.backpressure.pause.duration` | `ns` | Distribution of downstream-full pauses. |
| `receiver.filelog.checkpoint.failures` | `{failure}` | Failed checkpoint operations. |
| `receiver.filelog.checkpoint.wal.size` | `By` | Current active WAL size. |
| `receiver.filelog.files.tracked` | `{file}` | Current durable checkpoint population. |
| `receiver.filelog.files.pending` | `{file}` | Current pending-candidate population. |
| `receiver.filelog.files.open` | `{file}` | Current resident source handles. |
| `receiver.filelog.files.quarantined` | `{file}` | Current durable quarantined population. |
| `receiver.filelog.candidates.overflowed` | `{candidate}` | Candidate observations rejected by the configured bound. |
| `receiver.filelog.files.tracked.saturation` | `{event}` | Admissions that encountered a full tracked table. |
| `receiver.filelog.files.descriptor.saturation` | `{event}` | Reads that encountered descriptor saturation. |
| `receiver.filelog.descriptor.budget.warnings` | `{warning}` | Starts whose receiver-owned FD budget exceeds 80% of the Unix soft limit. |
| `receiver.filelog.environmental.reprobes` | `{reprobe}` | Source/traversal operations scheduled for bounded environmental retry. |
| `receiver.filelog.environmental.recoveries` | `{recovery}` | Environmental retry conditions cleared by successful source or discovery operations. |
| `receiver.filelog.rotation.copytruncate.detected` | `{rotation}` | Observable copy-truncate transitions. |
| `receiver.filelog.partial.bytes.pending` | `By` | Current uncommitted partial source bytes. |
| `receiver.filelog.terminal.unterminated.records` | `{record}` | Records emitted only because permanent rotation EOF established a boundary. |
| `receiver.filelog.health_events.suppressed` | `{event}` | Repeated detailed events withheld by the rate limiter. |

The complete instrument catalog and averaging rules for total-duration
counters are in the
[self-telemetry design](../../../../../docs/filelog-receiver.md#self-telemetry).

Detailed events are rate-limited by fixed category. Operator-relevant events
include:

| Event | Severity | Description |
| --- | --- | --- |
| `filelog_receiver.start` | `info` | Receiver startup began. |
| `filelog_receiver.drain_ingress` | `info` | Ingress drain began. |
| `filelog_receiver.downstream_backpressure` | `warn` | The downstream channel was full. |
| `filelog_receiver.batch_retry` | `info` | A retained batch entered bounded retry. |
| `filelog_receiver.checkpoint_operation_failed` | `warn` | A checkpoint operation failed. |
| `filelog_receiver.candidate_overflow` | `warn` | Discovery exceeded pending-candidate capacity. |
| `filelog_receiver.tracked_table_saturated` | `warn` | Durable tracked-file capacity was full. |
| `filelog_receiver.descriptor_capacity_saturated` | `warn` | No descriptor slot was currently usable. |
| `filelog_receiver.descriptor_budget_warning` | `warn` | Receiver-owned descriptors exceed 80% of the Unix process soft limit. |
| `filelog_receiver.source_reprobe_scheduled` | `warn` | One source operation entered bounded environmental retry. |
| `filelog_receiver.discovery_reprobe_scheduled` | `warn` | One traversal root or candidate probe entered bounded environmental retry. |
| `filelog_receiver.copytruncate_quarantined` | `warn` | Detectable truncation was quarantined under `fail`. |
| `filelog_receiver.copytruncate_reset` | `warn` | Detectable truncation reset to a new epoch under `read_new`. |
| `filelog_receiver.decode_quarantined` | `warn` | A decode failure was durably quarantined. |
| `filelog_receiver.drain_timeout` | `warn` | Drain ended with unacknowledged work and no progress advance. |
| `filelog_receiver.terminal_failure` | `warn` | The receiver returned a terminal error. |

Events use fixed categories and numeric fields; they do not include source
paths or arbitrary source/error strings. Metrics and events are not durably
spooled by the receiver.

## Limits

- Linux, macOS, and Windows local regular files are supported. SMB, NFS, and
  other network-filesystem source semantics are not guaranteed.
- Put `engine.state_dir` on a local filesystem. Namespace locking on NFS is
  unsupported for concurrent-rollout safety.
- Run the source pipeline with one core. Use `receiver:filelog` followed by a
  topic exporter to fan out to multicore downstream processing.
- Phase 1 supports one receiver-wide retained batch plus one carry-over
  record, so one slow or failing delivery creates intentional receiver-wide
  head-of-line blocking.
- Runtime leases prevent overlapping patterns inside one process. They do not
  coordinate separate engine processes or separate state directories.
- The receiver-owned FD budget is `max_open_files + 1` traversal handle,
  `+ 1` transient probe, and `+ 8` checkpoint/namespace handles. Unix startup
  rejects a budget above `RLIMIT_NOFILE` and warns above 80%; this is not an
  aggregate process admission claim.
- `EMFILE`/`ENFILE` use one receiver-global retry state. Other temporary
  source or root failures use one bounded per-file/root state with delays
  `250ms, 500ms, ... 30s`; they never become quarantine merely by recurring.
- Discovery opens at most one traversal directory handle. Environmental
  retries preserve progress, pause no healthy resident reader, and remain
  interruptible by drain or direct shutdown.
- Windows runtime qualification remains deferred. With
  `follow_symlinks: true`, the current traversal dependency may retain one
  ancestor handle per depth outside its requested one-handle cap.
- Writers must permit the platform's compatible shared-read/delete behavior.
  A writer that denies shared read cannot be tailed.
- Windows checkpoint marker replacement is atomic, but the standard library
  cannot sync the namespace directory. Power-loss durability of its directory
  entry relies on local filesystem metadata journaling and is weaker than a
  Unix directory `fsync`.
- Symlink traversal is disabled by default. Enabling it does not weaken the
  regular-file and checkpoint-directory exclusions.
- Resource ceilings multiply. In particular, tune `max_open_files` together
  with line/record bounds, and tune `max_tracked_files` together with
  checkpoint and candidate bounds.

## Related Docs

- [Runnable console example](../../../../../configs/filelog-console.yaml)
- [Filelog architecture and complete telemetry](../../../../../docs/filelog-receiver.md)
- [Checkpoint format](../../../../../docs/filelog-checkpoint-format.md)
- [Configuration model](../../../../../docs/configuration-model.md)
- [Core node catalog](../../../README.md)
