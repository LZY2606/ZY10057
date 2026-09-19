# Testing

## Acceptance commands

Run the preparation build from the repository root:

```sh
cargo build --all-targets
```

Acceptance uses the default feature set and requires no network access, running
services, extra environment variables, or timers:

```sh
cargo test --quiet
```

The added state-machine coverage is in `tests/state_machine.rs`. Its tests
include `generated_parsers_match_naive_interpreter`,
`generated_parsers_match_across_partial_buffers`,
`attempt_restores_checkpoint_for_shared_prefix_choice`, and
`partial_choice_committed_failure_resumes_selected_branch`. Deterministic async
buffer tests are source unit tests in `src/stream/buf_reader.rs`, including
`two_consecutive_pending_polls_do_not_consume_data`,
`bufferless_reader_preserves_bytes_split_inside_a_token`,
`bufferless_reader_forwards_io_error`, and
`polling_after_eof_keeps_returning_zero`.

The generator uses a fixed xorshift seed, builds parsers from
`token`/`choice`/`attempt`/`optional`/`many`/`lookahead`, and rejects grammars
where `many` can repeat a parser that succeeds without consuming a token.
Failure panics include a copyable parser description, input, and seed.

## Mutation points

These temporary mutations were each applied separately and then reverted. Both
targeted tests failed reliably, while the unchanged source passes
`cargo test --quiet`.

1. Attempt checkpoint restore — `src/parser/combinator.rs`,
   `Try::parse_mode_impl`: replace the `CommitErr` handling with
   `CommitErr(err) => CommitErr(err)`. This suppresses conversion back to a
   peek failure and prevents the shared-prefix alternative from running.
   Failing test: `attempt_restores_checkpoint_for_shared_prefix_choice`.

2. Async recovery consumption marker — `tests/state_machine.rs`,
   `ResumableAx::parse_mode_impl`: change the first-parse marker from
   `state.0 = true` to `state.0 = false`. After `a` is consumed and partial EOF
   is hit, the second poll cannot tell that the parser committed and must resume
   at `x`; it restarts as if at a fresh token boundary. Failing test:
   `partial_choice_committed_failure_resumes_selected_branch`.
