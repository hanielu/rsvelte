//! The completion and signature help requests tsrs never answers.
//!
//! tsrs 0.18.0 (and typescript-go 7.1.0-dev.20260930.4) loops for good on
//! some completion and signature help requests, one core busy and a
//! cancellation ignored (`docs/upstream/tsrs-lost-request` in svelte-rs). The
//! server restarts tsgo for an overdue request only when this module says
//! tsrs loops on it: a request that is merely slow (its document's check
//! takes longer than the timeout) must not cost a restart, which rebuilds
//! every project and throws the check away. While one such request about a
//! document is with tsgo, it answers the editor's others there that this
//! module says tsrs loops on without sending them.
//!
//! What loops, as measured against tsrs (`tests/data/tsrs-lost-requests.json`:
//! 470 requests, every loop among them found, and no request that is answered
//! counted as one except a signature help next to a call's parentheses and a
//! type assertion a `.tsx` file reads as an element):
//! both requests walk back from the position over the tokens a type argument
//! list can hold, to decide whether a `<` before it opens one (Strada's
//! `getPossibleTypeArgumentsInfo`), and completion repeats the walk from the
//! identifier before that `<` (`isPossiblyTypeArgumentPosition`). The walk
//! jumps from a `}`, `)` or `]` to the bracket that opens it, which the port
//! finds by searching the whole text rather than the text before it (Strada's
//! `findPrecedingMatchingToken` fast path, `lastIndexOf(…, tokenFullStart)`).
//! When the last `{` of the text comes after its last `}` (likewise `(` and
//! `)`, `[` and `]`), every closer jumps forward to that opener, and a walk
//! that reaches a closer again from there never ends. The walk only starts if
//! the text has a `<` anywhere (the same search, for the check that opens it).
//!
//! A projected component's shadow ends with generated code whose `}` and `)`
//! come after everything the author wrote, so only its brackets can meet the
//! condition; the script body a rejected document is answered from, and a
//! plain TypeScript buffer, can meet all three.

use std::collections::{HashMap, HashSet};

/// Whether tsrs can loop on any completion or signature help request in
/// `text`: the walk starts only where there is a `<`, and loops only through
/// an opener that comes after the last closer of its kind. It is cheap (a
/// few scans of the text), for every request; [`request_loops`] reads the
/// tokens.
#[must_use]
pub fn text_can_loop(text: &str) -> bool {
    text.contains('<')
        && PAIRS
            .iter()
            .any(|&(close, open)| opener_after_closers(text, close, open).is_some())
}

/// Whether tsrs loops for good on `method` at the UTF-8 `offset` of `text`.
#[must_use]
pub fn request_loops(method: &str, text: &str, offset: usize) -> bool {
    let completion = match method {
        "textDocument/completion" => true,
        "textDocument/signatureHelp" => false,
        _ => return false,
    };
    if !text_can_loop(text) {
        return false;
    }
    let tokens = Tokens::new(text);
    if tokens.in_comment(offset) {
        return false;
    }
    let Some(previous) = tokens.preceding(offset) else {
        return false;
    };
    // In a string the requests answer something else, without the walk.
    if tokens.list[previous].kind == Kind::Literal && offset < tokens.list[previous].end {
        return false;
    }
    let walker = Walker::new(text, &tokens);
    if !completion {
        // `findTokenOnLeftOfPosition`, then the walk from it.
        return walker.walk(previous) == WalkEnd::Loops;
    }
    // A position at the end of a word completes that word: the walk starts at
    // the token before it (`getCompletionData`'s `contextToken`).
    let context = if tokens.list[previous].kind == Kind::Word && offset <= tokens.list[previous].end
    {
        match previous.checked_sub(1) {
            Some(context) => context,
            None => return false,
        }
    } else {
        previous
    };
    // `isPossiblyTypeArgumentPosition` walks again from the identifier the
    // `<` follows, as long as it names no generic signature. That is taken as
    // never: where one does, a request tsrs answers is counted as a loop, which
    // costs only the editor's repeats of it while it is with tsgo.
    let mut called = HashSet::new();
    let mut end = walker.walk(context);
    loop {
        match end {
            WalkEnd::Loops => return true,
            WalkEnd::Nothing => return false,
            WalkEnd::Called(word) => {
                if !called.insert(word) {
                    return true;
                }
                end = walker.walk(word);
            }
        }
    }
}

