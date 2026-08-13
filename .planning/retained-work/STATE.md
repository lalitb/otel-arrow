# Retained-Work Pilot State

Updated: 2026-08-12

## Current Work

PR: A - stable delayed-work `LocalResumeId`

Branch: `lalitb/retained-work-local-resume-id`

Worktree:
`/Users/lalitb/work/obs/otel/rust/lalitb/otel-arrow-retained-work-pr-a`

Base: `f2b0348342bd6f0aae619c950dfc93b1f1bb1052` (`origin/main`)

Head: `80ac2a86ffd8dba690e973a7380b758c3e1b1445`

Status: PR A implemented and committed locally; implementation branch not pushed

## Completed

- Observe-only retained-work RFC merged upstream as #3316.
- Pdata retained-memory sizing merged upstream as #3443.
- Opus source review and targeted Phase 2 prior-art review completed.
- Fable architecture decision completed.
- Final decision: constrained pilot with a clean implementation.
- Dedicated coordination worktree and branch created.
- Approved PR A plan committed and pushed on the coordination branch as
  `1337bffbc` without entering the implementation branch ancestry.
- PR A implemented as one atomic commit on a worktree created directly from
  current `origin/main`.

## Implementation Result

Commit `80ac2a86f` changes nine Rust files with 175 insertions and 55 deletions:

- `crates/engine/src/control.rs`
- `crates/engine/src/node_local_scheduler.rs`
- `crates/engine/src/effect_handler.rs`
- `crates/engine/src/local/processor.rs`
- `crates/engine/src/shared/processor.rs`
- `crates/engine/src/pipeline_ctrl.rs`
- `crates/engine/src/message.rs`
- `crates/core-nodes/src/processors/retry_processor/mod.rs`
- `crates/core-nodes/src/processors/batch_processor/mod.rs`

The implementation adds the opaque node-local ID, returns it only for accepted
local scheduling, preserves it through due and shutdown delivery, emits `None`
for all runtime-global delayed delivery, and migrates every exact constructor
and pattern. It does not add memory accounting, metrics, configuration, tenant
behavior, enforcement, or unrelated Phase 2 code.

## Validation

Passed:

- `cargo fmt --all`
- `cargo fmt --all -- --check`
- `cargo check -p otap-df-engine`
- `cargo check -p otap-df-core-nodes`
- `cargo test -p otap-df-engine node_local_scheduler::tests` - 15 passed
- `cargo test -p otap-df-engine processor_inbox_emits_due_delayed_resume_as_control_message`
  - 1 passed
- `cargo test -p otap-df-engine processor_inbox_returns_pending_delayed_resumes_on_shutdown_latch`
  - 1 passed
- `cargo test -p otap-df-engine test_delay_data_integration` - 1 passed
- `cargo test -p otap-df-engine test_new_delay_data_returned_immediately_during_draining`
  - 1 passed
- `cargo test -p otap-df-engine test_queued_delayed_data_flushed_when_draining_begins`
  - 1 passed
- `cargo test -p otap-df-core-nodes test_retry_processor_nacks_then_success_time`
  - 1 passed
- `cargo clippy -p otap-df-engine -p otap-df-core-nodes --all-targets -- -D warnings`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `python3 tools/sanitycheck.py`
- `git diff --check` and `git diff --cached --check`

The first focused scheduler-test compile exposed two ignored
`Result<LocalResumeId, _>` values under `-D unused-results`. Those call sites
were corrected, and the recorded focused and workspace test runs above passed.

`cargo xtask check` was run but did not pass. It stopped at the initial
component-inventory gate because current `origin/main` is missing an inventory
annotation at
`crates/contrib-extensions/src/oauth2_client_auth/mod.rs:100`
(`OTAP_EXTENSION_FACTORIES`). The failure is outside the PR A diff, and the
command did not reach its format, clippy, or test stages. The equivalent full
workspace clippy and test commands were therefore run separately and passed.

As an additional review aid, `scripts/check-async-blocking.sh` exited 1 for
repository-wide existing `std::io`, `std::fs`, and thread-sleep findings. None
is in a PR A changed file.

## PR A Source Inventory

There is one current production caller of `requeue_later`: the retry processor.
The public local and shared processor effect handlers both expose the method,
and both delegate through `EffectHandlerCore` to `NodeLocalSchedulerHandle`.

`NodeControlMsg::DelayedData` currently combines two distinct paths:

- processor-local `requeue_later`, which synchronously transfers a retained
  payload into the bounded node-local scheduler; and
- runtime-global `delay_data`, which asynchronously sends
  `RuntimeControlMsg::DelayData` to the pipeline manager.

The local scheduler produces `DelayedData` when a deadline becomes due and when
shutdown rewrites a pending deadline to the shutdown-start time. The pipeline
manager produces the same variant when global delayed data becomes due, when
queued global data is flushed during drain, and when a new global delay request
arrives after draining has started.

Exact-field consumers that require migration are the retry processor, batch
processor, node-local scheduler tests, processor inbox tests, and pipeline
manager tests. Existing wildcard matches in partition, log sampling, durable
buffer, deterministic control-plane tests, pipeline control stress tests, and
control-channel benchmarks remain source-compatible and need no edit.

## PR A Exact API Plan

1. In `crates/engine/src/control.rs`, add an opaque, copyable, hashable
   `LocalResumeId` newtype over `u64`. Document that it is scheduler-assigned,
   unique only within one processor-local scheduler instance, and meaningful
   only for local delayed requeue. Add
   `resume_id: Option<LocalResumeId>` to `NodeControlMsg::DelayedData`.
