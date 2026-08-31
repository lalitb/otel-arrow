# Profiles Upstream Architecture Notes

## Purpose

This is a live list of architecture findings discovered while adding OTAP
Profiles. It distinguishes concrete extensibility limitations from intentional
compile-time safety. Findings can be implemented in the local Profiles stack
before being proposed upstream.

## Status Legend

- `open`: confirmed and not yet addressed in the local stack;
- `in-progress`: being changed in the current stack;
- `resolved-local`: addressed locally but not proposed upstream; and
- `not-an-issue`: reviewed and retained intentionally.

## Findings

### PA-001: Payload storage is indexed by protobuf enum value

- Status: `resolved-local`
- Area: `pdata::otap::raw_batch_store`
- Evidence: `POSITION_LOOKUP[payload_type as usize]` maps protocol enum values
  into compact per-signal arrays. The lookup table currently ends at enum value
  45, while Profiles payloads occupy values 50 through 63.
- Impact: adding a payload outside the table requires synchronized global table
  edits; a missed edit can cause an out-of-bounds access instead of a typed
  error.
- Local direction: replace protocol-number indexing with an explicit compact
  per-signal payload layout.
- Local result: branch `profiles/05-profile-engine-integration` uses an
  exhaustive payload-to-position mapping independent of protobuf values.
- Upstream framing: decouple internal batch positions from protocol enum
  discriminants.

### PA-002: Payload membership is limited to a `u64` protocol-value bitmap

- Status: `resolved-local`
- Area: `pdata::otap::raw_batch_store`
- Evidence: `RawBatchStore` uses `TYPE_MASK & (1 << payload_type as u64)`.
  Profiles reaches enum value 63, exhausting all representable positions.
- Impact: the next payload value cannot be represented, and protocol numbering
  unnecessarily constrains internal storage.
- Local direction: use an explicit per-signal payload layout for membership and
  compact position lookup.
- Local result: branch `profiles/05-profile-engine-integration` selects payload
  membership by signal layout without a fixed-width bitmap.
- Upstream framing: remove the 64-value ceiling from OTAP payload storage.

### PA-003: Closed signal enumeration

- Status: `not-an-issue`
- Area: `SignalType`, `OtapArrowRecords`, and signal-sensitive components
- Evidence: signal behavior is expressed with exhaustive matches over Logs,
  Metrics, and Traces.
- Assessment: this intentionally provides compile-time coverage when Profiles
  is added. The closed enum should remain. Mechanical signal metadata may be
  centralized, but truly signal-specific behavior should stay exhaustive.
- Local direction: add Profiles explicitly and preserve exhaustive matching.

### PA-004: Generic batching assumes a parent-ID tree

- Status: `resolved-local`
- Area: `pdata::otap::transform::{split,reindex}` and query-engine branch
  concatenation
- Evidence: generic split and reindex dispatch only supports Logs, Metrics, and
  Traces. Profiles also has non-parent foreign keys from samples to stacks and
  links, stack locations to locations, locations to mappings, and lines to
  functions.
- Impact: routing Profiles through generic split or concatenation reaches an
  `unreachable!()` today. Extending only the count-based dispatch would risk
  dangling or cross-batch references.
- Local direction: preserve Profiles BAR boundaries, reject an oversized BAR,
  and return a typed query-engine error when multiple Profiles branch results
  would require concatenation.
- Upstream framing: make transform capabilities explicit per signal and add a
  graph-aware Profiles partition/reindex implementation before enabling merge.

### PA-005: Schema IDs omit Profiles large value types

- Status: `resolved-local`
- Area: `pdata::otap::schema::SchemaIdBuilder`
- Evidence: schema IDs handled `Binary` and `List` but not `LargeBinary` or
  `LargeList`, both used by the Profiles schema.
- Impact: durable-buffer schema fingerprinting panicked while persisting a
  valid Profiles sample batch.
- Local direction: assign distinct stable schema-ID encodings to large binary
  and large list types and cover them in the schema-ID test.
- Upstream framing: keep schema fingerprint type coverage synchronized with
  every Arrow type accepted by OTAP schemas, preferably with typed errors for
  unsupported types.

### PA-006: Profiles item counting requires a full protobuf decode

- Status: `open`
- Area: `pdata::payload::OtlpProtoBytes`
- Evidence: Logs, Metrics, and Traces count serialized items through raw
  zero-copy views. No equivalent Profiles wire view exists, so the local stack
  decodes `ExportProfilesServiceRequest` to count profiles.
- Impact: the first item-count request parses and allocates the complete
  Profiles request on a runtime path used by flow metrics, batching, and
  durable buffering. Payload-level caches avoid repeated work but not the first
  decode.
- Local direction: keep the full decode for correct accounting in step 5.
- Upstream framing: add a validated raw Profiles view that can count nested
  profiles without materializing the protobuf graph.

