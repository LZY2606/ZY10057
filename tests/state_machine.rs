//! Differential state-machine tests for combine's consumption/backtracking
//! semantics and its partial (streaming) parse machinery.
//!
//! A deterministic generator (fixed seed, no external dependencies) builds
//! "safe" parsers out of `token`, `choice`, `attempt`, `optional`, `many`,
//! `many1` and `look_ahead` and compares combine against an independent,
//! naive tree-walking interpreter. The interpreter models the *specified*
//! semantics of each combinator (commit flags, backtracking, eager
//! end-of-input behaviour on partial streams) so the two implementations
//! only agree if combine's machinery is correct.
//!
//! Every comparison prints the seed, the generated parser and the input on
//! failure so the failing case can be reproduced by hand.
//!
//! The handwritten tests at the bottom pin down specific state
//! combinations: zero-width parsers in repetitions, shared prefixes,
//! commit-after-failure, `BufReader` chunk boundaries, I/O errors and
//! async-style `Pending`/`EOF` polling sequences.

#![cfg(feature = "std")]

use std::io::{self, Read};

use combine::{
    attempt, choice, many1,
    parser::{
        byte::byte,
        choice::optional,
        combinator::{any_partial_state, look_ahead, AnyPartialState},
        repeat::many,
    },
    stream::{
        buf_reader::BufReader,
        decode,
        decoder::{self, Decoder},
        easy, MaybePartialStream, PointerOffset,
    },
    parser::EasyParser,
    Parser, Stream,
};

/// Prints a one-line verification-phase summary directly to stderr so the
/// phase names are visible even under `cargo test --quiet` (libtest only
/// shows captured output for failing tests; a direct stderr write bypasses
/// the capture).
fn note_phase(name: &str, detail: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "state_machine::{}: {}", name, detail);
}

// ---------------------------------------------------------------------------
// Deterministic RNG (xorshift64*). No external crates, fully reproducible.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ---------------------------------------------------------------------------
// Grammar AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Node {
    Token(u8),
    Seq(Vec<Node>),
    Choice(Vec<Node>),
    Attempt(Box<Node>),
    Optional(Box<Node>),
    Many(Box<Node>),
    Many1(Box<Node>),
    Lookahead(Box<Node>),
}

/// `true` if the node can succeed without consuming any input. Such a node
/// must never be placed directly under `Many`/`Many1` or the repetition
/// would produce empty values forever.
fn nullable(node: &Node) -> bool {
    match node {
        Node::Token(_) => false,
        Node::Seq(children) => children.iter().all(nullable),
        Node::Choice(children) => children.iter().any(nullable),
        Node::Attempt(child) => nullable(child),
        Node::Optional(_) | Node::Many(_) | Node::Lookahead(_) => true,
        Node::Many1(child) => nullable(child),
    }
}

/// `true` if the grammar contains a repetition whose body may succeed
/// without consuming input (an infinite empty-production loop).
fn has_empty_loop(node: &Node) -> bool {
    match node {
        Node::Many(child) | Node::Many1(child) => nullable(child) || has_empty_loop(child),
        Node::Seq(children) | Node::Choice(children) => children.iter().any(has_empty_loop),
        Node::Attempt(child) | Node::Optional(child) | Node::Lookahead(child) => {
            has_empty_loop(child)
        }
        Node::Token(_) => false,
    }
}

fn gen_node(rng: &mut Rng, depth: usize) -> Node {
    if depth == 0 {
        return Node::Token(b"ab"[rng.below(2)]);
    }
    match rng.below(10) {
        0 | 1 | 2 => Node::Token(b"ab"[rng.below(2)]),
        3 => Node::Seq((0..2 + rng.below(2)).map(|_| gen_node(rng, depth - 1)).collect()),
        4 => Node::Choice(
            (0..2 + rng.below(2))
                .map(|_| gen_node(rng, depth - 1))
                .collect(),
        ),
        5 => Node::Attempt(Box::new(gen_node(rng, depth - 1))),
        6 => Node::Optional(Box::new(gen_node(rng, depth - 1))),
        7 => Node::Many(Box::new(gen_node(rng, depth - 1))),
        8 => Node::Many1(Box::new(gen_node(rng, depth - 1))),
        _ => Node::Lookahead(Box::new(gen_node(rng, depth - 1))),
    }
}

/// Generates a grammar, rejecting any grammar where a repetition could
/// produce empty values forever.
fn gen_grammar(rng: &mut Rng) -> Node {
    for _ in 0..100 {
        let node = gen_node(rng, 3);
        if !has_empty_loop(&node) {
            return node;
        }
    }
    Node::Token(b'a')
}

