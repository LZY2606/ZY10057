# State-machine & streaming tests

`tests/state_machine.rs` adds a deterministic differential test suite for
combine's consumption/backtracking semantics (`choice`, `attempt`,
`optional`, `many`/`many1`, `look_ahead`, `token`) and its partial
(streaming) parse machinery. No network, no sleeps, no external services;
the whole file runs in well under 10 seconds.

## Running

```sh
cargo build --all-targets   # preparation (not part of the demo)
cargo test --quiet          # acceptance, from the repository root
```

The acceptance run prints `running 14 tests` for the new suite, and each
test additionally writes a `state_machine::<name>: ...` summary line
directly to stderr (libtest hides captured output of passing tests, so the
phase names are emitted through a direct stderr write).

## How it works

- A fixed-seed xorshift64* RNG generates grammars from `token`, `seq`,
  `choice`, `attempt`, `optional`, `many`, `many1` and `lookahead`.
  The generator rejects any grammar where a repetition body can succeed
  without consuming input (infinite empty production), see
  `nullable`/`has_empty_loop`.
- An independent tree-walking interpreter models the specified combinator
  semantics, including commit flags, backtracking, furthest-error
  positions, and combine's partial-stream rule that end-of-buffer is a
  *committed* "unexpected end of input" (`wrap_stream_error` in
  `src/stream/mod.rs`).
- Generated parsers are compared against the interpreter on success value,
  consumed length and furthest error position, both in batch mode and in
  chunked (cross-buffer) mode. A second chunked harness resumes the same
  partial state across `Pending` polls (the async resume path) and is
  checked for consistency with the streaming semantics.
- Every failure prints the seed, the generated parser (`Debug`), the input
  and the chunking so the case can be reproduced by hand.

Note: on resume, combine's tuple sequences forget the commit flag of
elements completed in earlier polls (`first_empty_parser` is per-poll), so
a committed failure can legitimately become a backtrack across a buffer
boundary (e.g. `optional(seq('b','a'))` streamed as `"b"|"b"` yields
`Ok(None)` where batch yields `Err`). The resume harness therefore asserts
the sound invariant: interpreter-`Ok` implies identical resume output, and
resume never invents an error where the interpreter succeeds.

## Test inventory (14 named tests)

Generated state-machine tests:

- `state_machine_batch_matches_interpreter_seed_a`
- `state_machine_batch_matches_interpreter_seed_b`
- `state_machine_error_positions_match_interpreter`
- `state_machine_chunked_restart_matches_streaming_interpreter`
- `state_machine_chunked_resume_consistent_with_interpreter`
- `state_machine_generator_is_deterministic_for_seed`

Handwritten tests:

- `zero_width_repetition_is_rejected` — zero-width parsers in repetitions
  are rejected by the generator.
- `shared_prefix_branches_backtrack_after_attempt` — shared-prefix branches.
- `commit_after_consumed_input_prevents_backtrack` — failure after commit.
- `buf_reader_handles_chunk_split_mid_token` — `BufReader` refills split a
  token mid-way.
- `io_error_from_reader_is_returned` — underlying I/O error surfaces.
- `async_two_consecutive_pending_then_ready` — two consecutive `Pending`
  polls, then completion.
- `poll_after_eof_returns_same_error` — polling again after EOF is
  deterministic.
- `async_commit_marker_survives_pending_resume` — the commit marker stored
  in `many1`'s partial state survives a `Pending`/resume cycle.

## Mutation checks

Both mutations were applied, verified to fail stably, and then **reverted**
(`git status` shows no source modifications).

### M1 — break `attempt`'s checkpoint restore

Location: `src/parser/choice.rs`, `do_choice!` macro, `PeekErr` arm
(~line 190).

```diff
             PeekErr($head) => {
-                ctry!($input.reset($before.clone()).committed());
                 do_choice!(
```

Result: 6 tests fail stably —
`shared_prefix_branches_backtrack_after_attempt`,
`state_machine_batch_matches_interpreter_seed_a`,
`state_machine_batch_matches_interpreter_seed_b`,
`state_machine_error_positions_match_interpreter`,
`state_machine_chunked_restart_matches_streaming_interpreter`,
`state_machine_chunked_resume_consistent_with_interpreter`.

### M2 — break the consumption marker on async resume

Location: `src/parser/repeat.rs`, `Many1::parse_mode_impl` (~line 481).
`committed_state` is the flag stored in `many1`'s partial state that
records whether the first element consumed input; it is what keeps a
resumed repetition committed across `Pending` polls.

```diff
-            *committed_state = !committed.is_peek();
+            *committed_state = false;
```

Result: 2 tests fail stably —
`async_commit_marker_survives_pending_resume` (handwritten) and
`state_machine_batch_matches_interpreter_seed_b` (generated; the seed was
chosen so the generated grammars exercise this path).
