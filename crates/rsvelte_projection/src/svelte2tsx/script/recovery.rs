//! Recover a usable program from a script oxc cannot parse.
//!
//! Official svelte2tsx parses with TypeScript, whose parser is error-tolerant
//! and yields a usable AST for a script that does not parse; oxc discards the
//! AST on a fatal error, so a broken script would otherwise lose every script
//! transform — its imports stay inside `$$render()`, where TypeScript resolves
//! none of them, and the props, exports and store subscriptions disappear.
//! That is the state of almost every keystroke: `runtime.` before `</script>`,
//! an unclosed call, an import or initializer in progress.
//!
//! Recovery edits a copy of the script until it parses, and only ever in
//! place, so every span still addresses the original text and MagicString
//! keeps emitting the author's own bytes, broken ones included, for TypeScript
//! to read. A missing statement terminator becomes a line break; a missing
//! name, operand or closing bracket is written into the whitespace where
//! TypeScript would put its zero-width missing node, so the construct keeps
//! its extent; anything else unfinished is blanked to spaces (line breaks
//! kept), which only hides it from the transforms.

use std::ops::Range;

use crate::ast::oxc_program::RetainedProgram;

/// Each repair rewrites at least one byte and fills a byte at most once, so
/// the loop always ends; the cap bounds the cost of a script that keeps
/// failing somewhere new.
const MAX_REPAIRS: usize = 128;

/// Repairs that have not converged by now are chasing a construct that
/// swallowed the rest of the script (a template literal left open, say): the
/// code from the first failing line on is then blanked, as TypeScript's own
/// recovery would have absorbed it, the brackets left open before it are
/// closed, and the repairs start again. If that does not converge either, the
/// cut moves up to the line that opened the outermost of those brackets.
const TAIL_FALLBACK_AFTER: usize = 32;

/// Written where a name or an operand is missing.
const PLACEHOLDER: u8 = b'_';

/// Re-parse `source` with in-place repairs until oxc accepts it.
pub(super) fn recover(
    source: &str,
    failed: &RetainedProgram<'_>,
) -> Option<RetainedProgram<'static>> {
    let mut fault = Fault::of(failed)?;
    let first_line = line_start(source.as_bytes(), fault.anchor);
    let outer_line = {
        let bytes = source.as_bytes();
        open_brackets(bytes, &tokens(&bytes[..first_line]))
            .first()
            .map_or(first_line, |&(start, _)| line_start(bytes, start))
    };
    let mut cuts =
        std::iter::once(first_line).chain((outer_line < first_line).then_some(outer_line));
    let mut text = source.to_string();
    let mut state = State::default();
    let mut rounds_before_cut = TAIL_FALLBACK_AFTER;
    for _ in 0..MAX_REPAIRS {
        let planned = rounds_before_cut.checked_sub(1).and_then(|left| {
            rounds_before_cut = left;
            state.plan(&text, &fault)
        });
        let outcome = match planned {
            None => {
                let cut = cuts.next()?;
                state = State::default();
                rounds_before_cut = TAIL_FALLBACK_AFTER;
                attempt(cut_tail(source, cut, &mut state.filled)?)?
            }
            Some(Repair::Rewrite(range, edit)) => attempt(rewrite(&text, range, edit)?)?,
            Some(Repair::Fill {
                at,
                byte,
                otherwise,
            }) => {
                state.filled.push(at);
                match attempt(rewrite(&text, at..at + 1, Edit::Fill(byte))?)? {
                    // Kept only if the parser took the byte and got further.
                    Outcome::Failed(_, next) if next.fatal <= at => {
                        attempt(rewrite(&text, otherwise?, Edit::Blank)?)?
                    }
                    outcome => outcome,
                }
            }
        };
        match outcome {
            Outcome::Parsed(retained) => return Some(retained),
            Outcome::Failed(next_text, next_fault) => {
                text = next_text;
                fault = next_fault;
            }
        }
    }
    None
}