fn collect_tokens(node: &Node, out: &mut Vec<u8>) {
    match node {
        Node::Token(b) => out.push(*b),
        Node::Seq(children) | Node::Choice(children) => {
            for child in children {
                collect_tokens(child, out);
            }
        }
        Node::Attempt(child)
        | Node::Optional(child)
        | Node::Many(child)
        | Node::Many1(child)
        | Node::Lookahead(child) => collect_tokens(child, out),
    }
}

/// Generates an input. Half of the inputs are biased towards token bytes
/// occurring in the grammar so both success and failure paths are hit.
fn gen_input(rng: &mut Rng, grammar: &Node) -> Vec<u8> {
    let mut tokens = Vec::new();
    collect_tokens(grammar, &mut tokens);
    let len = rng.below(13);
    let guided = rng.below(2) == 0;
    (0..len)
        .map(|_| {
            if guided && !tokens.is_empty() && rng.below(4) != 0 {
                tokens[rng.below(tokens.len())]
            } else {
                b"ab"[rng.below(2)]
            }
        })
        .collect()
}

/// Generates a chunking of an input of length `len` (each chunk >= 1 byte).
fn gen_chunks(rng: &mut Rng, len: usize) -> Vec<usize> {
    let mut chunks = Vec::new();
    let mut remaining = len;
    while remaining > 0 {
        let chunk = 1 + rng.below(remaining);
        chunks.push(chunk);
        remaining -= chunk;
        if chunks.len() >= 4 && remaining > 0 {
            chunks.push(remaining);
            break;
        }
    }
    chunks
}

/// Absolute buffer limits for each successive poll; always ends at `len`
/// (the final poll sees EOF).
fn chunk_limits(len: usize, chunks: &[usize]) -> Vec<usize> {
    let mut limits = Vec::new();
    let mut current = 0;
    for &chunk in chunks {
        current = (current + chunk).min(len);
        limits.push(current);
        if current == len {
            break;
        }
    }
    if limits.last() != Some(&len) {
        limits.push(len);
    }
    limits
}

// ---------------------------------------------------------------------------
// Naive interpreter: an independent specification of the combinator
// semantics. `limit` models the end of the currently available buffer of a
// partial stream (`limit == input.len()` means EOF).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Out {
    Ok {
        val: String,
        pos: usize,
        committed: bool,
    },
    Err {
        committed: bool,
        /// `true` if the decisive error is an unexpected end of input.
        eoi: bool,
        furthest: usize,
    },
}

