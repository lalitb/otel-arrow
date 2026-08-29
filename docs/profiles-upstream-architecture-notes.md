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
- Area: OTLP Profiles to OTAP conversion and `ConversionOptions`
- Evidence: the encoder uses checked IDs and offsets, dictionary-backed repeated
  strings, a bounded AnyValue nesting depth, and Arrow capacity checks, but
  `ConversionOptions` exposes no Profiles-specific profile, sample, dictionary,
  list-element, or retained-memory limits.
- Impact: conversion still allocates proportional to a request that passes the
  receiver's encoded-byte limit, and embedders can invoke conversion without a
  transport admission limit.
- Local direction: keep the step-6 encoder fail-fast for representational
  limits and avoid repeated-string amplification.
- Upstream framing: add configurable conversion/admission limits before
  Profiles transport is enabled in step 9.

## Review Policy

New findings should record evidence, practical Profiles impact, whether they
are genuine limitations or intentional safety properties, and the chosen local
direction before implementation.
