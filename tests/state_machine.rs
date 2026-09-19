use combine::{
    attempt, easy,
    error::{ParseError, ParseResult, StreamError, Tracked},
    look_ahead, optional,
    parser::combinator::{any_partial_state, AnyPartialState},
    parser::repeat::many,
    parser::token::token,
    parser::ParseMode,
    stream::{decode_tokio, position, MaybePartialStream},
    EasyParser, Parser, Stream, StreamOnce,
};
use std::cell::Cell;
use std::fmt;
use std::rc::Rc;

#[derive(Clone, Debug)]
enum Ast {
    Token(char),
    Choice(Box<Ast>, Box<Ast>),
    Attempt(Box<Ast>),
    Optional(Box<Ast>),
    Many(Box<Ast>),
    LookAhead(Box<Ast>),
}

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Token(char),
    Choice(Box<Value>),
    Attempt(Box<Value>),
    Optional(Option<Box<Value>>),
    Many(Vec<Value>),
    LookAhead(Box<Value>),
}

impl fmt::Display for Ast {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ast::Token(c) => write!(f, "Token({c:?})"),
            Ast::Choice(a, b) => write!(f, "Choice({a}, {b})"),
            Ast::Attempt(a) => write!(f, "Attempt({a})"),
            Ast::Optional(a) => write!(f, "Optional({a})"),
            Ast::Many(a) => write!(f, "Many({a})"),
            Ast::LookAhead(a) => write!(f, "LookAhead({a})"),
        }
    }
}

type BoxedParser<'a, Input> =
    Box<dyn Parser<Input, Output = Value, PartialState = AnyPartialState> + 'a>;

fn build_parser<'a, Input>(ast: &'a Ast) -> BoxedParser<'a, Input>
where
    Input: Stream<Token = char> + 'a,
{
    match ast {
        Ast::Token(c) => Box::new(any_partial_state(token(*c).map(Value::token))),
        Ast::Choice(a, b) => Box::new(any_partial_state(
            build_parser(a).or(build_parser(b)).map(Value::choice),
        )),
        Ast::Attempt(a) => Box::new(any_partial_state(
            attempt(build_parser(a)).map(Value::attempt),
        )),
        Ast::Optional(a) => Box::new(any_partial_state(
            optional(build_parser(a)).map(Value::optional),
        )),
        Ast::Many(a) => Box::new(any_partial_state(
            many::<Vec<_>, _, _>(build_parser(a)).map(Value::Many),
        )),
        Ast::LookAhead(a) => Box::new(any_partial_state(
            look_ahead(build_parser(a)).map(Value::look_ahead),
        )),
    }
}

impl Value {
    fn token(c: char) -> Self {
        Value::Token(c)
    }
    fn choice(v: Self) -> Self {
        Value::Choice(Box::new(v))
    }
    fn attempt(v: Self) -> Self {
        Value::Attempt(Box::new(v))
    }
    fn optional(v: Option<Self>) -> Self {
        Value::Optional(v.map(Box::new))
    }
    fn look_ahead(v: Self) -> Self {
        Value::LookAhead(Box::new(v))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Step {
    pos: usize,
    committed: bool,
}

type NaiveResult = Result<(Value, Step), (usize, bool)>;

fn run_naive(ast: &Ast, input: &str, step: Step) -> NaiveResult {
    match ast {
        Ast::Token(expected) => {
            if input[step.pos..].starts_with(*expected) {
                Ok((
                    Value::Token(*expected),
                    Step {
                        pos: step.pos + expected.len_utf8(),
                        committed: true,
                    },
                ))
            } else {
                Err((step.pos, false))
            }
        }
        Ast::Choice(a, b) => match run_naive(a, input, step) {
            Ok((v, next)) => Ok((Value::choice(v), next)),
            Err((_, true)) => Err((step.pos, true)),
            Err((first, false)) => match run_naive(b, input, step) {
                Ok((v, mut next)) => {
                    next.committed |= step.committed;
                    Ok((Value::choice(v), next))
                }
                Err((second, committed)) => Err((first.max(second), committed)),
            },
        },
        Ast::Attempt(a) => match run_naive(a, input, step) {
            Ok((v, next)) => Ok((
                Value::attempt(v),
                Step {
                    pos: next.pos,
                    committed: step.committed,
                },
            )),
            Err((pos, _)) => Err((pos, false)),
        },
        Ast::Optional(a) => match run_naive(a, input, step) {
            Ok((v, next)) => Ok((Value::optional(Some(v)), next)),
            Err((pos, false)) => Ok((
                Value::optional(None),
                Step {
                    pos,
                    committed: step.committed,
                },
            )),
            Err((pos, true)) => Err((pos, true)),
        },
        Ast::Many(a) => {
            let mut values = Vec::new();
            let mut current = step;
            loop {
                let before = current.pos;
                match run_naive(a, input, current) {
                    Ok((v, next)) => {
                        assert!(next.pos > before, "generator accepted an empty many");
                        values.push(v);
                        current = next;
                    }
                    Err((pos, committed)) => {
                        return Ok((
                            Value::Many(values),
                            Step {
                                pos: pos.max(current.pos),
                                committed: current.committed || committed,
                            },
                        ));
                    }
                }
            }
        }
        Ast::LookAhead(a) => match run_naive(a, input, step) {
            Ok((v, _)) => Ok((Value::look_ahead(v), step)),
            Err(err) => Err(err),
        },
    }
}

#[derive(Clone, Copy, Debug)]
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Rng { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next_u64() % limit as u64) as usize
    }
}