### PA-007: OTLP byte batching assumes independent repeated root fields

- Status: `resolved-local`
- Area: `pdata::otlp::batching`
- Evidence: generic byte batching concatenates top-level field 1 entries and
  splits oversized entries independently. OTLP Profiles field 2 is one
  request-wide dictionary referenced by profiles under field 1.
- Impact: concatenating requests without index rewriting or splitting resource
  profiles away from their dictionary changes reference meaning.
- Local direction: preserve every serialized Profiles request as one output
  batch and reject an oversized request.
- Upstream framing: make byte-batching strategy signal-specific and require a
  dictionary-aware merge before combining Profiles requests.

### PA-008: Parquet partition IDs rewrite only tree relationships

- Status: `resolved-local`
- Area: `parquet_exporter::PartitionSequenceIdGenerator`
- Evidence: partition-unique ID generation offsets only `id` and `parent_id`.
  Profiles also uses `stack_id`, `link_id`, `location_id`, `mapping_id`, and
  `function_id`.
- Impact: exporting a second Profiles BAR would create dangling or cross-BAR
  references even though the root and parent columns looked unique.
- Local direction: reject Profiles before Parquet ID rewriting.
- Upstream framing: a Profiles-aware Parquet path must rewrite every foreign
  key together and preserve root partition metadata.

### PA-009: Generic filters and attribute mutation are not graph-aware

- Status: `resolved-local`
- Area: pdata filtering, query-engine conditionals and assignment, and the
  attributes processor
- Evidence: generic filtering materializes only parent-ID trees, while generic
  attribute assignment assumes `UInt16` owners and attribute rows without
  Profiles ordinals.
- Impact: filtering could change the signal variant or panic, and assignment
  could fail after partially evaluating a query or silently skip requested
  mutations.
- Local direction: return explicit unsupported errors for Profiles filtering,
  conditional processing, query assignment, and configured attribute
  transformations.
- Local result: step 10 adds explicit native sample filtering, optional
  reachability compaction, whole-owner profile/sample/mapping/location
  rename/delete operations, and bounded function-redaction copy-on-write.
  Generic query stages remain rejected because they do not encode Profiles
  owner semantics.
- Upstream framing: enable these operations only with graph closure,
  copy-on-write, `UInt32` owner support, ordinal maintenance, and post-mutation
  validation.

### PA-010: Resource entity references have no Profiles OTAP representation

- Status: `resolved-local`
- Area: OTLP Profiles to OTAP conversion
- Evidence: the pinned `Resource` message can carry `entity_refs`, but the
  current Profiles root and shared resource-attribute tables have no columns or
  related payload for them.
- Impact: accepting those requests would silently lose entity identity and
  description-key semantics during conversion.
- Local direction: reject a Profiles request containing resource entity
  references.
- Upstream framing: define a signal-independent OTAP entity-reference
  representation before enabling conversion of this field.

### PA-011: Profiles conversion limits are not configurable

- Status: `open`
- Area: both Profiles conversion directions and `ConversionOptions`
- Evidence: the encoder uses checked IDs and offsets, dictionary-backed repeated
  strings, a bounded AnyValue nesting depth, and Arrow capacity checks, but
  `ConversionOptions` exposes no Profiles-specific profile, sample, dictionary,
  list-element, or retained-memory limits.
- Impact: conversion still allocates proportional to a request that passes the
  receiver's encoded-byte limit, and embedders can invoke conversion without a
  transport admission limit.
- Local direction: keep conversion fail-fast for representational limits, avoid
  repeated-string amplification, preserve whole request/BAR boundaries, and
  apply the existing transport byte, concurrency, and queue bounds. Reverse
  conversion also estimates expanded dictionary strings, protobuf row objects,
  and serialized AnyValue structure before materialization.
- Local result: step 9 enables experimental transport without claiming
  profile, sample, dictionary, list-element, or retained-memory cardinality
  bounds.
- Upstream framing: add configurable Profiles conversion/admission limits
  before treating the transport as production-ready.

### PA-012: Ordered-child validation assumed physical row order

- Status: `resolved-local`
- Area: `ProfilesBatchView` graph validation
- Evidence: ordinal validation previously required rows for one parent to
  arrive physically as `0, 1, 2, ...`, even though OTAP IDs and ordinals define
  logical order independently of Arrow row order.
- Impact: a semantically valid processor reorder was rejected before the
  OTAP-to-OTLP encoder could sort by `(parent_id, ordinal)`.
- Local direction: group and sort ordinal values before checking uniqueness and
  contiguity.
- Upstream framing: validation must distinguish logical ordering columns from
  physical transport sort requirements.

### PA-013: Reverse conversion materializes the protobuf graph