enum Outcome {
    Parsed(RetainedProgram<'static>),
    Failed(String, Fault),
}

fn attempt(text: String) -> Option<Outcome> {
    let retained = RetainedProgram::parse_owned(text, true);
    if !retained.panicked() {
        return Some(Outcome::Parsed(retained));
    }
    let fault = Fault::of(&retained)?;
    Some(Outcome::Failed(retained.source().to_string(), fault))
}

#[derive(Clone, Copy)]
enum Edit {
    /// A space or tab becomes a line break.
    Break,
    /// Every byte becomes a space, line breaks kept.
    Blank,
    /// A whitespace byte becomes this ASCII byte.
    Fill(u8),
    /// The bytes become a string literal: `"`, spaces, `"`.
    Specifier,
}

fn rewrite(text: &str, range: Range<usize>, edit: Edit) -> Option<String> {
    let original = text.get(range.clone())?;
    let replacement: String = match edit {
        Edit::Break => "\n".to_string(),
        Edit::Fill(byte) => char::from(byte).to_string(),
        Edit::Specifier => format!("\"{}\"", " ".repeat(original.len().checked_sub(2)?)),
        Edit::Blank => original
            .bytes()
            .map(|byte| match byte {
                b'\n' | b'\r' => char::from(byte),
                _ => ' ',
            })
            .collect(),
    };
    debug_assert_eq!(replacement.len(), original.len());
    let mut rewritten = String::with_capacity(text.len());
    rewritten.push_str(&text[..range.start]);
    rewritten.push_str(&replacement);
    rewritten.push_str(&text[range.end..]);
    Some(rewritten)
}

/// What a fatal parse reported, in the coordinates of the text it parsed.
struct Fault {
    /// Where parsing stopped: the furthest diagnostic.
    fatal: usize,
    /// The earliest diagnostic on the fatal one's line: a recoverable error
    /// just before the fatal one (`import` used as a name) usually marks the
    /// token the unfinished construct swallowed.
    anchor: usize,
    /// `Expected a semicolon or an implicit semicolon`.
    missing_asi: Option<usize>,
    /// A string, template or regular expression the lexer could not close.
    unterminated: Option<Range<usize>>,
    /// The bracket an `Expected ")"`-style error says is still open.
    opened: Option<usize>,
    /// The closing bracket the fatal error expected.
    closer: Option<u8>,
}

impl Fault {
    fn of(retained: &RetainedProgram<'_>) -> Option<Self> {
        let bytes = retained.source().as_bytes();
        let mut fatal: Option<(usize, Option<usize>, Option<u8>)> = None;
        let mut missing_asi = None;
        let mut unterminated = None;
        let mut offsets = Vec::new();
        for diagnostic in retained.diagnostics() {
            let mut labels = diagnostic.labels.iter();
            let Some(primary) = labels.next() else {
                continue;
            };
            let offset = primary.offset() as usize;
            offsets.push(offset);
            let message: &str = &diagnostic.message;
            if missing_asi.is_none()
                && message.starts_with("Expected a semicolon or an implicit semicolon")
            {
                missing_asi = Some(offset);
            }
            if unterminated.is_none() && message.starts_with("Unterminated") {
                unterminated = Some(offset..offset + primary.len() as usize);
            }
            if fatal.is_none_or(|(fatal, _, _)| offset >= fatal) {
                let opened = labels
                    .find(|label| label.label() == Some("Opened here"))
                    .map(|label| label.offset() as usize);
                fatal = Some((offset, opened, expected_closer(message)));
            }
        }
        let (fatal, opened, closer) = fatal?;
        let fatal_line = line_start(bytes, fatal);
        let anchor = offsets
            .into_iter()
            .filter(|&offset| offset <= fatal && line_start(bytes, offset) == fatal_line)
            .min()
            .unwrap_or(fatal);
        Some(Self {
            fatal,
            anchor,
            missing_asi,
            unterminated,
            opened,
            closer,
        })
    }
}

/// The closing bracket in "Expected `)` but found …" or "Expected `,` or `)`
/// but found …".
fn expected_closer(message: &str) -> Option<u8> {
    let expected = message
        .strip_prefix("Expected ")?
        .split(" but found")
        .next()?;
    match expected.rsplit('`').nth(1)? {
        ")" => Some(b')'),
        "]" => Some(b']'),
        "}" => Some(b'}'),
        _ => None,
    }
}

enum Repair {
    Rewrite(Range<usize>, Edit),
    /// Write `byte` at `at`, or blank `otherwise` if the parser rejects it.
    Fill {
        at: usize,
        byte: u8,
        otherwise: Option<Range<usize>>,
    },
}

#[derive(Default)]
struct State {
    /// The start of a line whose first token oxc rejected: that line is where
    /// the next statement starts, so the construct to repair is the one before
    /// it, for as long as the failure stays on that line.
    line_fault: Option<usize>,
    /// Bytes already filled; a byte is never filled twice.
    filled: Vec<usize>,
}

impl State {
    fn plan(&mut self, text: &str, fault: &Fault) -> Option<Repair> {
        let bytes = text.as_bytes();
        let tokens = tokens(bytes);
        let anchor = fault.anchor.min(bytes.len());

        if let Some(offset) = fault.missing_asi
            && matches!(bytes.get(offset), Some(b' ' | b'\t'))
        {
            // `name.` before a line that starts another statement: TypeScript
            // reads a missing name there (`parseRightSideOfDot`), not
            // `name.let` and a missing semicolon.
            let before = tokens.partition_point(|token| token.range.end <= offset);
            if let [.., dot, word] = &tokens[..before]
                && word.kind == Kind::Word
                && matches!(&bytes[dot.range.clone()], b"." | b"?.")
                && is_first_on_line(bytes, word.range.start)
            {
                return Some(Repair::Rewrite(dot.range.clone(), Edit::Blank));
            }
            return Some(Repair::Rewrite(offset..offset + 1, Edit::Break));
        }
        // oxc reports a `?.` without a name at the `?.` itself.
        let at_anchor = tokens
            .iter()
            .position(|token| token.range.contains(&anchor));
        let anchor_line = line_start(bytes, anchor);
        let boundary = match at_anchor {
            Some(index) if &bytes[tokens[index].range.clone()] == b"?." => tokens[index].range.end,
            _ if self.line_fault == Some(anchor_line) => anchor_line,
            _ if is_first_on_line(bytes, anchor) => {
                self.line_fault = Some(anchor_line);
                anchor_line
            }
            _ => {
                self.line_fault = None;
                anchor
            }
        };
        let before = tokens.partition_point(|token| token.range.start < boundary);
        let culprit = before.checked_sub(1).map(|index| &tokens[index]);
        let blank_culprit = culprit.map(|token| token.range.clone());

        // An import still being typed stays an import, hoisted like official's
        // error-tolerant one, once what follows `import` is spelled as a
        // module specifier.
        if let Some(range) = unfinished_import(
            bytes,
            &tokens,
            culprit.map_or(anchor, |token| token.range.start),
        ) {
            return Some(Repair::Rewrite(range, Edit::Specifier));
        }
        if let Some(range) = &fault.unterminated
            && bytes
                .get(range.clone())
                .is_some_and(|slice| slice.iter().any(|byte| !byte.is_ascii_whitespace()))
        {
            return Some(Repair::Rewrite(range.clone(), Edit::Blank));
        }

        // The script ends inside a bracket opened by the unfinished construct:
        // close it in the trailing space. One opened on an earlier line
        // swallowed complete code instead, and is blanked below.
        let culprit_line = culprit.map(|token| line_start(bytes, token.range.start));
        if let Some(closer) = fault.closer
            && bytes[anchor..].iter().all(u8::is_ascii_whitespace)
            && fault
                .opened
                .is_none_or(|opened| Some(line_start(bytes, opened)) == culprit_line)
            && let Some(at) = self.fillable(bytes, culprit.map_or(0, |token| token.range.end))
        {
            return Some(Repair::Fill {
                at,
                byte: closer,
                otherwise: blank_culprit,
            });
        }
        // A bracket that swallowed what follows it is the construct to drop.
        // oxc names it for calls and blocks; an object or array literal left
        // open before a line that starts a declaration is found by hand.
        if let Some(opened) = fault
            .opened
            .or_else(|| swallowing_literal(bytes, &tokens, anchor))
            && opened < anchor
            && matches!(bytes.get(opened), Some(b'(' | b'[' | b'{'))
        {
            return Some(Repair::Rewrite(opened..opened + 1, Edit::Blank));
        }

        let Some(culprit) = culprit else {
            return at_anchor
                .map(|index| Repair::Rewrite(tokens[index].range.clone(), Edit::Blank));
        };
        // Everything before the failing token is complete: that token is the
        // stray one.
        if let Some(index) = at_anchor
            && boundary <= tokens[index].range.start
            && complete_before(bytes, &tokens[..before], culprit)
        {
            return Some(Repair::Rewrite(tokens[index].range.clone(), Edit::Blank));
        }
        if culprit.kind == Kind::Punct
            && takes_operand(&bytes[culprit.range.clone()])
            && let Some(at) = self.fillable(bytes, culprit.range.end)
        {
            return Some(Repair::Fill {
                at,
                byte: PLACEHOLDER,
                otherwise: blank_culprit,
            });
        }
        Some(Repair::Rewrite(culprit.range.clone(), Edit::Blank))
    }