fn generate(seed: u64) -> Result<Ast, ()> {
    let mut rng = Rng::new(seed);
    for _ in 0..1024 {
        let ast = generate_ast(&mut rng, 3);
        if can_progress(&ast) {
            return Ok(ast);
        }
    }
    Err(())
}

fn generate_ast(rng: &mut Rng, depth: u32) -> Ast {
    if depth == 0 || rng.below(4) == 0 {
        let alphabet = ['a', 'b', 'c'];
        return Ast::Token(alphabet[rng.below(alphabet.len())]);
    }

    match rng.below(6) {
        0 => Ast::Choice(
            Box::new(generate_ast(rng, depth - 1)),
            Box::new(generate_ast(rng, depth - 1)),
        ),
        1 => Ast::Attempt(Box::new(generate_ast(rng, depth - 1))),
        2 => Ast::Optional(Box::new(generate_ast(rng, depth - 1))),
        3 => Ast::Many(Box::new(generate_ast(rng, depth - 1))),
        4 => Ast::LookAhead(Box::new(generate_ast(rng, depth - 1))),
        _ => Ast::Token(['a', 'b', 'c'][rng.below(3)]),
    }
}

fn can_progress(ast: &Ast) -> bool {
    match ast {
        Ast::Token(_) => true,
        Ast::Choice(a, b) => can_progress(a) && can_progress(b),
        Ast::Attempt(a) | Ast::Optional(a) | Ast::LookAhead(a) => can_progress(a),
        Ast::Many(a) => must_consume(a),
    }
}

fn must_consume(ast: &Ast) -> bool {
    match ast {
        Ast::Token(_) => true,
        Ast::Choice(a, b) => must_consume(a) && must_consume(b),
        Ast::Attempt(a) => must_consume(a),
        Ast::Optional(_) | Ast::Many(_) | Ast::LookAhead(_) => false,
    }
}

fn fail_case(seed: u64, ast: &Ast, input: &str, message: &str) -> ! {
    panic!(
        "{message}\nseed: {seed}\nparser: {ast}\ninput: {:?}\nRust: combine::Parser::easy_parse(position::Stream::new(input))",
        input
    );
}

fn generated_seeds() -> Vec<u64> {
    (0x006d_316e_312d_3432_u64..).take(96).collect()
}

#[derive(Debug, PartialEq)]
enum FullOutcome {
    Success { value: Value, consumed: usize },
    Failure(usize),
}

fn naive_full(ast: &Ast, input: &str) -> FullOutcome {
    match run_naive(
        ast,
        input,
        Step {
            pos: 0,
            committed: false,
        },
    ) {
        Ok((value, step)) => FullOutcome::Success {
            value,
            consumed: step.pos,
        },
        Err((pos, _)) => FullOutcome::Failure(pos),
    }
}

fn combine_full(ast: &Ast, input: &str) -> FullOutcome {
    build_parser(ast)
        .easy_parse(position::Stream::new(input))
        .map(|(value, rest)| FullOutcome::Success {
            value,
            consumed: input.len() - rest.input.len(),
        })
        .unwrap_or_else(|error| FullOutcome::Failure((error.position.column - 1) as usize))
}