- Status: `open`
- Area: OTAP Profiles to OTLP conversion
- Evidence: the step-7 encoder reconstructs a complete
  `ExportProfilesServiceRequest`, computes its encoded length, and then encodes
  it into a bounded buffer. Step 9 adds a conservative preflight for logical
  Arrow bytes, expanded dictionary references, protobuf row objects, and
  serialized AnyValue structure.
- Impact: compact dictionary-backed input is rejected before large repeated
  strings are cloned, but accepted conversion still temporarily holds the
  Arrow graph, protobuf graph, and encoded bytes concurrently.
- Local direction: bound materialization with conservative expansion accounting
  and the existing 256 MiB maximum output limit.
- Upstream framing: stream Profiles fields transactionally into `ProtoBuffer`
  once profiling shows the extra materialization is significant.

### PA-014: No privacy-safe real Profiles capture is checked in

- Status: `open`
- Area: validation datasets and benchmarks
- Evidence: the repository contains no pprof, JFR, Linux perf, or OTLP Profiles
  capture suitable for repeatable tests.
- Impact: current validation uses deterministic synthetic workloads and cannot
  prove fidelity or compression behavior for every producer-specific shape.
- Local direction: cover CPU, allocation, off-CPU, timestamp-only,
  high-cardinality attribute, and original-payload workloads synthetically.
- Upstream framing: contribute anonymized, redistributable captures with
  documented provenance and expected semantics.

### PA-015: The Rust workspace has no persistent fuzz harness

- Status: `open`
- Area: conversion robustness
- Evidence: there is no checked-in `cargo-fuzz`, `proptest`, `quickcheck`, or
  corpus infrastructure.
- Impact: bounded deterministic randomized tests exercise both conversion
  directions, malformed protobuf bytes, and mutated OTAP graphs, but they do
  not provide coverage-guided fuzzing across revisions.
- Local direction: use fixed-seed randomized no-panic tests in the normal test
  suite so coverage remains dependency-free and repeatable.
- Upstream framing: add shared fuzz infrastructure before maintaining
  Profiles-specific corpora and long-running CI jobs.

### PA-016: The Go runtime has no Profiles Arrow implementation

- Status: `open`
- Area: Go OTAP producer, consumer, receiver, and exporter packages
- Evidence: the shared protobuf and generated Go API declare Profiles payload
  types and `ArrowProfilesService`, but the Go tree has no Profiles Arrow
  schemas, encoder/decoder, or signal service integration.
- Impact: Go users can compile against the new service interface but cannot
  produce, consume, receive, or export OTAP Profiles with the repository's Go
  runtime.
- Local direction: regenerate the Go protocol binding and mocks so the shared
  wire contract stays synchronized, without implying runtime support.
- Upstream framing: implement the Go Profiles Arrow codec and transport path in
  a separate change with cross-language Rust/Go fixtures.

### PA-017: Generic attribute value builders drop Profiles owner metadata

- Status: `open`
- Area: pdata attribute insert, upsert, update, and hash transforms
- Evidence: the generic builder reconstructs the common attribute schema but
  does not retain Profiles `ordinal` and `unit` columns.
- Impact: routing Profiles through value-changing actions can publish an
  invalid or semantically incomplete attribute batch.
- Local direction: step 10 supports collision-safe rename and delete through a
  Profiles-specific wrapper and rejects insert, upsert, update, and hash.
- Upstream framing: make attribute builders schema-extensible before enabling
  value-changing Profiles actions.

### PA-018: Query languages do not express Profiles attribute owners

- Status: `open`
- Area: query-engine filtering, apply-to-attributes, and root attribute scopes
- Evidence: existing query scopes distinguish root and non-root attributes for
  logs, metrics, and traces but cannot identify profile, sample, mapping, or
  location owners and their global versus selection-local semantics.
- Impact: guessing a payload can silently no-op or mutate a shared owner more
  broadly than the query implies.
- Local direction: keep generic query stages explicitly rejected and expose
  safe semantics through native Profiles filter and transformation kernels.
- Upstream framing: add typed Profiles owner scopes before enabling query
  filtering or assignment.

### PA-019: Selection-local copy-on-write is intentionally narrow

- Status: `open`
- Area: Profiles shared-entity mutation
- Evidence: step 10 implements a bounded reverse walk for function-filename
  redaction by cloning functions, complete locations, lines, location
  attributes, stacks, and stack-location edges.
- Impact: selection-local mapping/location attributes, resource/scope changes,
  arbitrary function fields, and profile splitting still lack bounded kernels.
- Local direction: expose only the implemented filename-redaction walk and keep
  other selection-local operations unsupported.
- Upstream framing: add one bounded, benchmarked semantic kernel at a time
  rather than a generic graph mutation API.

### PA-020: Profiles parent encodings must be canonicalized before validation

