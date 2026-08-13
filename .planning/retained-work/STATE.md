# Retained-Work Pilot State

Updated: 2026-08-12

## Current Work

PR: A - stable delayed-work `LocalResumeId`

Branch: not created

Worktree: not created

Base: current `origin/main`

Status: ready for current-source planning

## Completed

- Observe-only retained-work RFC merged upstream as #3316.
- Pdata retained-memory sizing merged upstream as #3443.
- Opus source review and targeted Phase 2 prior-art review completed.
- Fable architecture decision completed.
- Final decision: constrained pilot with a clean implementation.
- Dedicated coordination worktree and branch created.

## Validation

No implementation has been built or tested as part of this planning work.

## Open Questions for PR A

- Which current `requeue_later` and `DelayedData` consumers must change?
- Should runtime-global delayed delivery carry an optional ID or keep a distinct
  no-ID contract?
- What is the smallest API change that remains generic and testable?

## Next Exact Action

Refresh current `origin/main`, inspect every current delayed-requeue caller and
control-message consumer, then write a source-anchored PR A plan before editing.

## Session Handoff

Read, in order:

1. `.planning/threads/retained-work-observe-only-accounting-pilot-and-small-pr-ser.md`
2. `.planning/retained-work/DECISIONS.md`
3. `.planning/retained-work/ROADMAP.md`
4. This file

Work only on the PR named under Current Work. Do not reopen accepted architecture
decisions without contradictory evidence from current source. Preserve unrelated
worktrees and do not place planning commits in implementation branch ancestry.