fn eval(node: &Node, input: &[u8], pos: usize, limit: usize, fuel: &mut u64) -> Out {
    *fuel = fuel
        .checked_sub(1)
        .expect("interpreter fuel exhausted (grammar loops?)");
    let partial = limit < input.len();
    match node {
        Node::Token(b) => {
            if pos < limit {
                if input[pos] == *b {
                    Out::Ok {
                        val: (*b as char).to_string(),
                        pos: pos + 1,
                        committed: true,
                    }
                } else {
                    // A mismatch never consumes input.
                    Out::Err {
                        committed: false,
                        eoi: false,
                        furthest: pos,
                    }
                }
            } else if partial {
                // `wrap_stream_error`: hitting the end of a partial buffer
                // is a *committed* "unexpected end of input" error so that
                // `many`/`optional`/`choice` wait for more input instead of
                // eagerly succeeding or falling through.
                Out::Err {
                    committed: true,
                    eoi: true,
                    furthest: pos,
                }
            } else {
                // At EOF the same situation is a plain non-committed error.
                Out::Err {
                    committed: false,
                    eoi: true,
                    furthest: pos,
                }
            }
        }
        Node::Seq(children) => {
            let mut current = pos;
            let mut committed = false;
            let mut val = String::new();
            for child in children {
                match eval(child, input, current, limit, fuel) {
                    Out::Ok {
                        val: v,
                        pos: p,
                        committed: c,
                    } => {
                        val.push_str(&v);
                        current = p;
                        committed |= c;
                    }
                    Out::Err {
                        committed: c,
                        eoi,
                        furthest,
                    } => {
                        return Out::Err {
                            committed: committed || c,
                            eoi,
                            furthest,
                        }
                    }
                }
            }
            Out::Ok {
                val,
                pos: current,
                committed,
            }
        }
        Node::Choice(children) => {
            let mut best: Option<(usize, bool)> = None;
            for child in children {
                match eval(child, input, pos, limit, fuel) {
                    ok @ Out::Ok { .. } => return ok,
                    // A committed failure aborts the choice immediately; the
                    // errors of earlier alternatives are discarded.
                    Out::Err {
                        committed: true,
                        eoi,
                        furthest,
                    } => {
                        return Out::Err {
                            committed: true,
                            eoi,
                            furthest,
                        }
                    }
                    Out::Err {
                        committed: false,
                        eoi,
                        furthest,
                    } => {
                        best = Some(match best {
                            Some((f, e)) if f > furthest => (f, e),
                            Some((f, e)) if f == furthest => (f, e || eoi),
                            _ => (furthest, eoi),
                        });
                    }
                }
            }
            let (furthest, eoi) = best.expect("choice without alternatives");
            Out::Err {
                committed: false,
                eoi,
                furthest,
            }
        }
        Node::Attempt(child) => match eval(child, input, pos, limit, fuel) {
            // On a partial stream an unexpected end of input stays committed
            // (the parser may still succeed once more input arrives);
            // otherwise `attempt` downgrades the failure to non-committed.
            Out::Err {
                committed: true,
                eoi,
                furthest,
            } if !(eoi && partial) => Out::Err {
                committed: false,
                eoi,
                furthest,
            },
            other => other,
        },
        Node::Optional(child) => match eval(child, input, pos, limit, fuel) {
            Out::Ok {
                val,
                pos: p,
                committed,
            } => Out::Ok {
                val: format!("Some({})", val),
                pos: p,
                committed,
            },
            Out::Err {
                committed: true,
                eoi,
                furthest,
            } => Out::Err {
                committed: true,
                eoi,
                furthest,
            },
            Out::Err { .. } => Out::Ok {
                val: "None".to_string(),
                pos,
                committed: false,
            },
        },
        Node::Many(child) | Node::Many1(child) => {
            let minimum = if matches!(node, Node::Many1(_)) { 1 } else { 0 };
            let mut current = pos;
            let mut committed = false;
            let mut items: Vec<String> = Vec::new();
            loop {
                match eval(child, input, current, limit, fuel) {
                    Out::Ok {
                        val,
                        pos: p,
                        committed: c,
                    } => {
                        assert!(
                            p > current,
                            "repetition body succeeded without consuming input"
                        );
                        items.push(val);
                        current = p;
                        committed |= c;
                    }
                    Out::Err {
                        committed: c,
                        eoi,
                        furthest,
                    } => {
                        if c {
                            return Out::Err {
                                committed: true,
                                eoi,
                                furthest,
                            };
                        }
                        if items.len() < minimum {
                            return Out::Err {
                                committed,
                                eoi,
                                furthest,
                            };
                        }
                        break;
                    }
                }
            }
            Out::Ok {
                val: format!("[{}]", items.join(",")),
                pos: current,
                committed,
            }
        }
        Node::Lookahead(child) => match eval(child, input, pos, limit, fuel) {
            Out::Ok { val, .. } => Out::Ok {
                val: format!("Look({})", val),
                pos,
                committed: false,
            },
            // `look_ahead` restores the input but propagates the failure
            // (including its commit flag) unchanged.
            err @ Out::Err { .. } => err,
        },
    }
}

fn eval_batch(node: &Node, input: &[u8]) -> Result<(String, usize), usize> {
    let mut fuel = 1_000_000;
    match eval(node, input, 0, input.len(), &mut fuel) {
        Out::Ok { val, pos, .. } => Ok((val, pos)),
        Out::Err { furthest, .. } => Err(furthest),
    }
}

/// Mirrors the poll loop of a streaming decoder: each limit is one poll.
/// A non-committed unexpected-end-of-input before EOF means "Pending".
fn eval_streaming(node: &Node, input: &[u8], limits: &[usize]) -> Result<(String, usize), usize> {
    for &limit in limits {
        let mut fuel = 1_000_000;
        match eval(node, input, 0, limit, &mut fuel) {
            Out::Ok { val, pos, .. } => return Ok((val, pos)),
            Out::Err { eoi: true, .. } if limit < input.len() => continue,
            Out::Err { furthest, .. } => return Err(furthest),
        }
    }
    unreachable!("chunk_limits always ends at the input length");
}

// ---------------------------------------------------------------------------
// Compilation of the AST into a real combine parser.
// ---------------------------------------------------------------------------

type BoxParser<'a, I> = Box<dyn Parser<I, Output = String, PartialState = AnyPartialState> + 'a>;

