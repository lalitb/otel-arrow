# Profiles Implementation PR Plan

> Temporary planning aid for the stacked implementation. Remove this file
> before the design change is merged.

The implementation is intentionally split into small dependent pull requests.
Each branch is stacked on the preceding branch.

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
5. `profiles/05-profile-engine-integration`
   - Add the Profiles signal and payload variants, memory accounting, batching
     primitives, and usable durable-buffer persistence and reconstruction.
6. `profiles/06-otlp-to-otap-profiles`
   - Encode OTLP Profiles into the OTAP Profiles representation.
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
