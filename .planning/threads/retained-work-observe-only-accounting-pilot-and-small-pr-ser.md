---
slug: retained-work-observe-only-accounting-pilot-and-small-pr-ser
title: Retained-work observe-only accounting pilot and small PR series
status: open
created: 2026-08-12
updated: 2026-08-12
---

<!-- markdownlint-disable MD025 -->

# Thread: Retained-work observe-only accounting pilot and small PR series

## Goal

Deliver a constrained observe-only retained-work accounting pilot through small,
reviewable PRs without introducing memory enforcement.

## Context

Architecture review converged on a clean implementation against current main.
The merged process limiter remains the RSS/cgroup safety backstop. Current
pressure-aware admission is receiver-instance based, not tenant-keyed.

The observe-only RFC (#3316) and pdata retained-memory sizing (#3443) are already
merged. The old POC and Phase 2 foundation are prior art only and must not be
resurrected as implementation branches.

Read the decision, roadmap, state, and review summaries referenced below before
starting or resuming implementation.

## References

- `.planning/retained-work/DECISIONS.md`
- `.planning/retained-work/ROADMAP.md`
- `.planning/retained-work/STATE.md`
- `.planning/retained-work/reviews/OPUS-EVIDENCE.md`
- `.planning/retained-work/reviews/FABLE-DECISION.md`
- `rust/otap-dataflow/rfcs/0000-observe-only-retained-work-accounting.md`
- `rust/otap-dataflow/docs/memory-limiter-phase1.md`

## Next Steps

- Start PR A: introduce stable `LocalResumeId` scheduler correlation against
  current `origin/main`.
- Inspect all current `requeue_later` and `DelayedData` callers before editing.
- Keep PR A generic and independent of memory accounting.