fn compile<'a, I>(node: &Node) -> BoxParser<'a, I>
where
    I: Stream<Token = u8> + 'a,
{
    match node {
        Node::Token(b) => {
            let b = *b;
            any_partial_state(byte(b).map(move |t| (t as char).to_string())).boxed()
        }
        Node::Seq(children) => {
            let mut iter = children.iter();
            let mut acc = compile::<I>(iter.next().expect("seq without children"));
            for child in iter {
                let next = compile::<I>(child);
                acc = any_partial_state(acc.and(next).map(|(a, b)| a + &b)).boxed();
            }
            acc
        }
        Node::Choice(children) => {
            let mut iter = children.iter();
            let mut acc = compile::<I>(iter.next().expect("choice without children"));
            for child in iter {
                let next = compile::<I>(child);
                acc = any_partial_state(acc.or(next)).boxed();
            }
            acc
        }
        Node::Attempt(child) => any_partial_state(attempt(compile::<I>(child))).boxed(),
        Node::Optional(child) => any_partial_state(optional(compile::<I>(child)).map(|o| {
            match o {
                Some(s) => format!("Some({})", s),
                None => "None".to_string(),
            }
        }))
        .boxed(),
        Node::Many(child) => any_partial_state(
            many::<Vec<String>, _, _>(compile::<I>(child))
                .map(|items| format!("[{}]", items.join(","))),
        )
        .boxed(),
        Node::Many1(child) => any_partial_state(
            many1::<Vec<String>, _, _>(compile::<I>(child))
                .map(|items| format!("[{}]", items.join(","))),
        )
        .boxed(),
        Node::Lookahead(child) => {
            any_partial_state(look_ahead(compile::<I>(child)).map(|s| format!("Look({})", s)))
                .boxed()
        }
    }
}

// ---------------------------------------------------------------------------
// Test harnesses driving combine.
// ---------------------------------------------------------------------------

type EasySlice<'a> = easy::Stream<&'a [u8]>;
type EasyPartialSlice<'a> = easy::Stream<MaybePartialStream<&'a [u8]>>;

/// Batch parse of the whole input (EOF semantics).
fn run_batch(node: &Node, input: &[u8]) -> Result<(String, usize), usize> {
    let mut parser = compile::<EasySlice>(node);
    match parser.easy_parse(&input[..]) {
        Ok((value, rest)) => Ok((value, input.len() - rest.len())),
        Err(err) => Err(err.position.translate_position(&input[..])),
    }
}

/// Parses chunk by chunk with a *fresh* partial state per poll (the whole
/// prefix is re-parsed each time the buffer grows). Equivalent to
/// `parse_lazy`'s restart behaviour on partial streams.
fn run_chunked_restart(node: &Node, input: &[u8], limits: &[usize]) -> Result<(String, usize), usize> {
    let mut parser = compile::<EasyPartialSlice>(node);
    for &limit in limits {
        let eof = limit == input.len();
        let mut state = AnyPartialState::default();
        let mut stream = easy::Stream(MaybePartialStream(&input[..limit], !eof));
        match decode(&mut *parser, &mut stream, &mut state) {
            Ok((Some(value), consumed)) => return Ok((value, consumed)),
            Ok((None, _)) => assert!(!eof, "decode returned `None` (Pending) at EOF"),
            Err(err) => return Err(err.position.translate_position(&input[..limit])),
        }
    }
    unreachable!("chunk_limits always ends at the input length");
}

/// Parses chunk by chunk resuming the *same* partial state after every
/// `Pending`, removing committed bytes from the buffer (the way the
/// `decode!` macro drives a `Decoder`). Must agree with the restart
/// harness: resuming partial state must be equivalent to restarting.
fn run_chunked_resume(node: &Node, input: &[u8], limits: &[usize]) -> Result<(String, usize), ()> {
    let mut parser = compile::<EasyPartialSlice>(node);
    let mut state = AnyPartialState::default();
    let mut start = 0;
    let mut consumed = 0;
    for &limit in limits {
        let eof = limit == input.len();
        let mut stream = easy::Stream(MaybePartialStream(&input[start..limit], !eof));
        match decode(&mut *parser, &mut stream, &mut state) {
            Ok((Some(value), removed)) => {
                consumed += removed;
                return Ok((value, consumed));
            }
            Ok((None, removed)) => {
                assert!(!eof, "decode returned `None` (Pending) at EOF");
                start += removed;
                consumed += removed;
            }
            Err(_) => return Err(()),
        }
    }
    unreachable!("chunk_limits always ends at the input length");
}