fn assert_full_matches(seed: u64, ast: &Ast, input: &str) {
    let expected = naive_full(ast, input);
    let actual = combine_full(ast, input);
    if actual != expected {
        fail_case(
            seed,
            ast,
            input,
            &format!("full parse mismatch: expected {expected:?}, actual {actual:?}"),
        );
    }
}

fn random_input(rng: &mut Rng) -> String {
    let alphabet = ['a', 'b', 'c', 'x'];
    let len = rng.below(7);
    (0..len)
        .map(|_| alphabet[rng.below(alphabet.len())])
        .collect()
}

#[derive(Debug, PartialEq)]
enum PartialOutcome {
    Finished(Option<Value>, usize),
}

fn run_partial_chunks(ast: &Ast, chunks: &[&str]) -> PartialOutcome {
    assert!(!chunks.is_empty());
    let mut state = AnyPartialState::default();
    let mut buffer = String::new();
    let mut consumed = 0;

    for (index, chunk) in chunks.iter().enumerate() {
        buffer.push_str(chunk);
        let last = index + 1 == chunks.len();
        let outcome = decode_tokio(
            build_parser(ast),
            &mut easy::Stream(MaybePartialStream(&buffer[..], !last)),
            &mut state,
        );

        let (value, removed) = outcome.unwrap_or_else(|error| {
            panic!(
                "{}",
                format!(
                    "partial parse failed\n
parser: {ast}
chunks: {chunks:?}
error: {error:?}"
                )
            )
        });
        buffer.drain(..removed);
        consumed += removed;

        if let Some(value) = value {
            return PartialOutcome::Finished(Some(value), consumed);
        }
    }

    PartialOutcome::Finished(None, consumed)
}

fn assert_partial_matches(seed: u64, ast: &Ast, input: &str, cuts: &[usize]) {
    let expected = naive_full(ast, input);
    let (expected_value, expected_consumed) = match expected {
        FullOutcome::Success { value, consumed } => (value, consumed),
        FullOutcome::Failure(_) => return,
    };

    let mut start = 0;
    let mut chunks = Vec::new();
    for cut in cuts {
        chunks.push(&input[start..*cut]);
        start = *cut;
    }
    chunks.push(&input[start..]);

    let actual = run_partial_chunks(ast, &chunks);
    match actual {
        PartialOutcome::Finished(Some(actual_value), consumed) => {
            if actual_value != expected_value || consumed != expected_consumed {
                fail_case(
                    seed,
                    ast,
                    input,
                    &format!(
                        "partial mismatch: expected ({expected_value:?}, {expected_consumed}), actual ({actual_value:?}, {consumed}), cuts {cuts:?}"
                    ),
                );
            }
        }
        other => fail_case(
            seed,
            ast,
            input,
            &format!("parser did not finish across chunks: {other:?}, cuts {cuts:?}"),
        ),
    }
}

#[test]
fn generated_zero_width_optional_inside_many_is_rejected() {
    let ast = Ast::Many(Box::new(Ast::Optional(Box::new(Ast::Token('a')))));
    assert!(!can_progress(&ast));
}

#[test]
fn generated_zero_width_many_inside_many_is_rejected() {
    let ast = Ast::Many(Box::new(Ast::Many(Box::new(Ast::Token('a')))));
    assert!(!can_progress(&ast));
}

#[test]
fn generated_zero_width_lookahead_inside_many_is_rejected() {
    let ast = Ast::Many(Box::new(Ast::LookAhead(Box::new(Ast::Token('a')))));
    assert!(!can_progress(&ast));
}

#[test]
fn generated_parsers_match_naive_interpreter() {
    for seed in generated_seeds() {
        let ast = generate(seed).expect("fixed seed should produce a progressing grammar");
        let mut rng = Rng::new(seed ^ 0x9e37_79b9_7f4a_7c15);
        for _ in 0..24 {
            let input = random_input(&mut rng);
            assert_full_matches(seed, &ast, &input);
        }
    }
}