const PAIRS: [(u8, u8); 3] = [(b'}', b'{'), (b')', b'('), (b']', b'[')];

/// How many tokens a walk may visit before it counts as endless. A walk that
/// ends visits each token at most once per lap through a forward jump, and
/// each lap lowers its count of open `>`s, so this bounds no real one.
const WALK_LIMIT: usize = 1_000_000;

/// The last `open` of `text`, when no `close` comes after it.
fn opener_after_closers(text: &str, close: u8, open: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let opener = bytes.iter().rposition(|byte| *byte == open)?;
    bytes[opener..]
        .iter()
        .all(|byte| *byte != close)
        .then_some(opener)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// An identifier or a keyword a type can hold.
    Word,
    /// A keyword that only starts or joins statements and expressions.
    Reserved,
    /// A string or a number.
    Literal,
    LessThan,
    GreaterThan,
    Open(u8),
    Close(u8),
    Comma,
    QuestionDot,
    /// `=>`, `.`, `|`, `?` or `:`: tokens a type argument list holds.
    TypePunctuation,
    Other,
}

struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

struct Tokens {
    list: Vec<Token>,
    comments: Vec<std::ops::Range<usize>>,
}

/// Words that are never an identifier or a type: the walk stops at them.
const RESERVED: &[&str] = &[
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "var",
    "while",
    "with",
    "yield",
];

const OPERATORS: &[&str] = &[
    "...", "??=", "?.", "??", "=>", "===", "!==", "==", "!=", "<<=", "<<", "<=", "||=", "&&=",
    "||", "&&", "|=", "&=", "^=", "**=", "**", "++", "--", "+=", "-=", "*=", "/=", "%=",
];