fn describe_case(seed: u64, case: usize, grammar: &Node, input: &[u8]) -> String {
    format!(
        "\nseed: {:#x}\ncase: {}\nparser: {:#?}\ninput: {:?} (bytes {:?})",
        seed,
        case,
        grammar,
        String::from_utf8_lossy(input),
        input
    )
}

fn run_batch_suite(name: &str, seed: u64, grammars: usize, inputs_per_grammar: usize) {
    let mut rng = Rng::new(seed);
    let (mut successes, mut failures) = (0, 0);
    for case in 0..grammars {
        let grammar = gen_grammar(&mut rng);
        for _ in 0..inputs_per_grammar {
            let input = gen_input(&mut rng, &grammar);
            let expected = eval_batch(&grammar, &input);
            let actual = run_batch(&grammar, &input);
            match &expected {
                Ok(_) => successes += 1,
                Err(_) => failures += 1,
            }
            assert_eq!(
                expected,
                actual,
                "batch parse disagrees with the interpreter{}",
                describe_case(seed, case, &grammar, &input)
            );
        }
    }
    assert!(
        successes > 0 && failures > 0,
        "suite must exercise both success and failure paths (seed {:#x}): ok={} err={}",
        seed,
        successes,
        failures
    );
    note_phase(
        name,
        &format!(
            "seed {:#x}, {} cases compared ({} ok / {} err)",
            seed,
            successes + failures,
            successes,
            failures
        ),
    );
}

fn run_chunked_suite(name: &str, seed: u64, grammars: usize, inputs_per_grammar: usize) {
    let mut rng = Rng::new(seed);
    let (mut successes, mut failures) = (0, 0);
    for case in 0..grammars {
        let grammar = gen_grammar(&mut rng);
        for _ in 0..inputs_per_grammar {
            let input = gen_input(&mut rng, &grammar);
            let chunks = gen_chunks(&mut rng, input.len());
            let limits = chunk_limits(input.len(), &chunks);
            let description = format!(
                "{}\nchunks: {:?}",
                describe_case(seed, case, &grammar, &input),
                chunks
            );

            let expected = eval_streaming(&grammar, &input, &limits);
            let restart = run_chunked_restart(&grammar, &input, &limits);
            match &expected {
                Ok(_) => successes += 1,
                Err(_) => failures += 1,
            }
            assert_eq!(
                expected, restart,
                "chunked parse (restart) disagrees with the streaming interpreter{}",
                description
            );

            // Resuming partial state must agree with the streaming
            // semantics wherever the semantics succeed, and must never
            // invent an error where the semantics succeed. (The reverse
            // does not hold: combine's tuple sequences forget the commit
            // flag of elements completed in earlier polls, so a committed
            // failure may legitimately become a backtrack on resume.)
            let resume = run_chunked_resume(&grammar, &input, &limits);
            match (&expected, &resume) {
                (Ok(expected_value), Ok(actual_value)) => assert_eq!(
                    expected_value, actual_value,
                    "resuming partial state produced a different value{}",
                    description
                ),
                (Ok(expected_value), Err(_)) => panic!(
                    "resuming partial state failed where the streaming \
                     semantics succeeded with {:?}{}",
                    expected_value, description
                ),
                (Err(_), _) => (),
            }
        }
    }
    assert!(
        successes > 0 && failures > 0,
        "suite must exercise both success and failure paths (seed {:#x}): ok={} err={}",
        seed,
        successes,
        failures
    );
    note_phase(
        name,
        &format!(
            "seed {:#x}, {} chunked cases compared ({} ok / {} err)",
            seed,
            successes + failures,
            successes,
            failures
        ),
    );
}

// ---------------------------------------------------------------------------
// State-machine tests (generated grammars, fixed seeds)
// ---------------------------------------------------------------------------

#[test]
fn state_machine_batch_matches_interpreter_seed_a() {
    run_batch_suite("batch_matches_interpreter_seed_a", 0xC0FF_EE01, 120, 4);
}

#[test]
fn state_machine_batch_matches_interpreter_seed_b() {
    run_batch_suite("batch_matches_interpreter_seed_b", 0x5EED_0004, 120, 4);
}