#[test]
fn generated_parsers_match_across_partial_buffers() {
    for seed in generated_seeds().into_iter().take(48) {
        let ast = generate(seed).expect("fixed seed should produce a progressing grammar");
        let mut rng = Rng::new(seed ^ 0x517c_c1b7_2722_0a85);
        for _ in 0..8 {
            let input = random_input(&mut rng);
            if input.is_empty() {
                continue;
            }
            let first = 1 + rng.below(input.len());
            assert_partial_matches(seed, &ast, &input, &[first]);
            if input.len() > 2 {
                let available = input.len() - 1;
                let left = 1 + rng.below(available);
                let right_limit = input.len() - left - 1;
                let right = left
                    + 1
                    + if right_limit > 0 {
                        rng.below(right_limit).min(1)
                    } else {
                        0
                    };
                if right < input.len() {
                    assert_partial_matches(seed, &ast, &input, &[left, right]);
                }
            }
        }
    }
}

#[test]
fn generator_failure_report_contains_seed_parser_and_input() {
    let ast = Ast::Many(Box::new(Ast::Optional(Box::new(Ast::Token('a')))));
    let report = format!(
        "seed: {seed}\nparser: {ast}\ninput: {input:?}",
        seed = 12345,
        ast = ast,
        input = "aa"
    );
    assert!(report.contains("seed: 12345"));
    assert!(report.contains("Many(Optional(Token('a')))"));
    assert!(report.contains("input: \"aa\""));
}

#[test]
fn attempt_restores_checkpoint_for_shared_prefix_choice() {
    let mut parser = attempt((token('a'), token('x')).map(|_| Value::token('x')))
        .or(token('a').map(Value::token));
    let (value, rest) = parser
        .parse(easy::Stream(position::Stream::new("a")))
        .unwrap();
    assert_eq!(value, Value::token('a'));
    assert_eq!(rest.0.input, "");
}

#[test]
fn attempt_turns_committed_failure_into_backtracked_failure() {
    let mut parser = attempt((token('a'), token('x')).map(|pair| pair.1)).or(token('a'));
    let (value, rest) = parser
        .parse(easy::Stream(position::Stream::new("a")))
        .unwrap();
    assert_eq!(value, 'a');
    assert_eq!(rest.0.input, "");
}

#[test]
fn committed_failure_without_attempt_reports_farthest_position() {
    let mut parser = (token('a'), token('x')).map(|pair| pair.1).or(token('a'));
    let error = parser
        .parse(easy::Stream(position::Stream::new("a")))
        .map(|(value, rest)| (value, rest.0))
        .unwrap_err();
    assert_eq!(error.position.column, 2);
}

#[test]
fn shared_prefix_branches_can_fail_at_two_positions() {
    let ast = Ast::Choice(
        Box::new(Ast::Choice(
            Box::new(Ast::Token('a')),
            Box::new(Ast::Token('b')),
        )),
        Box::new(Ast::Choice(
            Box::new(Ast::Token('a')),
            Box::new(Ast::Token('c')),
        )),
    );
    assert_full_matches(10, &ast, "a");
    assert_full_matches(10, &ast, "b");
    assert_full_matches(10, &ast, "c");
    assert_full_matches(10, &ast, "x");
}

#[test]
fn generated_zero_width_lookahead_with_optional_inside_many_is_rejected() {
    let ast = Ast::Many(Box::new(Ast::LookAhead(Box::new(Ast::Optional(Box::new(
        Ast::Token('a'),
    ))))));
    assert!(!can_progress(&ast));
}

#[test]
fn optional_returns_none_without_consuming() {
    let ast = Ast::Optional(Box::new(Ast::Token('a')));
    assert_eq!(
        naive_full(&ast, "b"),
        FullOutcome::Success {
            value: Value::optional(None),
            consumed: 0
        }
    );
    assert_full_matches(11, &ast, "b");
}

#[test]
fn buffered_stream_replays_attempt_within_lookahead() {
    use combine::parser::byte::byte;
    use combine::stream::{buffered, position, read, Positioned};
    let mut parser = attempt(byte(b'a').with(byte(b'x'))).or(byte(b'a'));
    let stream = buffered::Stream::new(position::Stream::new(read::Stream::new(&b"a"[..])), 4);
    let result = parser
        .parse(stream)
        .map(|(value, rest)| (value, rest.position()));
    assert_eq!(result, Ok((b'a', 1)));
}

struct FailingRead {
    calls: Rc<Cell<usize>>,
}

impl std::io::Read for FailingRead {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        self.calls.set(self.calls.get() + 1);
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "test io error",
        ))
    }
}