    /// The whitespace byte right at `offset`, unless filling it would join the
    /// filled byte to a following word or it was filled before.
    fn fillable(&self, bytes: &[u8], offset: usize) -> Option<usize> {
        (bytes.get(offset).is_some_and(u8::is_ascii_whitespace)
            && !bytes
                .get(offset + 1)
                .is_some_and(|&byte| is_word_byte(byte))
            && !self.filled.contains(&offset))
        .then_some(offset)
    }
}

/// What follows `import` on the line holding `offset`, when that line starts an
/// import declaration that does not parse on its own and the rest fits in a
/// string literal.
fn unfinished_import(bytes: &[u8], tokens: &[Token], offset: usize) -> Option<Range<usize>> {
    let line = line_start(bytes, offset);
    let line_end = memchr::memchr(b'\n', &bytes[line..]).map_or(bytes.len(), |n| line + n);
    let first = tokens.partition_point(|token| token.range.start < line);
    let on_line: Vec<&Token> = tokens[first..]
        .iter()
        .take_while(|token| token.range.end <= line_end)
        .collect();
    let (keyword, last) = (on_line.first()?, on_line.last()?);
    if &bytes[keyword.range.clone()] != b"import"
        || on_line
            .get(1)
            .is_some_and(|next| matches!(&bytes[next.range.clone()], b"(" | b"." | b"?."))
    {
        return None;
    }
    let declaration = std::str::from_utf8(&bytes[keyword.range.start..last.range.end]).ok()?;
    let rest = keyword.range.end..last.range.end;
    (rest.len() >= 2 && RetainedProgram::parse_owned(declaration.to_string(), true).panicked())
        .then_some(rest)
}

/// The innermost bracket still open where the line holding `anchor` starts,
/// when that line starts a declaration or statement and the bracket opens an
/// expression (an argument list, an array or an object literal) rather than a
/// block, so the declaration cannot belong inside it.
fn swallowing_literal(bytes: &[u8], tokens: &[Token], anchor: usize) -> Option<usize> {
    let line = line_start(bytes, anchor);
    let first = tokens.partition_point(|token| token.range.start < line);
    let keyword = tokens.get(first)?;
    if !matches!(
        &bytes[keyword.range.clone()],
        b"const"
            | b"let"
            | b"var"
            | b"function"
            | b"class"
            | b"import"
            | b"export"
            | b"if"
            | b"for"
            | b"while"
            | b"return"
            | b"interface"
    ) {
        return None;
    }
    let mut open: Vec<usize> = Vec::new();
    for (index, token) in tokens[..first].iter().enumerate() {
        match &bytes[token.range.clone()] {
            b"(" | b"[" | b"{" => open.push(index),
            b")" | b"]" | b"}" => {
                open.pop();
            }
            _ => {}
        }
    }
    let innermost = *open.last()?;
    let bracket = &tokens[innermost];
    if bytes[bracket.range.start] != b'{' {
        return Some(bracket.range.start);
    }
    let before = innermost
        .checked_sub(1)
        .map(|index| &bytes[tokens[index].range.clone()]);
    matches!(
        before,
        Some(b"=" | b":" | b"(" | b"," | b"[" | b"?" | b"|" | b"&" | b"!" | b"return")
    )
    .then_some(bracket.range.start)
}

/// Punctuation that leaves an expression or a member name unfinished.
fn takes_operand(token: &[u8]) -> bool {
    matches!(
        token,
        b"." | b"?."
            | b"=>"
            | b"="
            | b"+"
            | b"-"
            | b"*"
            | b"/"
            | b"%"
            | b"&"
            | b"|"
            | b"^"
            | b"!"
            | b"~"
            | b"<"
            | b">"
            | b"?"
            | b":"
    )
}

/// Whether the tokens before the failing one form complete code once the
/// brackets opened on earlier lines than `culprit`'s are closed: brackets
/// opened on the culprit's own line belong to the unfinished construct.
fn complete_before(bytes: &[u8], before: &[Token], culprit: &Token) -> bool {
    let culprit_line = line_start(bytes, culprit.range.start);
    let open = open_brackets(bytes, before);
    if open.iter().any(|&(start, _)| start >= culprit_line) {
        return false;
    }
    let mut prefix = String::from_utf8_lossy(&bytes[..culprit.range.end]).into_owned();
    for (_, closer) in open.iter().rev() {
        prefix.push_str(closer);
    }
    !RetainedProgram::parse_owned(prefix, true).panicked()
}

/// The brackets and template substitutions `tokens` leave open, innermost
/// last, each with the text that closes it.
fn open_brackets(bytes: &[u8], tokens: &[Token]) -> Vec<(usize, &'static str)> {
    let mut open = Vec::new();
    for token in tokens {
        let text = &bytes[token.range.clone()];
        if token.kind == Kind::Template && text.first() == Some(&b'}') {
            open.pop();
        }
        match text {
            b"(" => open.push((token.range.start, ")")),
            b"[" => open.push((token.range.start, "]")),
            b"{" => open.push((token.range.start, "}")),
            b")" | b"]" | b"}" => {
                open.pop();
            }
            _ if token.kind == Kind::Template && text.ends_with(b"${") => {
                open.push((token.range.start, "}`"));
            }
            _ => {}
        }
    }
    open
}

/// `source` with everything from `cut` on blanked, and the brackets the code
/// before `cut` leaves open closed in the blanked space.
fn cut_tail(source: &str, cut: usize, filled: &mut Vec<usize>) -> Option<String> {
    let blanked = rewrite(source, cut..source.len(), Edit::Blank)?;
    let mut bytes = blanked.into_bytes();
    let open = open_brackets(&bytes, &tokens(&bytes[..cut]));
    let closers = open.iter().rev().flat_map(|(_, closer)| closer.bytes());
    let spaces: Vec<usize> = (cut..bytes.len()).filter(|&at| bytes[at] == b' ').collect();
    for (at, closer) in spaces.into_iter().zip(closers) {
        bytes[at] = closer;
        filled.push(at);
    }
    String::from_utf8(bytes).ok()
}

fn line_start(bytes: &[u8], offset: usize) -> usize {
    let offset = offset.min(bytes.len());
    memchr::memrchr(b'\n', &bytes[..offset]).map_or(0, |newline| newline + 1)
}

fn is_first_on_line(bytes: &[u8], offset: usize) -> bool {
    let offset = offset.min(bytes.len());
    bytes[line_start(bytes, offset)..offset]
        .iter()
        .all(|&byte| matches!(byte, b' ' | b'\t' | b'\r'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Punct,
    String,
    /// A template chunk: `` `…` ``, `` `…${ ``, `}…${` or `` }…` ``.
    Template,
    Regex,
}

#[derive(Debug, Clone)]
struct Token {
    range: Range<usize>,
    kind: Kind,
}

/// The tokens oxc would lex, close enough to choose what to repair: comments
/// are skipped, a string, template chunk or regular expression is one token,
/// and punctuation is one byte except `?.`, `=>` and `...`. An unterminated
/// string or regular expression ends at its line break, as oxc reports it.
fn tokens(bytes: &[u8]) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    // One entry per open `{`: whether it is a template `${` substitution.
    let mut braces: Vec<bool> = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        let kind;
        (index, kind) = match byte {
            _ if byte.is_ascii_whitespace() => {
                index += 1;
                continue;
            }
            b'/' if next == Some(b'/') => {
                index = memchr::memchr(b'\n', &bytes[index..]).map_or(bytes.len(), |n| index + n);
                continue;
            }
            b'/' if next == Some(b'*') => {
                index = memchr::memmem::find(&bytes[index + 2..], b"*/")
                    .map_or(bytes.len(), |n| index + 2 + n + 2);
                continue;
            }
            b'\'' | b'"' => (string_end(bytes, index), Kind::String),
            b'`' => (
                template_chunk_end(bytes, index + 1, &mut braces),
                Kind::Template,
            ),
            b'{' => {
                braces.push(false);
                (index + 1, Kind::Punct)
            }
            b'}' if braces.pop() == Some(true) => (
                template_chunk_end(bytes, index + 1, &mut braces),
                Kind::Template,
            ),
            b'/' if regex_allowed(bytes, tokens.last()) => (regex_end(bytes, index), Kind::Regex),
            _ if is_word_byte(byte) => {
                let mut end = index + 1;
                while end < bytes.len() && is_word_byte(bytes[end]) {
                    end += 1;
                }
                (end, Kind::Word)
            }
            b'?' if next == Some(b'.') && !bytes.get(index + 2).is_some_and(u8::is_ascii_digit) => {
                (index + 2, Kind::Punct)
            }
            b'=' if next == Some(b'>') => (index + 2, Kind::Punct),
            b'.' if next == Some(b'.') && bytes.get(index + 2) == Some(&b'.') => {
                (index + 3, Kind::Punct)
            }
            _ => (index + 1, Kind::Punct),
        };
        tokens.push(Token {
            range: start..index,
            kind,
        });
    }
    tokens
}

/// Identifier, keyword and number bytes; a non-ASCII byte is taken as part of
/// a word, which keeps every token on a character boundary.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#' | b'@') || byte >= 0x80
}