#[test]
fn state_machine_error_positions_match_interpreter() {
    // Dedicated seed; also asserts that a meaningful share of the generated
    // cases actually fail so error positions are really compared.
    let seed = 0xC0FF_EE03;
    let mut rng = Rng::new(seed);
    let mut compared_failures = 0;
    for case in 0..120 {
        let grammar = gen_grammar(&mut rng);
        for _ in 0..4 {
            let input = gen_input(&mut rng, &grammar);
            let expected = eval_batch(&grammar, &input);
            let actual = run_batch(&grammar, &input);
            if expected.is_err() {
                compared_failures += 1;
            }
            assert_eq!(
                expected,
                actual,
                "error position disagrees with the interpreter{}",
                describe_case(seed, case, &grammar, &input)
            );
        }
    }
    assert!(
        compared_failures >= 50,
        "expected at least 50 failing cases to compare error positions on, got {}",
        compared_failures
    );
    note_phase(
        "error_positions_match_interpreter",
        &format!("{} failing cases compared", compared_failures),
    );
}

#[test]
fn state_machine_chunked_restart_matches_streaming_interpreter() {
    run_chunked_suite(
        "chunked_restart_matches_streaming_interpreter",
        0xC0FF_EE04,
        100,
        3,
    );
}

#[test]
fn state_machine_chunked_resume_consistent_with_interpreter() {
    run_chunked_suite(
        "chunked_resume_consistent_with_interpreter",
        0xC0FF_EE05,
        100,
        3,
    );
}

#[test]
fn state_machine_generator_is_deterministic_for_seed() {
    let mut rng_a = Rng::new(0xC0FF_EE06);
    let mut rng_b = Rng::new(0xC0FF_EE06);
    for _ in 0..50 {
        let grammar_a = gen_grammar(&mut rng_a);
        let grammar_b = gen_grammar(&mut rng_b);
        assert_eq!(
            format!("{:?}", grammar_a),
            format!("{:?}", grammar_b),
            "same seed must generate the same parser"
        );
        let input_a = gen_input(&mut rng_a, &grammar_a);
        let input_b = gen_input(&mut rng_b, &grammar_b);
        assert_eq!(input_a, input_b, "same seed must generate the same input");
    }
    note_phase("generator_is_deterministic_for_seed", "50 grammars compared");
}

// ---------------------------------------------------------------------------
// Handwritten tests
// ---------------------------------------------------------------------------

#[test]
fn zero_width_repetition_is_rejected() {
    let token_a = Node::Token(b'a');
    let token_b = Node::Token(b'b');

    // Repetitions of parsers that may succeed without consuming input must
    // be rejected (they would produce empty values forever).
    assert!(has_empty_loop(&Node::Many(Box::new(Node::Optional(
        Box::new(token_a.clone())
    )))));
    assert!(has_empty_loop(&Node::Many(Box::new(Node::Many(Box::new(
        token_a.clone()
    ))))));
    assert!(has_empty_loop(&Node::Many1(Box::new(Node::Lookahead(
        Box::new(token_a.clone())
    )))));
    assert!(has_empty_loop(&Node::Many(Box::new(Node::Attempt(Box::new(
        Node::Optional(Box::new(token_a.clone()))
    ))))));
    assert!(has_empty_loop(&Node::Many(Box::new(Node::Choice(vec![
        token_a.clone(),
        Node::Optional(Box::new(token_b.clone())),
    ])))));
    // Nested inside an otherwise fine grammar.
    assert!(has_empty_loop(&Node::Seq(vec![
        token_a.clone(),
        Node::Many(Box::new(Node::Lookahead(Box::new(token_b.clone())))),
    ])));

    // These are safe: the body always consumes input when it succeeds.
    assert!(!has_empty_loop(&Node::Many(Box::new(Node::Seq(vec![
        Node::Optional(Box::new(token_a.clone())),
        token_b.clone(),
    ])))));
    assert!(!has_empty_loop(&Node::Many1(Box::new(Node::Attempt(
        Box::new(Node::Seq(vec![token_a.clone(), token_b.clone()]))
    )))));
    assert!(!has_empty_loop(&Node::Many(Box::new(token_a.clone()))));

    // The generator itself must never produce an empty-looping grammar.
    let mut rng = Rng::new(0xC0FF_EE07);
    for _ in 0..500 {
        let grammar = gen_grammar(&mut rng);
        assert!(
            !has_empty_loop(&grammar),
            "generator produced an empty-looping grammar: {:?}",
            grammar
        );
    }
    note_phase(
        "zero_width_repetition_is_rejected",
        "6 empty-looping shapes rejected, 500 generated grammars validated",
    );
}