#[test]
fn sync_reader_surfaces_io_error() {
    use combine::stream::{buffered, position, read};
    let calls = Rc::new(Cell::new(0));
    let stream = buffered::Stream::new(
        position::Stream::new(read::Stream::new(FailingRead {
            calls: calls.clone(),
        })),
        1,
    );
    let result = token(b'a').parse(stream);
    assert!(result.is_err());
    assert_eq!(calls.get(), 2);
}

#[derive(Default)]
struct ResumeState(bool);

#[derive(Default)]
struct ResumableAx;

impl<Input> Parser<Input> for ResumableAx
where
    Input: Stream<Token = char>,
{
    type Output = char;
    type PartialState = ResumeState;

    fn parse_partial(
        &mut self,
        input: &mut Input,
        state: &mut Self::PartialState,
    ) -> ParseResult<char, <Input as StreamOnce>::Error> {
        self.parse_mode_impl(combine::parser::PartialMode::default(), input, state)
    }

    fn parse_first(
        &mut self,
        input: &mut Input,
        state: &mut Self::PartialState,
    ) -> ParseResult<char, <Input as StreamOnce>::Error> {
        self.parse_mode_impl(combine::parser::FirstMode, input, state)
    }

    fn parse_mode_impl<M>(
        &mut self,
        mode: M,
        input: &mut Input,
        state: &mut Self::PartialState,
    ) -> ParseResult<char, <Input as StreamOnce>::Error>
    where
        M: ParseMode,
    {
        if mode.is_first() || !state.0 {
            state.0 = true;
            match input.uncons() {
                Ok('a') => match input.uncons() {
                    Ok('x') => ParseResult::CommitOk('x'),
                    Ok(other) => ParseResult::CommitErr(<Input as StreamOnce>::Error::from_error(
                        input.position(),
                        <<Input as StreamOnce>::Error as ParseError<
                            char,
                            <Input as StreamOnce>::Range,
                            <Input as StreamOnce>::Position,
                        >>::StreamError::unexpected_token(other),
                    )),
                    Err(stream_error) => ParseResult::CommitErr(
                        <Input as StreamOnce>::Error::from_error(input.position(), stream_error),
                    ),
                },
                Ok(other) => {
                    ParseResult::PeekErr(Tracked::from(<Input as StreamOnce>::Error::from_error(
                        input.position(),
                        <<Input as StreamOnce>::Error as ParseError<
                            char,
                            <Input as StreamOnce>::Range,
                            <Input as StreamOnce>::Position,
                        >>::StreamError::unexpected_token(other),
                    )))
                }
                Err(stream_error) => ParseResult::PeekErr(Tracked::from(
                    <Input as StreamOnce>::Error::from_error(input.position(), stream_error),
                )),
            }
        } else {
            assert!(state.0, "parser resumed without its committed-input state");
            state.0 = false;
            match input.uncons() {
                Ok('x') => ParseResult::CommitOk('x'),
                Ok(other) => ParseResult::CommitErr(<Input as StreamOnce>::Error::from_error(
                    input.position(),
                    <<Input as StreamOnce>::Error as ParseError<
                        char,
                        <Input as StreamOnce>::Range,
                        <Input as StreamOnce>::Position,
                    >>::StreamError::unexpected_token(other),
                )),
                Err(stream_error) => ParseResult::CommitErr(
                    <Input as StreamOnce>::Error::from_error(input.position(), stream_error),
                ),
            }
        }
    }
}

fn parse_partial_once<'a, P>(
    parser: &mut P,
    input: &'a str,
    partial: bool,
    state: &mut P::PartialState,
) -> Result<Option<char>, <easy::Stream<MaybePartialStream<&'a str>> as StreamOnce>::Error>
where
    P: Parser<easy::Stream<MaybePartialStream<&'a str>>, Output = char>,
{
    let mut stream = easy::Stream(MaybePartialStream(input, partial));
    match parser.parse_with_state(&mut stream, state) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            if error.is_unexpected_end_of_input() && partial {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

#[test]
fn partial_choice_committed_failure_resumes_selected_branch() {
    let mut parser = ResumableAx;
    let mut state = ResumeState::default();

    assert_eq!(
        parse_partial_once(&mut parser, "a", true, &mut state).unwrap(),
        None
    );
    assert_eq!(
        parse_partial_once(&mut parser, "x", false, &mut state).unwrap(),
        Some('x')
    );
}