impl Tokens {
    /// TypeScript's tokens, near enough for the walk: a `>` is always one
    /// token (the scanner's, before the parser rescans `>>` or `>=`), and a
    /// private name (`#a`) is a `#` and a word. A template's text is a token
    /// up to each `${`, and the `}` that ends a substitution (the one that
    /// closes no `{` opened in it) continues it, as the scanner reads it again
    /// for the parser; a `;` in a substitution ends the template instead (the
    /// parser gives up on its expression there, and a later `}` is a plain
    /// one). A `/` where an expression starts begins a regular expression
    /// ([`slash_starts_regex`], [`regex_end`]), a token the walk stops at.
    fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut list = Vec::new();
        let mut comments = Vec::new();
        // For each `{` and `${` not closed yet, whether it opened a
        // template's substitution.
        let mut braces = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let Some(character) = text[at..].chars().next() else {
                break;
            };
            let start = at;
            if character.is_whitespace() {
                at += character.len_utf8();
                continue;
            }
            if text[at..].starts_with("//") {
                at = text[at..].find('\n').map_or(bytes.len(), |line| at + line);
                comments.push(start..at);
                continue;
            }
            if bytes[at] == b';' && braces.last() == Some(&true) {
                braces.pop();
            }
            if text[at..].starts_with("/*") {
                at = text[at + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |close| at + 2 + close + 2);
                comments.push(start..at);
                continue;
            }
            // Read as division, a regular expression's quote would open a
            // string, and its `//` or `/*` a comment, that hides the walk's
            // tokens (review 6: `/'/` on the line of a loop the model missed).
            if character == '/' && slash_starts_regex(text, list.last()) {
                at = regex_end(text, at);
                list.push(Token {
                    kind: Kind::Other,
                    start,
                    end: at,
                });
                continue;
            }
            let kind = match character {
                '\'' | '"' => {
                    let quote = bytes[at];
                    at += 1;
                    while at < bytes.len() && bytes[at] != quote {
                        if bytes[at] == b'\\' {
                            at += 1;
                        } else if bytes[at] == b'\n' {
                            break;
                        }
                        at += 1;
                    }
                    at = (at + 1).min(bytes.len());
                    Kind::Literal
                }
                // A template's text is not among the tokens the walk passes.
                '`' => {
                    let substitution;
                    (at, substitution) = template_text(bytes, at + 1);
                    if substitution {
                        braces.push(true);
                    }
                    Kind::Other
                }
                '}' if braces.last() == Some(&true) => {
                    braces.pop();
                    let substitution;
                    (at, substitution) = template_text(bytes, at + 1);
                    if substitution {
                        braces.push(true);
                    }
                    Kind::Other
                }
                '0'..='9' => {
                    at = skip_word(text, at);
                    Kind::Literal
                }
                '.' if bytes.get(at + 1).is_some_and(u8::is_ascii_digit) => {
                    at = skip_word(text, at + 1);
                    Kind::Literal
                }
                '?' if text[at..].starts_with("?.")
                    && !bytes.get(at + 2).is_some_and(u8::is_ascii_digit) =>
                {
                    at += 2;
                    Kind::QuestionDot
                }
                _ if is_word_character(character) => {
                    at = skip_word(text, at + character.len_utf8());
                    if RESERVED.contains(&&text[start..at]) {
                        Kind::Reserved
                    } else {
                        Kind::Word
                    }
                }
                _ => {
                    if let Some(operator) = OPERATORS
                        .iter()
                        .find(|operator| text[at..].starts_with(**operator))
                    {
                        at += operator.len();
                        if *operator == "=>" {
                            Kind::TypePunctuation
                        } else {
                            Kind::Other
                        }
                    } else {
                        at += character.len_utf8();
                        match character {
                            '{' => braces.push(false),
                            '}' => {
                                braces.pop();
                            }
                            _ => {}
                        }
                        match character {
                            '<' => Kind::LessThan,
                            '>' => Kind::GreaterThan,
                            '{' | '(' | '[' => Kind::Open(bytes[start]),
                            '}' | ')' | ']' => Kind::Close(bytes[start]),
                            ',' => Kind::Comma,
                            '.' | '|' | '?' | ':' => Kind::TypePunctuation,
                            _ => Kind::Other,
                        }
                    }
                }
            };
            list.push(Token {
                kind,
                start,
                end: at,
            });
        }
        Self { list, comments }
    }

    /// `findPrecedingToken`: the last token that starts before `offset` (the
    /// one `offset` is inside, or the one before the trivia it is in).
    fn preceding(&self, offset: usize) -> Option<usize> {
        self.list
            .partition_point(|token| token.start < offset)
            .checked_sub(1)
    }

    fn in_comment(&self, offset: usize) -> bool {
        self.comments
            .iter()
            .any(|comment| comment.start < offset && offset < comment.end)
    }
}

/// The end of a template's text that starts at `from` (after its `` ` `` or
/// the `}` of a substitution), and whether it ends with `${`.
fn template_text(bytes: &[u8], mut from: usize) -> (usize, bool) {
    while from < bytes.len() {
        match bytes[from] {
            b'\\' => from += 2,
            b'`' => return (from + 1, false),
            b'$' if bytes.get(from + 1) == Some(&b'{') => return (from + 2, true),
            _ => from += 1,
        }
    }
    (bytes.len(), false)
}

/// Whether a `/` after `previous` starts a regular expression, which the
/// parser decides (it rescans the `/` where it reads an expression): after a
/// value (a word, a literal, `)`, `]`, a whole template, `++` or `--`) it
/// divides, and after a `<` it is no expression's start either (a type
/// assertion's type or a closing tag follows: `lsfuzz2.mjs` seed 12's
/// `</scripi<…`); at the start, after an operator, an opener, a `,`, a
/// keyword that is no value or a `}` (a block's end: a statement starts
/// there) it begins one.
fn slash_starts_regex(text: &str, previous: Option<&Token>) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    let source = &text[previous.start..previous.end];
    match previous.kind {
        Kind::Word | Kind::Literal | Kind::LessThan | Kind::Close(b')' | b']') => false,
        Kind::Reserved => !matches!(source, "this" | "super" | "null"),
        Kind::Other => !(source.ends_with('`') || source == "++" || source == "--"),
        _ => true,
    }
}

