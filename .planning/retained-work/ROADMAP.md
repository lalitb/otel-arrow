# Retained-Work Observe-Only Pilot Roadmap

## Goal

Prove that attributed logical retained bytes are correct, operationally useful,
bounded in cardinality, and inexpensive enough to enable before expanding site
coverage or designing enforcement.

## Dependency Graph

```text
PR A: stable delayed-work LocalResumeId

PR B1: local ticket lifecycle and accounting
  -> PR B2: attributed scope and bounded WorkOwnerId
       -> PR C: runtime scope wiring and fail-loud installation
            -> PR D: metrics, generation retirement, cardinality tests
                 +-> PR E: retry pilot (also depends on PR A)
                 +-> PR F: batch-pending pilot
```

PR A and PR B1 can begin independently from current `origin/main`. PR E and PR
F may proceed in parallel after their dependencies are stable.

## PR A: Stable Delayed-Work Identity

Principal invariant: delayed payload identity does not depend on its deadline.

- Add scheduler-assigned `LocalResumeId`.
- Return it from successful local delayed requeue.
- Carry it back on local `DelayedData` delivery.
- Preserve identity when shutdown rewrites deadlines.
- Cover duplicate deadlines, rejection, delivery, and shutdown drain.
- Keep the change generic and independent of memory accounting.

## PR B1: Local Accounting Primitive

Principal invariant: a local retained charge settles exactly once.

- Add non-`Send` local account and ticket types.
- Add explicit normal completion.
- Refund unresolved `Drop` and record abandonment.
- Use checked arithmetic plus visible corruption diagnostics.
- Cover known and unknown size, completion, exceptional drop, double-settlement
  prevention, overflow, and underflow behavior.
- Do not add configuration, production wiring, metrics export, or charge sites.

## PR B2: Attributed Scope and Work Owner

Principal invariant: every charge belongs to a required, bounded, trusted
scope.

- Define compact `WorkOwnerId` handles and a bounded registry.
- Define `Mixed` and `Unregistered` sentinels.
- Include group, pipeline, runtime/core, generation, component, and owner in the
  scope; keep retention site on the ticket or charge call.
- Never register raw request identity directly.
- Cover capacity bounds and deterministic sentinel behavior.

## PR C: Runtime Scope Wiring

Principal invariant: production accounting cannot silently disappear.

- Install the runtime-local account during pinned runtime initialization.
- Construct attributed component scopes through existing runtime/node context.
- Resolve ambient runtime state once during wiring and cache it in the scope.
- Fail pipeline startup clearly when required installation is absent.
- Cover custom-component access, live generation replacement, and teardown.

## PR D: Metrics and Lifecycle

Principal invariant: exported attribution is useful and bounded over time.

- Export logical retained bytes, unknown-size items, abandonment, and accounting
  corruption diagnostics.
- Use static site labels and existing bounded component/pipeline conventions.
- Ensure destroyed generations retire their metric state.
- Enumerate maximum active series for a configuration.
- Add the first user-facing changelog entry.

## PR E: Retry Pilot

Principal invariant: retry tickets follow delayed payload identity through every
terminal path.

- Key tickets by `LocalResumeId`, never deadline or size.
- Complete on resume, failed scheduling cleanup, normal cancellation, drain,
  shutdown, and handled errors.
- Prove duplicate deadlines and shutdown rewriting preserve accounting.
- Add a user-facing changelog entry if this expands observable coverage.

## PR F: Batch-Pending Pilot

Principal invariant: pending payload and accounting ownership cannot diverge.

- Store payload, context, and ticket in one private pending-entry type.
- Complete on flush, partial flush, error cleanup, drain, and shutdown.
- Use the merged pdata retained-size estimator once at retention start.
- Represent mixed ownership with the bounded `Mixed` owner.
- Add a user-facing changelog entry if this expands observable coverage.

## Pilot Stop/Go Gate

Continue beyond retry and batch only when all conditions hold:

- Retained totals return to zero after normal success, errors, drain, and
  shutdown.
- Arithmetic-corruption and abandonment counters stay zero on all normal paths.
- Forced abnormal drop produces an abandonment signal.
- The coverage inventory explicitly distinguishes covered and uncovered sites.
- An induced backlog identifies the responsible pipeline and site before or
  alongside process pressure.
- Disabled overhead is effectively negligible.
- Enabled throughput regression and pdata estimation cost meet an agreed
  benchmark threshold; the architecture review suggested 1 percent as an
  initial target, not a pre-approved requirement.
- Active metric series are bounded and old generations are retired.
- Logical retained totals diverge from RSS only for documented causes such as
  fanout, allocator memory, and uncovered sites.
- Owner attribution remains stable across reconfiguration.

Tenant enforcement receives a separate RFC only after the pilot demonstrates a
small and understood `Mixed`/unattributed fraction.

## Branch and PR Policy

- Use ordinary dependent GitHub PRs.
- Give each PR one principal invariant and explicit exclusions.
- Prefer approximately 200-500 changed lines when practical.
- While stacked, target the immediate parent branch and state `Depends on` in
  the PR description.
- After a parent merges, rebase the child onto current main, retarget it to
  main, verify the displayed diff, and rerun required validation.
- Never base implementation branches on this planning branch.
