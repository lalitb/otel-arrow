# Retained-Work Pilot Decisions

## Status

Accepted for a constrained observe-only pilot.

## Existing Upstream Foundations

- RFC #3316 is merged.
- Pdata retained-memory sizing #3443 is merged.
- The process-wide RSS/cgroup memory limiter remains the safety backstop.
- Pressure-aware admission currently uses a receiver-instance bucket; tenant
  identity does not select independent buckets.

## Accepted Design

- Implement cleanly against current main.
- Use hybrid scope wiring: the engine supplies an attributed component scope
  through existing runtime context, and that scope caches runtime-local account
  access resolved during wiring.
- Missing runtime accounting installation must fail loudly during pipeline
  construction, never silently disable an instrumented site.
- Use runtime-local, non-`Send` tickets with checked arithmetic.
- Normal lifecycle paths explicitly complete tickets. Exceptional unresolved
  `Drop` refunds and records abandonment.
- Normal shutdown, drain, and handled errors must not increment abandonment.
- Introduce a compact trusted `WorkOwnerId` in the initial scope model.
- Initially resolve owners from trusted pipeline identity.
- Never use raw tenant strings as owner IDs or metric labels.
- Use bounded owner registration with `Mixed` and `Unregistered` sentinels.
- Report mixed-owner batches as `Mixed` during the pilot.
- Keep `OtapPdata` accounting-neutral.
- Use retry and batch pending as the only initial retention-site pilots.
- Keep metric series bounded by active runtime deployments and retire series
  when their runtime generation is destroyed.

## Reusable Prior Art

- The `LocalResumeId` design from the Phase 2 branch.
- The fixed, statically labelled `RetainedSiteKind` concept, limited initially
  to sites with implemented semantics.
- The shared escrow lifecycle as specification material for post-pilot work.

Reuse these ideas after reviewing them against current main; do not copy old
branch code without reconciling current callers and tests.

## Deferred

- Shared escrow, queues, topics, broadcast rings, fanout, and exporters.
- Tenant-specific limits and fairness.
- Hierarchical budgets, leases, overshoot debt, reclaim, reserves, preemption,
  rejection, and backpressure driven by retained-work totals.
- Any enforcement design, which requires a separate RFC and pilot evidence.

## Rejected

- Repairing the observe-only POC as the implementation base.
- Extracting the 6,392-line Phase 2 memory-budget implementation.
- Ambient account lookup on each charge.
- Retry-ticket matching by deadline.
- Parallel batch payload and ticket vectors; use one private retained-entry type.
- Treating logical retained bytes as RSS or as a replacement for the process
  limiter.