/// The end of the regular expression whose `/` is at `slash`, as
/// TypeScript's scanner reads it (`reScanSlashToken`): to the `/` that is
/// neither escaped nor in a class, then its flags. One that a line ends
/// first ends at the first bracket it does not open (outside a class, a
/// `{…}` quantifier and its own groups), before the spaces and `;`s ahead
/// of that bracket, so a half-typed `/'; }` leaves the `}` to the script.
fn regex_end(text: &str, slash: usize) -> usize {
    let bytes = text.as_bytes();
    let body = slash + 1;
    let mut at = body;
    let mut escaped = false;
    let mut class = false;
    while at < bytes.len() && !starts_line_break(bytes, at) {
        match bytes[at] {
            _ if escaped => escaped = false,
            b'/' if !class => return skip_word(text, at + 1),
            b'[' => class = true,
            b'\\' => escaped = true,
            b']' => class = false,
            _ => {}
        }
        at += 1;
    }
    let line_end = at;
    at = body;
    escaped = false;
    let mut class_depth = 0_usize;
    let mut quantifier = false;
    let mut group_depth = 0_usize;
    while at < line_end {
        match bytes[at] {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'[' => class_depth += 1,
            b']' if class_depth > 0 => class_depth -= 1,
            _ if class_depth > 0 => {}
            b'{' => quantifier = true,
            b'}' if quantifier => quantifier = false,
            _ if quantifier => {}
            b'(' => group_depth += 1,
            b')' if group_depth > 0 => group_depth -= 1,
            b')' | b']' | b'}' => break,
            _ => {}
        }
        at += 1;
    }
    while at > body && (bytes[at - 1] == b';' || bytes[at - 1].is_ascii_whitespace()) {
        at -= 1;
    }
    at
}

/// Whether a line break (one TypeScript's scanner ends a line at: `\n`,
/// `\r`, U+2028 or U+2029) starts at the UTF-8 offset `at`.
fn starts_line_break(bytes: &[u8], at: usize) -> bool {
    matches!(bytes[at], b'\n' | b'\r')
        || bytes[at..].starts_with(&[0xE2, 0x80, 0xA8])
        || bytes[at..].starts_with(&[0xE2, 0x80, 0xA9])
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(character, '_' | '$')
        || !character.is_ascii() && !character.is_whitespace()
}

fn skip_word(text: &str, mut at: usize) -> usize {
    while let Some(character) = text[at..].chars().next() {
        if !is_word_character(character) {
            break;
        }
        at += character.len_utf8();
    }
    at
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WalkEnd {
    /// The walk found the `<` of a type argument list after this word.
    Called(usize),
    /// The walk stopped: nothing there takes type arguments.
    Nothing,
    /// The walk never ends.
    Loops,
}

struct Walker<'a> {
    tokens: &'a Tokens,
    /// For each closer, the opener every one of its kind jumps to, when the
    /// port's text search finds one after the last closer.
    forward: HashMap<u8, usize>,
}

impl<'a> Walker<'a> {
    fn new(text: &str, tokens: &'a Tokens) -> Self {
        let mut forward = HashMap::new();
        for (close, open) in PAIRS {
            // The text search finds an opener in a comment or a string too;
            // the token before it is taken only if it is that opener.
            if let Some(opener) = opener_after_closers(text, close, open)
                && let Some(token) = tokens.preceding(opener + 1)
                && tokens.list[token].kind == Kind::Open(open)
            {
                forward.insert(close, token);
            }
        }
        Self { tokens, forward }
    }

    /// `findPrecedingMatchingToken` for the closer at `index`.
    fn opener_of(&self, index: usize, close: u8) -> Option<usize> {
        if let Some(&opener) = self.forward.get(&close) {
            return Some(opener);
        }
        let open = PAIRS.iter().find(|pair| pair.0 == close)?.1;
        let mut depth = 0_usize;
        for candidate in (0..index).rev() {
            match self.tokens.list[candidate].kind {
                Kind::Open(byte) if byte == open => {
                    if depth == 0 {
                        return Some(candidate);
                    }
                    depth -= 1;
                }
                Kind::Close(byte) if byte == close => depth += 1,
                _ => {}
            }
        }
        None
    }