fn string_end(bytes: &[u8], open: usize) -> usize {
    let quote = bytes[open];
    let mut index = open + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'\n' => return index,
            byte if byte == quote => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// The end of a template chunk starting at `index` (just past its opening
/// `` ` `` or `}`): past the closing `` ` ``, or past a `${` whose brace is
/// then recorded as a substitution.
fn template_chunk_end(bytes: &[u8], mut index: usize, braces: &mut Vec<bool>) -> usize {
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'`' => return index + 1,
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                braces.push(true);
                return index + 2;
            }
            _ => index += 1,
        }
    }
    bytes.len()
}

/// A `/` starts a regular expression unless it follows something a division
/// can follow: a value, a closing bracket, or a word that is not an operator
/// keyword.
fn regex_allowed(bytes: &[u8], previous: Option<&Token>) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    let text = &bytes[previous.range.clone()];
    match previous.kind {
        Kind::String | Kind::Regex => false,
        Kind::Template => !text.ends_with(b"${"),
        Kind::Punct => !matches!(text, b")" | b"]" | b"}"),
        Kind::Word => matches!(
            text,
            b"return"
                | b"typeof"
                | b"instanceof"
                | b"in"
                | b"of"
                | b"new"
                | b"delete"
                | b"void"
                | b"throw"
                | b"case"
                | b"do"
                | b"else"
                | b"yield"
                | b"await"
        ),
    }
}

