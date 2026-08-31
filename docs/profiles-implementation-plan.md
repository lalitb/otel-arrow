# Profiles Implementation PR Plan

> Temporary planning aid for the stacked implementation. Remove this file
> before the design change is merged.

The implementation is intentionally split into small dependent pull requests.
Each branch is stacked on the preceding branch.

## Current Status

- Steps 1 through 11 are implemented on their corresponding stacked branches.
- All stack branches are rebased onto upstream commit `80d38834` from August
  28, 2026.
- Step 10 is implemented on the current branch with native sample filtering,
  explicit graph compaction and dense ID remapping, whole-owner attribute
  rename/delete operations, global function-filename redaction, and bounded
  selection-local copy-on-write.
- Profiles BARs remain separate during batching, and oversized BARs are
  rejected until graph-aware partitioning and concatenation are implemented.
  Serialized OTLP Profiles requests likewise remain separate because each owns
  a request-wide dictionary.
- Architecture findings and decisions are tracked in
  [Profiles Upstream Architecture Notes](profiles-upstream-architecture-notes.md).
- No privacy-safe real production capture or repository fuzz harness was
  available, so those remain explicit follow-ups.
- The Go binding exposes the Profiles stream API, but the Go runtime still has
  no Profiles Arrow codec or receiver/exporter implementation.
- Generic query transformations, selection-local resource/scope mutation, and
  Profiles insert/upsert/update/hash remain explicit unsupported paths.
- Step 11 adds deterministic Profiles generation and semantic validation plus
  an environment-gated real OpenTelemetry eBPF profiler smoke test. See
  [Profiles eBPF Integration](../rust/otap-dataflow/docs/profiles-ebpf-integration.md).

1. `profiles/01-otap-data-model-design`
   - Define the Profiles OTAP data model, processing semantics, validation
     requirements, and implementation boundaries.
2. `profiles/02-profile-payload-types`
   - Pin the Profiles protobuf revision and declare the OTAP payload types.
3. `profiles/03-explicit-wal-slot-mapping`
   - Decouple durable-buffer slots from protocol enum values, preserve existing
     assignments, reserve Profiles Arrow slots 46-59, and reserve opaque
     Profiles slot 63.
4. `profiles/04-profile-arrow-schemas`
   - Implement Profiles Arrow schemas, typed views, and graph validation.
   - Status: implemented; pdata checks, Clippy, and all pdata library tests pass.
5. `profiles/05-profile-engine-integration`
   - Add the Profiles signal and payload variants, memory accounting, batching
     primitives, and usable durable-buffer persistence and reconstruction.
   - Status: implemented on the handoff branch; unsafe generic filtering,
     mutation, merge, partitioning, and unsupported exporters fail explicitly.
6. `profiles/06-otlp-to-otap-profiles`
   - Encode OTLP Profiles into the OTAP Profiles representation.
   - Status: implemented; malformed dictionary references and unsupported
     resource entity references reject the complete request.
7. `profiles/07-otap-to-otlp-profiles`
   - Decode OTAP Profiles into OTLP and add semantic round-trip coverage.
   - Status: implemented; full-graph reconstruction, bounded output,
     canonical fixed-point, reordered dictionary, and malformed-input tests
     pass.
8. `profiles/08-profile-validation-benchmarks`
   - Validate semantic equivalence with real profiling datasets, fuzz both
     conversion directions, and benchmark size, CPU, and memory behavior.
   - Status: deterministic CPU, allocation, off-CPU, timestamp-only,
     high-cardinality, and original-payload datasets are covered. Bounded
     randomized tests and both-direction benchmarks are implemented; real
     captures and persistent fuzz corpora remain open.
9. `profiles/09-profile-transport`
   - Add receiver, exporter, OTAP transport, and bounded batching behavior.
   - Status: implemented; OTLP/gRPC and `/v1development/profiles` preserve
     serialized request boundaries, OTLP exporters encode and send Profiles,
     and OTAP uses bounded signal-specific streams with ACK/NACK correlation.
     Generic transport byte, concurrency, and queue limits apply;
     Profiles-specific graph cardinality limits remain open.
10. `profiles/10-profile-transformations`
    - Add native filtering, attribute transformation, redaction, compaction,
      and copy-on-write handling for shared profile entities.
    - Status: implemented; sample filtering can optionally compact unreachable
      dimensions and densely rewrite IDs. Rename/delete operations preserve
      profile attribute owners and ordinals. Function filename redaction
      supports both BAR-global mutation and bounded sample-local copy-on-write.
      Generic query stages and value-creating attribute actions remain rejected.
11. `profiles/11-ebpf-profiler-integration`
    - Connect the eBPF profiler to the Profiles pipeline and verify the complete
      collection, transport, processing, buffering, and export path.
    - Status: implemented. Normal CI uses deterministic Profiles through OTLP,
      graph-aware processing, durable buffering, OTAP transport, canonical
      reconstruction, and semantic equivalence. A pinned Collector
      `0.159.0` smoke test exercises the real profiler on approved Linux hosts
      with Docker and the required eBPF privileges.

The branch names after step 3 are provisional and may be adjusted when each
change is started. Schema or protocol discoveries may also require inserting a
small prerequisite PR without expanding the scope of an existing review.
