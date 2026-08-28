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

## Review Policy

New findings should record evidence, practical Profiles impact, whether they
are genuine limitations or intentional safety properties, and the chosen local
direction before implementation.