fn regex_end(bytes: &[u8], open: usize) -> usize {
    let mut index = open + 1;
    let mut in_class = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'\n' => return index,
            b'[' => {
                in_class = true;
                index += 1;
            }
            b']' => {
                in_class = false;
                index += 1;
            }
            b'/' if !in_class => {
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                    index += 1;
                }
                return index;
            }
            _ => index += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recovered(source: &str) -> String {
        let failed = RetainedProgram::parse(source, true);
        assert!(failed.panicked(), "the probe must not parse: {source:?}");
        let retained = recover(source, &failed).expect("recovers");
        assert_eq!(retained.source().len(), source.len());
        retained.source().to_string()
    }

    fn token_texts(source: &str) -> Vec<&str> {
        tokens(source.as_bytes())
            .into_iter()
            .map(|token| &source[token.range])
            .collect()
    }

    #[test]
    fn the_tokenizer_keeps_strings_templates_comments_and_regexes_whole() {
        assert_eq!(
            token_texts(
                "a?.b // c\n/* d */ x = /[/]/g.test(`t ${ {e: 1}.e } u`) / 2 ...f => 'g\\'h'"
            ),
            vec![
                "a", "?.", "b", "x", "=", "/[/]/g", ".", "test", "(", "`t ${", "{", "e", ":", "1",
                "}", ".", "e", "} u`", ")", "/", "2", "...", "f", "=>", "'g\\'h'",
            ]
        );
        assert_eq!(token_texts("'open\nnext"), vec!["'open", "next"]);
        assert_eq!(token_texts("é.ü"), vec!["é", ".", "ü"]);
    }

    #[test]
    fn a_dangling_member_access_gets_a_placeholder_name() {
        assert_eq!(recovered("let a = 1;\nruntime.\n"), "let a = 1;\nruntime._");
        assert_eq!(recovered("runtime?.\n"), "runtime?._");
        assert_eq!(
            recovered("function go() {\n\truntime.\n}\n"),
            "function go() {\n\truntime._}\n"
        );
    }

    #[test]
    fn a_dot_with_no_room_for_a_name_is_blanked() {
        assert_eq!(recovered("runtime."), "runtime ");
        assert_eq!(recovered("log(runtime.)"), "log(runtime )");
    }

    #[test]
    fn a_dot_before_a_line_that_starts_a_statement_is_a_missing_name() {
        assert_eq!(
            recovered("runtime.\nlet count = 1;\n"),
            "runtime \nlet count = 1;\n"
        );
    }

    #[test]
    fn a_missing_operand_gets_a_placeholder() {
        assert_eq!(recovered("let x = \n"), "let x =_\n");
        assert_eq!(recovered("const o = { a: };\n"), "const o = { a:_};\n");
    }

    #[test]
    fn a_bracket_left_open_at_the_end_is_closed_in_the_trailing_space() {
        assert_eq!(recovered("foo(\n"), "foo()");
        assert_eq!(recovered("let { a } = $props(\n"), "let { a } = $props()");
    }

    #[test]
    fn an_unfinished_statement_before_a_line_that_starts_a_new_one_is_blanked() {
        assert_eq!(
            recovered("let x =\nconst y = 2;\n"),
            "let x  \nconst y = 2;\n"
        );
        assert_eq!(
            recovered("let o = {\n\ta: 1,\n\tb\nconst y = 2;\n"),
            "let o =  \n\ta\n 1,\n\tb\nconst y = 2;\n"
        );
    }

    #[test]
    fn an_unfinished_import_stays_an_import() {
        assert_eq!(recovered("import { a }\n"), "import\"    \"\n");
        assert_eq!(
            recovered("import { a } from './a\nlet b = 1;\n"),
            "import\"              \"\nlet b = 1;\n"
        );
        assert_eq!(
            recovered("import {\nimport B from 'b';\n"),
            "import\"\"\nimport B from 'b';\n"
        );
        assert_eq!(
            recovered("import { a, b\nimport B from 'b';\n"),
            "import\"     \"\nimport B from 'b';\n"
        );
        assert_eq!(
            recovered("import B from 'b';\nimport { a }\nlet c = 1;\n"),
            "import B from 'b';\nimport\"    \"\nlet c = 1;\n"
        );
    }

    #[test]
    fn a_stray_token_after_complete_code_is_blanked() {
        assert_eq!(
            recovered("let a = 1;\n=\nlet b = 2;\n"),
            "let a = 1;\n \nlet b = 2;\n"
        );
        assert_eq!(
            recovered("let a = 1;\nif () {\n\ta = 2;\n}\n"),
            "let a = 1;\n      {\n\ta = 2;\n}\n"
        );
    }

    #[test]
    fn an_open_bracket_is_blanked_rather_than_what_it_swallowed() {
        assert_eq!(recovered("foo(\nlet b = 1;\n"), "foo \nlet b = 1;\n");
        assert_eq!(
            recovered("function f() {\n\tlet a = 1;\nlet { p } = $props();\n"),
            "function f()  \n\tlet a = 1;\nlet { p } = $props();\n"
        );
    }

    #[test]
    fn a_literal_left_open_before_a_declaration_is_blanked() {
        assert_eq!(
            recovered("const o = {\nconst r = f(\n\t[1],\n);\nlet x = 1;\n"),
            "const o =_ \nconst r = f(\n\t[1],\n);\nlet x = 1;\n"
        );
        assert_eq!(
            recovered("const o = [\nconst r = 1;\n"),
            "const o =_ \nconst r = 1;\n"
        );
        assert_eq!(
            recovered("function f() {\n\tconst x = ;\n}\n"),
            "function f() {\n\tconst x =_;\n}\n"
        );
    }

    #[test]
    fn a_construct_that_swallows_the_rest_keeps_the_code_before_it() {
        let head = "import A from 'a';\nlet { p } = $props();\n";
        let source = format!(
            "{head}`${{a.\n// note\nconst router = create(\n\t[\n\t\t{{\n\t\t\tid: 'root',\n\t\t\tpath: '*',\n\t\t\tComponent: A,\n\t\t\tloader: p.loader,\n\t\t}},\n\t],\n\t{{ basename: p.base, data: {{ root: p.initial ?? null }} }}\n);\n$effect(() => () => router.dispose());\n"
        );
        assert!(recovered(&source).starts_with(head));
    }

    #[test]
    fn an_unterminated_string_is_blanked_to_its_line_break_and_replaced() {
        assert_eq!(
            recovered("const s = 'abc\nconst y = 2;\n"),
            "const s =_    \nconst y = 2;\n"
        );
    }

    #[test]
    fn a_missing_semicolon_still_becomes_a_line_break() {
        assert_eq!(recovered("let b = 1 let c = 2\n"), "let b = 1\nlet c = 2\n");
    }

    #[test]
    fn multibyte_text_is_blanked_byte_for_byte() {
        assert_eq!(recovered("const é = ü."), "const é = ü ");
        assert_eq!(recovered("const s = 'ü\n"), "const s =_   \n");
    }
}
