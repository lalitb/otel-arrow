# Fable Final Architecture Decision Summary

## Verdict

Proceed with a constrained observe-only pilot, not a complete rollout.

## Core Judgment

Attributed logical retained bytes provide a diagnostic signal that process RSS
and message-count queue depth cannot provide. The principal cost is maintaining
exact ownership settlement across every normal and exceptional exit path.

## Selected Direction

- Restart cleanly from the merged RFC.
- Reuse isolated prior-art ideas, not prior implementation branches.
- Use a hybrid scope-wiring model that resolves runtime-local state during
  engine wiring and caches it for charge sites.
- Include compact, trusted owner attribution from the first primitive.
- Keep raw tenant identity out of metrics and owner registration.
- Use explicit local completion and exceptional abandonment-on-drop.
- Keep tickets structurally attached to retained component state.
- Report mixed-owner batches through a bounded `Mixed` sentinel initially.
- Keep tenant enforcement and shared-boundary accounting out of the pilot.

## Corrections Applied After the Review

- RFC #3316 and pdata sizing #3443 are already merged; they are not future PRs.
- Reuse `LocalResumeId` and retention-site concepts after reconciling current
  main, not literally without review.
- Batch ownership must use one retained-entry structure rather than parallel
  vectors.
- Runtime generation metrics must be retired; they are bounded-active rather
  than intrinsically fixed-cardinality.
- Normal shutdown must not produce abandonment spikes.

## Pilot Sites

- Delayed retry retention keyed by stable scheduler identity.
- Batch pending retention with payload and ticket in one private entry.

## Enforcement Boundary

Retained-work sites observe ownership only. Any future tenant rejection or
throttling belongs at explicit admission points and requires a separate RFC.
The process-wide memory limiter remains the backstop for allocator overhead,
unattributed state, uninstrumented components, and logical/RSS divergence.