- Status: `resolved-local`
- Area: OTAP transport optimization and Profiles graph construction
- Evidence: Profiles uses UInt32 delta parents for samples, value types,
  stack-location edges, and location-line rows; resource and scope attributes
  use UInt16 quasi-delta parents.
- Impact: validating physical deltas as logical IDs can corrupt graph closure,
  owner uniqueness, dense remapping, and copy-on-write selection.
- Local direction: mark internal plain parents explicitly, encode the specified
  Profiles columns for transport, materialize all encoded parents inside
  `Profiles::try_from`, and reject cumulative overflow.
- Upstream framing: every graph validator must operate on canonical logical IDs,
  never transport deltas.

### PA-021: Profiler request sizes exceed the normal OTLP receiver default

- Status: `resolved-local`
- Area: OTLP Profiles ingress and deployment configuration
- Evidence: the eBPF profiler and Collector distribution allow gRPC messages up
  to 32 MiB, while the local OTLP receiver defaults to 4 MiB.
- Impact: a valid high-cardinality host profile can be rejected before Profiles
  conversion or graph validation begins.
- Local direction: configure the eBPF pipeline receiver for a finite 32 MiB
  decoding limit and retain conversion expansion checks behind that boundary.
- Upstream framing: profiling deployment examples must align exporter and
  receiver message limits without removing bounded admission.

### PA-022: The upstream profiler has no unprivileged reporter or replay mode

- Status: `open`
- Area: eBPF integration testing
- Evidence: the profiler requires Linux 5.10 or newer, host PID visibility,
  tracefs, eBPF/perf capabilities, and unconfined syscall policies before its
  reporter starts. It has no packaged mode that emits Profiles without loading
  eBPF.
- Impact: the real integration cannot run on ordinary unprivileged CI workers,
  non-Linux hosts, or Docker environments without host integration.
- Local direction: keep deterministic Profiles generation in normal CI and run
  the pinned real-profiler smoke only on explicitly approved hosts.
- Upstream framing: add a reporter-level replay or synthetic source that uses
  the production export path without requiring kernel attachment.

### PA-023: Profiler artifacts and captures require separate distribution review

- Status: `open`
- Area: licensing, provenance, and profile-data privacy
- Evidence: the profiler code is Apache-2.0, its embedded eBPF object is
  GPL-2.0, and real captures can disclose host process, executable, and source
  information.
- Impact: vendoring binaries, image layers, coredumps, or unsanitized captures
  would add licensing and privacy obligations unrelated to the Rust code.
- Local direction: reference a digest-pinned official image at runtime, profile
  only an in-repository workload, and do not persist or check in the output.
- Upstream framing: publish redistributable sanitized fixtures with provenance,
  expected semantics, and explicit license review.

### PA-024: Validation scenarios discarded controller startup failures

- Status: `resolved-local`
- Area: validation harness, cross-cutting
- Evidence: `PipelineSimulator::run` ignored the result of
  `Controller::run_till_shutdown`, leaving the readiness poll as the only
  observable failure path.
- Impact: an unknown component or invalid runtime configuration appeared as a
  delayed readiness timeout for every signal instead of the original error.
- Local direction: return the controller result through a bounded channel and
  check it during readiness, generation, and validation polling.
- Upstream framing: programmatic harnesses must preserve background runtime
  failures and distinguish startup exit from readiness timeout.

### PA-025: OTLP receiver concurrency was multiplied by signal services

- Status: `resolved-local`
- Area: OTLP receiver admission, cross-cutting
- Evidence: independently constructed logs, metrics, traces, and Profiles
  services could each admit up to the configured concurrency value.
- Impact: aggregate receiver work and memory could exceed the operator's
  configured bound as more signal services were enabled.
- Local direction: share one receiver-wide admission gate across every OTLP
  service and protocol path.
- Upstream framing: define concurrency limits at the receiver boundary and test
  aggregate mixed-signal admission.

### PA-026: OTAP status handling lost generic failure semantics

- Status: `resolved-local`
- Area: OTAP ACK/NACK transport, cross-cutting
- Evidence: downstream batch status codes were not consistently classified as
  retryable or permanent, and malformed BARs could fail without a correlated
  `INVALID_ARGUMENT` status.
- Impact: all signals could be retried after permanent rejection or wait for a
  response that no longer corresponded to the submitted batch.
- Local direction: preserve status-code retryability, emit permanent NACKs for
  non-retryable failures, and correlate malformed-batch rejection.
- Upstream framing: continue the generic status propagation work tracked by
  [open-telemetry/otel-arrow#1921](https://github.com/open-telemetry/otel-arrow/issues/1921)
  rather than treating Profiles as a special case.

## Review Policy

New findings should record evidence, practical Profiles impact, whether they
are genuine limitations or intentional safety properties, and the chosen local
direction before implementation.
