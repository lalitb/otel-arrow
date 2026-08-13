# Opus Review Evidence Summary

## Scope

Source-only review of current upstream capabilities, relevant RFC and branches,
the small observe-only POC, and targeted prior art in the Phase 2 foundation.
Nothing was built or tested.

## Confirmed Findings

- Existing queue telemetry reports message depth rather than retained bytes;
  RSS/cgroup memory has no pipeline or retention-site attribution.
- Current pressure-aware admission gates ignore `AdmissionContext` identity and
  use receiver-instance buckets. Tenant-keyed throttling is not implemented.
- The observe-only POC is not production-wired: its ambient runtime account
  installation has no non-test caller.
- The POC performs ambient lookup at charge sites rather than scope resolution
  during component/runtime wiring.
- POC retry tickets are matched by deadline. Shutdown can rewrite deadlines,
  breaking settlement; duplicate deadlines are also ambiguous.
- The POC lacks explicit completion versus abandonment, scope attribution, and
  metrics export.
- The large Phase 2 foundation couples accounting deeply to enforcement,
  leases, overshoot debt, reclaim, and drain allowances. It is design prior art,
  not a reusable branch.
- Phase 2 contains useful isolated patterns: scheduler-assigned
  `LocalResumeId`, static retention-site labels, and an explicit shared escrow
  lifecycle.
- Phase 2 also repeated ambient lookup and omitted explicit completion for local
  tickets, showing that injection and local abandonment require deliberate
  design rather than blind RFC transcription.

## Later Current-Head Correction

- Pdata retained-memory sizing is already merged upstream as #3443.
- The observe-only retained-work RFC is already merged upstream as #3316.
- Therefore neither belongs in the new implementation PR stack.

## Review Limitations

- No compile, test, or benchmark evidence was collected.
- The complete Phase 2 diff was not reviewed.
- Performance claims remain hypotheses until the pilot measures them.