    /// `getPossibleTypeArgumentsInfo` from the token at `start`.
    fn walk(&self, start: usize) -> WalkEnd {
        let list = &self.tokens.list;
        let mut index = start;
        let mut open_greater_thans = 0_usize;
        // The count of open `>`s each forward jump target was reached with:
        // reached again with as many or more, the walk repeats for good.
        let mut reached = HashMap::new();
        for _ in 0..WALK_LIMIT {
            match list[index].kind {
                Kind::LessThan => {
                    let Some(mut word) = index.checked_sub(1) else {
                        return WalkEnd::Nothing;
                    };
                    if list[word].kind == Kind::QuestionDot {
                        let Some(before) = word.checked_sub(1) else {
                            return WalkEnd::Nothing;
                        };
                        word = before;
                    }
                    if list[word].kind != Kind::Word {
                        return WalkEnd::Nothing;
                    }
                    if open_greater_thans == 0 {
                        return WalkEnd::Called(word);
                    }
                    open_greater_thans -= 1;
                    index = word;
                }
                Kind::GreaterThan => open_greater_thans += 1,
                Kind::Close(close) => {
                    let Some(opener) = self.opener_of(index, close) else {
                        return WalkEnd::Nothing;
                    };
                    if opener > index
                        && reached
                            .insert(opener, open_greater_thans)
                            .is_some_and(|before| open_greater_thans >= before)
                    {
                        return WalkEnd::Loops;
                    }
                    index = opener;
                }
                Kind::Comma | Kind::Word | Kind::Literal | Kind::TypePunctuation => {}
                Kind::Reserved | Kind::Open(_) | Kind::QuestionDot | Kind::Other => {
                    return WalkEnd::Nothing;
                }
            }
            let Some(before) = index.checked_sub(1) else {
                return WalkEnd::Nothing;
            };
            index = before;
        }
        WalkEnd::Loops
    }
}

#[cfg(test)]
mod tests {
    use super::{request_loops, text_can_loop};

    /// Each request tsrs 0.18.0 was sent in a one-file project, and whether it
    /// answered within 2.5 s (`loops`: it never did, and kept a core busy).
    /// Collected with `docs/upstream/tsrs-lost-request/loopprobe.mjs` in
    /// svelte-rs; the two long texts are the lost requests `lsfuzz.mjs` seeds
    /// 22 and 38 found, as tsgo had them.
    const MEASURED: &str = include_str!("../tests/data/tsrs-lost-requests.json");

    #[derive(serde::Deserialize)]
    struct Measured {
        text: String,
        offset: usize,
        method: String,
        loops: bool,
    }

    #[test]
    fn every_measured_loop_is_found_and_no_answered_request_but_known_ones() {
        let measured: Vec<Measured> = serde_json::from_str(MEASURED).unwrap();
        assert!(measured.len() > 300);
        let mut missed = Vec::new();
        let mut wrong = Vec::new();
        for case in &measured {
            let method = format!("textDocument/{}", case.method);
            let found = request_loops(&method, &case.text, case.offset);
            let at = format!(
                "{} {:?}|{:?}",
                case.method,
                &case.text[..case.offset],
                &case.text[case.offset..]
            );
            if case.loops && !found {
                missed.push(at);
            } else if !case.loops && found {
                wrong.push(at);
            }
        }
        assert!(missed.is_empty(), "loops not found: {missed:#?}");
        // A signature help next to a call's parentheses is answered for the
        // call without the walk; the model, which reads no syntax tree, walks.
        // In a `.tsx` file `<any>b` opens an element, whose text the model
        // reads as script.
        assert!(
            wrong
                .iter()
                .all(|at| at.starts_with("signatureHelp") || at.contains("<any>b")),
            "answered requests counted as loops: {wrong:#?}"
        );
        assert!(wrong.len() <= 7, "{wrong:#?}");
    }

    #[test]
    fn only_completion_and_signature_help_can_loop() {
        let text = "q<w;\n}c{";
        assert!(request_loops("textDocument/completion", text, 6));
        assert!(request_loops("textDocument/signatureHelp", text, 6));
        assert!(!request_loops("textDocument/hover", text, 6));
        assert!(!request_loops("textDocument/definition", text, 6));
    }

