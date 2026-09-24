# Filelog physical-line framer

`LineFramer` builds physical lines above the [source decoder](decoder.md). It
is portable and does not register a receiver. The current
[Phase 1 specification][spec] determines behavior.

## Interface and ownership

Construct with `LineConfig` and `LineStart`. Bounds are validated against the
encoding's largest unit and allocation arithmetic. The effective physical-line
bound is `min(max_line_bytes, max_record_bytes)`; equality fits.

```text
next(expected_input_offset, borrowed_input) -> LineStep
complete_partial(Idle | PermanentEof) -> CompletionStep
```

`next` processes at most one source event or retained overflow unit. Advance
caller input by `consumed`, including on failure. Continue while `advanced` is
true, even if consumption is zero and no output was returned. Empty-input calls
drain buffered work but never finish incomplete input. Callers can stop after
any step: the framer borrows no input and owns no output queue. A scheduler must
bound its own loop/source turn.

Each output transfers an owned body and exact half-open **source-byte** ranges.
LF belongs to the frame, never the body; CR and NUL remain body data. A matching
initial BOM belongs to the first frame but not its body. Empty lines still own
LF. `source_body` retains exact bytes alongside clean preserve-raw text so a
later multiline consumer can change the grouped representation without rereading
source. Byte bodies themselves provide that evidence. Consumers own and account
for both allocations by **capacity**, including spare capacity, after transfer.
Body lengths determine framing limits, not physical memory charges.

The input position, decoder-delivered boundary and returned frame boundary are
separate. For UTF-16 `AB`, a high surrogate, then `C`, replacement with a four-byte
limit can consume through offset 8 while emitting `AB` through offset 4. The
framer retains the replacement unit at `[4,6)`; the decoder retains `C` at
`[6,8)`. Continue live input at 8. Do not discard either pending unit or resume
live input at 4. `pending_source_start` is the first byte not owned by output,
not a committed frontier.

## States and completion

| State | Body unit | LF |
| --- | --- | --- |
| Buffered line | Append if it fits; otherwise enter split/truncate | Emit line |
| Split | Append if it fits; otherwise return nonfinal fragment and retain overflow unit | Emit final fragment |
| Truncate | Validate and count; retain no tail bytes | Emit prefix with complete frame ownership |
| Terminal error | Return sticky failure with zero consumption | Same |

Prospective preserve-raw sizing uses `max(decoded UTF-8 bytes, source bytes)`.
Every preserve-raw split fragment uses exact source bytes, even before a later
malformed unit is found. A clean unsplit/truncated prefix can remain text.
Malformed truncate tails count toward `malformed_units` without changing a
clean prefix's representation. `discarded_source_bytes` excludes terminal LF.

Truncate emits only after its entire deterministic tail has been validated.
A fail-policy error suppresses that record's prefix. Earlier returned frames
remain owned by the caller and must resolve before quarantine. A malformed unit
that would overflow an unfinished split prefix does not itself authorize that
prefix: decoding fails before a safe boundary can be established.

Only caller-established idle eligibility or permanent rotation EOF authorizes
`complete_partial`. It drains earlier events before resolving an incomplete
unit. Repeat with the same reason until `complete`, retaining every output.
Fresh input and reason changes are rejected while completion is active. Returned
partial output distinguishes `Idle` from terminal-unterminated `PermanentEof`.
Subsequent bytes start a new line/unit without BOM probing. Empty completion is
idempotent; a resumed split with no newly observed bytes remains pending. A
BOM-only pending source range can complete as an empty body owning that range.
Shutdown, read pauses, empty chunks and ordinary EOF grant no such authority.

`CompletionInProgress` and the input-offset `Decode(OffsetDiscontinuity)`
reported by `next` are recoverable caller errors and leave the framer usable.
Malformed-input, source-offset overflow, allocation and fragment-index failures
are terminal. `terminal_error()` reports whether this instance is stopped.
Constructor validation errors never create an instance.

The `Decode` variant preserves the decoder's error type, but recovery follows
the framer contract. In particular, `DrainRequired` cannot arise during normal
framer operation: completion drains first. If an internal decoder call violates
that invariant, its error is latched rather than inviting a caller retry.

## Continuation and consumer boundaries

A nonfinal fragment provides the original frame start and next `u32` fragment
index. Record projection combines the origin with file identity/epoch for the
specified correlation ID. Index overflow fails before a nonfinal fragment is
emitted; a final fragment at `u32::MAX` is allowed.

Delivery integration may propose this frame range and continuation only after
the output is accepted into its bookkeeping. Applied/durable advancement still
requires the specified Ack, transaction and sync rules. Checkpoint integration
maps `LineContinuation` to the existing scan-to-LF form (`record_end_offset ==
0`); this primitive neither imports nor changes checkpoint formats.

For restart, validate identity, continuity, profile compatibility and a safe
source-unit boundary, then construct `LineStart::Continuation` at the returned
frame end. Reread only surviving bytes from that boundary; decoding never
re-probes a BOM. The original prefix is not emitted again. Volatile decoder
lookahead is retained during ordinary pauses. Reconstructing at a returned
boundary is a separate caller-authorized replay operation, not proof that the
live decoder is empty. No API here authorizes dropping pending failure state.

Multiline grouping combines ordinary line frames and preserves internal
separators (the configured encoding determines their exact LF bytes). It must
resolve earlier buffered multiline content before accepting an oversized
line's fragments or applying a later failure. It owns patterns, aggregate
record bounds, known-end multiline continuation and grouping metadata.
Scheduling and lifecycle code decide eligibility, source turns and rotation.
Receiver memory accounting owns aggregate retention.

## Resource bounds

The framer has one text buffer and, only for preserve-raw text, one exact-source
buffer. Raw mode has one byte buffer. Each grows geometrically with an explicit
configured cap; retained payload is at most two effective bounds, plus inline
decoder state and one event. Truncate memory does not grow with the tail. There
is no per-unit heap allocation, rescanning, synchronization or retained input
borrow. `retained_capacity` reports owned heap capacities; include
`size_of::<LineFramer>()` for inline storage. Caller scratch, returned outputs,
allocator overhead and transient old/new allocations during growth are separate
charges. Reservation/aggregate admission belongs to the receiver.

After emitting a preserve-raw byte body, the framer clears its text buffer but
retains its capacity for reuse, up to the effective bound. This allocation
remains charged even at an otherwise empty line boundary. A later text output
transfers that buffer, so its capacity can greatly exceed its body length.
Account for returned `String`/`Vec` capacities and any `source_body` capacity;
do not assume a fixed capacity-to-length ratio. Retained framer allocations
remain charged until transferred or the framer is dropped.

Allocation failure is terminal and returns no frame. Offsets are checked by the
source decoder; malformed counts cannot exceed the distinct bytes in a `u64`
source range. Bound checks use subtraction before appending, and fragment
indices use checked arithmetic.

Tests use independently specified bodies/ranges and exhaustive partitions of
small fixtures, exercise continuation reconstruction and bounded long-line
retention, and separately measure allocation-free truncate scanning. These are
primitive checks, not full receiver performance qualification.

[spec]: ../../../../../../docs/filelog-receiver-phase1-spec.md#source-decoding-and-framing
