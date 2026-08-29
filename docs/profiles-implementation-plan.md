# Profiles Implementation PR Plan

> Temporary planning aid for the stacked implementation. Remove this file
> before the design change is merged.

The implementation is intentionally split into small dependent pull requests.
Each branch is stacked on the preceding branch.

## Current Status

- Steps 1 through 5 are implemented and committed on their corresponding local
  stack branches.
- All stack branches are rebased onto upstream commit `80d38834` from August
  28, 2026.
- Step 6 is implemented on the current branch. It decodes the pinned OTLP
  Profiles request, resolves the request-wide dictionary, materializes nested
  string references, emits all Profiles OTAP tables, and validates the complete
  graph.
- Profiles BARs remain separate during batching, and oversized BARs are
  rejected until graph-aware partitioning and concatenation are implemented.
  Serialized OTLP Profiles requests likewise remain separate because each owns
  a request-wide dictionary.
- Architecture findings and decisions are tracked in
  [Profiles Upstream Architecture Notes](profiles-upstream-architecture-notes.md).
- Steps 7 through 11 have not started.

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
8. `profiles/08-profile-validation-benchmarks`
   - Validate semantic equivalence with real profiling datasets, fuzz both
     conversion directions, and benchmark size, CPU, and memory behavior.
9. `profiles/09-profile-transport`
   - Add receiver, exporter, OTAP transport, and bounded batching behavior.
10. `profiles/10-profile-transformations`
    - Add native filtering, attribute transformation, redaction, compaction,
      and copy-on-write handling for shared profile entities.
11. `profiles/11-ebpf-profiler-integration`
    - Connect the eBPF profiler to the Profiles pipeline and verify the complete
      collection, transport, processing, buffering, and export path.

The branch names after step 3 are provisional and may be adjusted when each
change is started. Schema or protocol discoveries may also require inserting a
small prerequisite PR without expanding the scope of an existing review.