    #[test]
    fn a_projected_shadow_can_loop_only_through_its_brackets() {
        let tail = "\n}\nconst X = __sveltets_2_isomorphic_component($$render());\n/*Ωignore_startΩ*/type X = InstanceType<typeof X>;\n/*Ωignore_endΩ*/export default X;\n";
        let shadow = |script: &str| format!("function $$render() {{\n{script}{tail}");
        assert!(!text_can_loop(&shadow("\tlet a = 1;}i<{\n")));
        let unclosed = shadow("\tlet q = [1]a[\n");
        assert!(text_can_loop(&unclosed));
        let at = unclosed.find("]a[").unwrap() + 2;
        assert!(request_loops("textDocument/completion", &unclosed, at));
        assert!(!request_loops(
            "textDocument/completion",
            &shadow("\tlet q = [1]a[];\n"),
            at
        ));
    }

    /// A template's text is no token the walk passes, and the parser reads a
    /// `}` that ends a substitution as more of it; but a substitution its
    /// expression cannot reach the end of (a `;` in it) ends the template
    /// there. `lsfuzz2.mjs` seed 12 lost a completion in a script body whose
    /// first line opened such a substitution (the text is in the measured
    /// requests), which the model, reading the rest as one template, missed.
    #[test]
    fn a_template_is_read_as_typescript_reads_it() {
        let completion = |text: &str| {
            request_loops(
                "textDocument/completion",
                text,
                text.rfind('c').unwrap() + 1,
            )
        };
        assert!(completion("`${q<w;\n}c{`"));
        assert!(completion("x = `a${b}c${d}`;q<w;\n}c{"));
        assert!(!completion("q<w;\n`${}c{"));
        assert!(!completion("q<w;\n}c{`${a}`"));
    }

    /// A regular expression is one token, wherever a `/` starts an
    /// expression (review 6: read as a division, the quote in `/'/` opened a
    /// string to the end of the line and the `/*` in `/\/*/` a comment, which
    /// hid the loop's brackets, and tsrs looped on a request the model
    /// cleared); after a value a `/` still divides, and a regular expression
    /// a line ends leaves the bracket it does not open to the script, as
    /// TypeScript's scanner does. Each of these is in the measured requests.
    #[test]
    fn a_regular_expression_is_read_as_typescript_reads_it() {
        let completion = |text: &str| {
            request_loops(
                "textDocument/completion",
                text,
                text.rfind("}c").unwrap() + 2,
            )
        };
        let line = "let a = x < y;\n";
        for regex in [
            "/'/",
            "/\"/",
            "'a'.replace(/['\"]/g, '')",
            "/\\/*/",
            "/https?:\\/\\//",
            "/[/']/gi",
            "!/'/.test(a)",
            "() => /'/",
            "[/'/]",
            "a ? /'/ : /\"/",
            "/'",
        ] {
            assert!(
                completion(&format!("{line}const r = {regex}; }}c{{")),
                "{regex}"
            );
        }
        assert!(completion(&format!("{line}const r = /\\/*/;\n}}c{{")));
        assert!(completion(&format!(
            "{line}if (a) {{}}\n/'/.test(a); }}c{{"
        )));
        for division in ["n / 2", "(n) / 2", "m[0] / 2", "`${n}` / 2", "this.v / 2"] {
            assert!(
                completion(&format!(
                    "{line}const d = {division}; const s = \"/\"; }}c{{"
                )),
                "{division}"
            );
        }
        // Brackets a regular expression holds are no tokens.
        let text = "let a = 1 < 2;\nconst r = /[}]/; c{";
        assert!(!request_loops(
            "textDocument/completion",
            text,
            text.len() - 1
        ));
    }

    #[test]
    fn a_position_in_a_comment_or_a_string_does_not_walk() {
        let text = "q<w;\n}c/* x */{";
        assert!(!request_loops(
            "textDocument/completion",
            text,
            text.find('x').unwrap()
        ));
        let text = "q<w;\n}c'x'{";
        assert!(!request_loops(
            "textDocument/completion",
            text,
            text.find('x').unwrap()
        ));
    }
}