#[test]
fn shared_prefix_branches_backtrack_after_attempt() {
    // `attempt` must restore the input when its branch fails so the second
    // branch can re-read the shared prefix.
    let mut parser = choice((
        attempt(byte(b'a').with(byte(b'b'))),
        byte(b'a').with(byte(b'c')),
    ));
    let (output, rest) = parser
        .easy_parse(&b"ac"[..])
        .expect("attempt must backtrack over the shared prefix");
    assert_eq!(output, b'c');
    assert_eq!(rest, &b""[..]);
    note_phase(
        "shared_prefix_branches_backtrack_after_attempt",
        "attempt backtracked over the shared prefix",
    );
}

#[test]
fn commit_after_consumed_input_prevents_backtrack() {
    // Without `attempt`, consuming 'a' commits to the first branch: the
    // parse must fail at offset 1 instead of trying the second branch.
    let mut parser = choice((byte(b'a').with(byte(b'b')), byte(b'a').with(byte(b'c'))));
    let err = parser
        .easy_parse(&b"ac"[..])
        .expect_err("a committed branch must not backtrack");
    assert_eq!(err.position.translate_position(&b"ac"[..]), 1);
    note_phase(
        "commit_after_consumed_input_prevents_backtrack",
        "committed failure reported at offset 1",
    );
}

// ---------------------------------------------------------------------------
// Handwritten tests: I/O and async-style polling
// ---------------------------------------------------------------------------

/// A `Read` that returns one predetermined chunk per `read` call, then
/// either an error or EOF. Deterministic, no threads, no sleeping.
struct ChunkedRead<'a> {
    chunks: Vec<&'a [u8]>,
    error: Option<io::Error>,
    reads: usize,
}

impl<'a> ChunkedRead<'a> {
    fn new(chunks: Vec<&'a [u8]>) -> Self {
        ChunkedRead {
            chunks,
            error: None,
            reads: 0,
        }
    }

    fn failing_after(chunks: Vec<&'a [u8]>, error: io::Error) -> Self {
        ChunkedRead {
            chunks,
            error: Some(error),
            reads: 0,
        }
    }
}

impl<'a> Read for ChunkedRead<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        if self.chunks.is_empty() {
            if let Some(error) = self.error.take() {
                return Err(error);
            }
            return Ok(0);
        }
        let chunk = self.chunks.remove(0);
        let n = chunk.len().min(buf.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        if n < chunk.len() {
            self.chunks.insert(0, &chunk[n..]);
        }
        Ok(n)
    }
}

#[test]
fn buf_reader_handles_chunk_split_mid_token() {
    // The reader returns "he", "l", "lo": the 5-byte token "hello" is split
    // across three buffer refills and must still parse as one token.
    let chunked = ChunkedRead::new(vec![&b"he"[..], &b"l"[..], &b"lo"[..]]);
    let mut read = BufReader::new(chunked);
    let mut decoder = Decoder::new();
    let result = combine::decode!(
        decoder,
        read,
        combine::parser::range::range(&b"hello"[..]).map(|bytes: &[u8]| bytes.to_owned()),
        |input, _position| combine::easy::Stream::from(input),
    );
    assert_eq!(
        result.as_ref().map(Vec::as_slice).ok(),
        Some(&b"hello"[..]),
        "a token split across buffer refills must parse completely"
    );
    assert!(
        read.get_ref().reads >= 3,
        "the decoder must have pulled all three chunks (reads = {})",
        read.get_ref().reads
    );
    note_phase(
        "buf_reader_handles_chunk_split_mid_token",
        "token split across 3 buffer refills parsed completely",
    );
}

#[test]
fn io_error_from_reader_is_returned() {
    // The reader delivers "he" and then fails: the I/O error must surface
    // instead of being mistaken for EOF or a parse error.
    let chunked = ChunkedRead::failing_after(
        vec![&b"he"[..]],
        io::Error::new(io::ErrorKind::Other, "boom"),
    );
    let mut read = BufReader::new(chunked);
    let mut decoder = Decoder::new();
    let result = combine::decode!(
        decoder,
        read,
        combine::parser::range::range(&b"hello"[..]).map(|bytes: &[u8]| bytes.to_owned()),
        |input, _position| combine::easy::Stream::from(input),
    );
    match result {
        Err(decoder::Error::Io { error, .. }) => {
            assert!(error.to_string().contains("boom"));
        }
        other => panic!("expected an I/O error, got {:?}", other),
    }
    note_phase("io_error_from_reader_is_returned", "I/O error surfaced");
}

type PartialInput<'a> = easy::Stream<MaybePartialStream<&'a [u8]>>;