2. In `crates/engine/src/node_local_scheduler.rs`, derive the ID from the
   scheduler's existing FIFO sequence, return it only after successful
   insertion, and carry the same `Some(id)` through normal due delivery and
   shutdown conversion. Replace saturating sequence exhaustion with an explicit
   checked invariant so two accepted local resumes can never receive the same
   ID.
3. In `crates/engine/src/effect_handler.rs`, change
   `requeue_later` to return `Result<LocalResumeId, PData>`. Apply the same
   return type to the local and shared processor wrappers in
   `crates/engine/src/local/processor.rs` and
   `crates/engine/src/shared/processor.rs`.
4. In `crates/engine/src/pipeline_ctrl.rs`, set `resume_id: None` on all three
   runtime-global production paths: ordinary due delivery, queued-data drain,
   and immediate return of a delay request received during drain.
5. In `crates/core-nodes/src/processors/retry_processor/mod.rs`, keep accepting
   any successful ID without storing it yet and ignore the returned field on
   delivery. In
   `crates/core-nodes/src/processors/batch_processor/mod.rs`, ignore the new
   field explicitly with `..`. No accounting state is introduced in PR A.

## Delayed-Data Semantics

- `Some(LocalResumeId)` means the payload came from the node-local retained-data
  scheduler. The ID returned by `requeue_later` must equal the ID delivered with
  that same payload.
- `None` means the payload came from the runtime-global `delay_data` path. PR A
  does not assign identity to that path or change its queueing semantics.
- Equal local deadlines remain FIFO. Distinct accepted payloads receive
  distinct IDs even when deadlines are equal.
- Rejection for capacity or latched shutdown returns the original payload and
  returns no ID.
- Local shutdown changes only `when`; it preserves payload and ID. Global drain
  behavior also changes only `when` and keeps `resume_id: None`.
- The ID is correlation metadata, not ordering, memory accounting, ownership,
  configuration, tenant identity, metrics, or an enforcement handle.

## Files and Caller Migration

Expected production edits:

- `crates/engine/src/control.rs`
- `crates/engine/src/node_local_scheduler.rs`
- `crates/engine/src/effect_handler.rs`
- `crates/engine/src/local/processor.rs`
- `crates/engine/src/shared/processor.rs`
- `crates/engine/src/pipeline_ctrl.rs`
- `crates/core-nodes/src/processors/retry_processor/mod.rs`
- `crates/core-nodes/src/processors/batch_processor/mod.rs`

Expected test edits are colocated in `node_local_scheduler.rs` and
`pipeline_ctrl.rs`, plus processor-inbox coverage in
`crates/engine/src/message.rs`. No benchmark source should need modification
because benchmark matches already use `DelayedData { .. }`.

## Test Plan

- Node-local scheduler: successful return/delivery identity, distinct IDs for
  duplicate deadlines, capacity rejection with payload recovery and no ID, and
  shutdown deadline rewrite with identity preservation.
- Processor inbox: due local delivery carries the returned ID; shutdown-latched
  delivery carries the same ID before shutdown; post-shutdown requeue still
  rejects and returns the payload.
- Pipeline manager: normal runtime-global delivery, queued drain flush, and
  delay-during-drain delivery all carry `resume_id: None` while preserving their
  existing deadline behavior.
- Retry processor: existing scheduling/resume behavior remains unchanged after
  the signature and pattern migration.

Every new or modified test declaration must retain specific `Scenario:` and
`Guarantees:` doc comments.

## Compatibility and Review Risks

- Adding a field to a public enum variant breaks exhaustive constructors and
  exact-field patterns. The workspace inventory above covers every current
  occurrence; downstream custom components must add the field when constructing
  the variant or use `..` when matching it.
- Changing `Result<(), PData>` to `Result<LocalResumeId, PData>` can break code
  that requires the exact old result type. Callers using `Ok(_)`, `.is_ok()`, or
  `?` without forcing `()` are straightforward migrations.
- `Option<LocalResumeId>` is intentional compatibility between two existing
  delayed-data mechanisms. Giving global delayed data a fabricated local ID
  would blur scheduler ownership; splitting the enum variant would cause a
  wider consumer migration without improving PR A's invariant.
- The ID must not be inferred from `when`. Duplicate deadlines and shutdown
  rewriting make deadlines unsuitable for correlation.
- Sequence exhaustion must fail explicitly rather than silently reuse an ID.
  It is practically unreachable, but the uniqueness claim should remain true
  by construction.

Residual risk is limited to this intentional source API migration for external
exact constructors, patterns, and callers that require `Result<(), PData>`.
`LocalResumeId` is deliberately opaque and scheduler-local; consumers must not
treat it as globally unique. The repository-required wrapper remains blocked by
the unrelated current-main component-inventory issue even though its underlying
workspace clippy and test stages pass independently.

Estimated size is about 120-180 changed lines across nine Rust files, including
tests. This is an internal generic engine API change with no user-facing
behavior, configuration, or telemetry change, so no changelog entry is planned;
the eventual PR title should include `chore` unless review establishes a
user-facing impact.

## Next Exact Action

Obtain explicit user authorization to push
`lalitb/retained-work-local-resume-id`. After authorization, push the existing
implementation commit without rebasing it onto the coordination branch. Do not
open a GitHub PR, post comments, or request review without separate explicit
authorization.

## Session Handoff

Read, in order:

1. `.planning/threads/retained-work-observe-only-accounting-pilot-and-small-pr-ser.md`
2. `.planning/retained-work/DECISIONS.md`
3. `.planning/retained-work/ROADMAP.md`
4. This file

Work only on the PR named under Current Work. Do not reopen accepted architecture
decisions without contradictory evidence from current source. Preserve unrelated
worktrees and do not place planning commits in implementation branch ancestry.