/// One "poll" of a parser against the currently available buffer, the
/// manual equivalent of an async runtime polling a decoder future.
/// `Ok((None, _))` corresponds to `Poll::Pending`.
fn poll_once<'a, P>(
    parser: &mut P,
    state: &mut P::PartialState,
    buf: &'a [u8],
    eof: bool,
) -> Result<(Option<P::Output>, usize), easy::Errors<u8, &'a [u8], PointerOffset<[u8]>>>
where
    P: Parser<PartialInput<'a>>,
{
    let mut stream = easy::Stream(MaybePartialStream(buf, !eof));
    decode(parser, &mut stream, state)
}

#[test]
fn async_two_consecutive_pending_then_ready() {
    let mut parser = any_partial_state((byte(b'a'), byte(b'b')));
    let mut state = AnyPartialState::default();
    let input = b"ab";

    // Poll 1: only "a" is available. 'a' is consumed and committed, then
    // the parser pends on 'b'.
    let (value, removed) = poll_once(&mut parser, &mut state, &input[..1], false).unwrap();
    assert_eq!(value, None, "parser must pend waiting for 'b'");
    assert_eq!(removed, 1, "the consumed 'a' must be reported as committed");

    // Poll 2: spurious wakeup, no new input. Must pend again without
    // consuming or losing the committed byte.
    let (value, removed) = poll_once(&mut parser, &mut state, &input[1..1], false).unwrap();
    assert_eq!(value, None, "a second Pending must not confuse the parser");
    assert_eq!(removed, 0);

    // Poll 3: 'b' arrives; the parser resumes and completes.
    let (value, removed) = poll_once(&mut parser, &mut state, &input[1..2], false).unwrap();
    assert_eq!(value, Some((b'a', b'b')));
    assert_eq!(removed, 1);
    note_phase(
        "async_two_consecutive_pending_then_ready",
        "two Pending polls then Ready((a, b))",
    );
}

#[test]
fn poll_after_eof_returns_same_error() {
    let mut parser = any_partial_state((byte(b'a'), byte(b'b')));
    let mut state = AnyPartialState::default();
    let input = b"a";

    let (value, _) = poll_once(&mut parser, &mut state, &input[..], false).unwrap();
    assert_eq!(value, None);

    // EOF: the missing 'b' is now a hard error, not Pending.
    let err1 = poll_once(&mut parser, &mut state, &input[1..], true)
        .expect_err("at EOF the missing token must be a real error");
    // Polling again after EOF must deterministically produce the same
    // error (no panic, no Pending, no state corruption).
    let err2 = poll_once(&mut parser, &mut state, &input[1..], true)
        .expect_err("polling after EOF must keep returning the error");
    assert_eq!(format!("{:?}", err1), format!("{:?}", err2));
    note_phase(
        "poll_after_eof_returns_same_error",
        "post-EOF poll returned the identical error",
    );
}

#[test]
fn async_commit_marker_survives_pending_resume() {
    // Branch 1 parses "a"+ then 'c'; branch 2 parses "a"+ only. Once
    // branch 1 has consumed 'a' it is committed: when 'b' arrives instead
    // of 'c' the parse must fail, not fall back to branch 2. Losing the
    // commit marker stored in `many1`'s partial state makes branch 1 look
    // non-committed, which would incorrectly allow the fall-back.
    let mut parser = any_partial_state(choice((
        many1::<Vec<u8>, _, _>(byte(b'a'))
            .and(byte(b'c'))
            .map(|(mut xs, c): (Vec<u8>, u8)| {
                xs.push(c);
                xs
            }),
        many1::<Vec<u8>, _, _>(byte(b'a')),
    )));
    let mut state = AnyPartialState::default();
    let input = b"ab";

    // Poll 1: empty buffer, more input may follow. Nothing can be consumed
    // yet, so the parser pends without committing to a branch.
    let (value, removed) = poll_once(&mut parser, &mut state, &input[..0], false).unwrap();
    assert_eq!(value, None, "parser must pend waiting for input");
    assert_eq!(removed, 0);

    // Poll 2: "ab" arrives. Branch 1 consumes 'a' and commits, so the 'b'
    // where it expects 'c' is a hard error.
    let result = poll_once(&mut parser, &mut state, &input[..], true);
    assert!(
        result.is_err(),
        "consuming 'a' must commit to branch 1; falling back to branch 2 \
         means the commit marker was lost across the resume: {:?}",
        result
    );
    note_phase(
        "async_commit_marker_survives_pending_resume",
        "commit across Pending prevented the fall-back",
    );
}
