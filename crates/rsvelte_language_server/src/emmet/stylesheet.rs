//! The stylesheet abbreviation tokenizer and parser: `tokenize` and `parser`
//! in emmet 2.4.11 (`@emmetio/css-abbreviation`). Works on UTF-16 code
//! units, as JavaScript's strings do.

use super::EmmetError;

/// A token, or a value built from tokens. `start` and `end` are where the
/// tokenizer found it; a value emmet builds itself has none (`undefined`),
/// which `outputValue` compares as such.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Token {
    pub kind: Kind,
    pub start: Option<usize>,
    pub end: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Kind {
    /// `${1:name}` (a field) or `${name}` (a variable, `index: None`).
    Field {
        index: Option<usize>,
        name: String,
    },
    CustomProperty(String),
    Number {
        value: f64,
        raw: String,
        unit: String,
    },
    Color(Color),
    String {
        value: String,
        single: bool,
    },
    Bracket {
        open: bool,
    },
    /// `+`, `!`, `,`, `:` or `-`.
    Operator(char),
    WhiteSpace,
    Literal(String),
    FunctionCall {
        name: String,
        arguments: Vec<CssValue>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Color {
    pub r: u32,
    pub g: u32,
    pub b: u32,
    pub a: f64,
}

/// emmet's `CSSValue`: the tokens of one comma-separated value.
pub(super) type CssValue = Vec<Token>;

/// emmet's `CSSProperty`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Property {
    pub name: Option<String>,
    pub value: Vec<CssValue>,
    pub important: bool,
}

impl Token {
    pub(super) const fn new(kind: Kind) -> Self {
        Self {
            kind,
            start: None,
            end: None,
        }
    }

    pub(super) fn literal(value: impl Into<String>) -> Self {
        Self::new(Kind::Literal(value.into()))
    }

    pub(super) fn field(index: usize, name: impl Into<String>) -> Self {
        Self::new(Kind::Field {
            index: Some(index),
            name: name.into(),
        })
    }
}

/// `parse` in `@emmetio/css-abbreviation`: `value` parses a property's value
/// (as a snippet's), not a list of properties.
pub(super) fn parse(abbreviation: &str, value: bool) -> Result<Vec<Property>, EmmetError> {
    let text: Vec<u16> = abbreviation.encode_utf16().collect();
    let tokens = tokenize(&text, value)?;
    parser(&tokens, value)
}

struct Scanner<'a> {
    text: &'a [u16],
    pos: usize,
    start: usize,
}

impl Scanner<'_> {
    fn eof(&self) -> bool {
        self.pos >= self.text.len()
    }

    fn peek(&self) -> Option<u16> {
        self.text.get(self.pos).copied()
    }

    fn eat(&mut self, test: impl Fn(u16) -> bool) -> bool {
        let ok = self.peek().is_some_and(test);
        if ok {
            self.pos += 1;
        }
        ok
    }

    fn eat_char(&mut self, ch: u8) -> bool {
        self.eat(|c| c == u16::from(ch))
    }

    fn eat_while(&mut self, test: impl Fn(u16) -> bool) -> bool {
        let start = self.pos;
        while !self.eof() && self.eat(&test) {}
        self.pos != start
    }

    fn substring(&self, start: usize, end: usize) -> String {
        String::from_utf16_lossy(&self.text[start.min(end)..end.min(self.text.len())])
    }

    fn current(&self) -> String {
        self.substring(self.start, self.pos)
    }

    fn token(&self, kind: Kind, start: usize) -> Token {
        Token {
            kind,
            start: Some(start),
            end: Some(self.pos),
        }
    }
}

fn tokenize(text: &[u16], value: bool) -> Result<Vec<Token>, EmmetError> {
    let mut brackets: i64 = 0;
    let mut scanner = Scanner {
        text,
        pos: 0,
        start: 0,
    };
    let mut tokens: Vec<Token> = Vec::new();
    while !scanner.eof() {
        let token = token(&mut scanner, brackets == 0 && !value)?.ok_or(EmmetError)?;
        if let Kind::Bracket { open } = token.kind {
            if brackets == 0 && open {
                merge_tokens(&scanner, &mut tokens);
            }
            brackets += if open { 1 } else { -1 };
            if brackets < 0 {
                return Err(EmmetError);
            }
        }
        // A dash right after a unit-less number or a colour separates values.
        let consume_dash = match &token.kind {
            Kind::Color(_) => true,
            Kind::Number { unit, .. } => unit.is_empty(),
            _ => false,
        };
        tokens.push(token);
        if consume_dash && let Some(operator) = operator(&mut scanner) {
            tokens.push(operator);
        }
    }
    Ok(tokens)
}

/// `getToken`: `short` reads a literal of letters only, so `p10` is `p`
/// and `10`.
fn token(scanner: &mut Scanner<'_>, short: bool) -> Result<Option<Token>, EmmetError> {
    if let Some(field) = field(scanner)? {
        return Ok(Some(field));
    }
    Ok(custom_property(scanner)
        .or_else(|| number_value(scanner))
        .or_else(|| color_value(scanner))
        .or_else(|| string_value(scanner))
        .or_else(|| bracket(scanner))
        .or_else(|| operator(scanner))
        .or_else(|| white_space(scanner))
        .or_else(|| literal(scanner, short)))
}

fn field(scanner: &mut Scanner<'_>) -> Result<Option<Token>, EmmetError> {
    let start = scanner.pos;
    if scanner.eat_char(b'$') && scanner.eat_char(b'{') {
        scanner.start = scanner.pos;
        let mut index = None;
        let mut name = String::new();
        if scanner.eat_while(is_number) {
            index = Some(scanner.current().parse().map_err(|_| EmmetError)?);
            if scanner.eat_char(b':') {
                name = consume_placeholder(scanner)?;
            }
        } else if scanner.peek().is_some_and(is_alpha) {
            name = consume_placeholder(scanner)?;
        }
        if scanner.eat_char(b'}') {
            return Ok(Some(scanner.token(Kind::Field { index, name }, start)));
        }
        return Err(EmmetError);
    }
    scanner.pos = start;
    Ok(None)
}

/// A field's placeholder, which may hold balanced braces.
fn consume_placeholder(scanner: &mut Scanner<'_>) -> Result<String, EmmetError> {
    let mut stack = Vec::new();
    scanner.start = scanner.pos;
    while !scanner.eof() {
        if scanner.eat_char(b'{') {
            stack.push(scanner.pos);
        } else if scanner.eat_char(b'}') {
            if stack.pop().is_none() {
                scanner.pos -= 1;
                break;
            }
        } else {
            scanner.pos += 1;
        }
    }
    if !stack.is_empty() {
        return Err(EmmetError);
    }
    Ok(scanner.current())
}

/// `--foo-bar`.
fn custom_property(scanner: &mut Scanner<'_>) -> Option<Token> {
    let start = scanner.pos;
    if scanner.eat_char(b'-') && scanner.eat_char(b'-') {
        scanner.start = start;
        scanner.eat_while(is_keyword);
        return Some(scanner.token(Kind::CustomProperty(scanner.current()), start));
    }
    scanner.pos = start;
    None
}

/// A number with an optional unit: `%` or a word.
fn number_value(scanner: &mut Scanner<'_>) -> Option<Token> {
    let start = scanner.pos;
    if !consume_number(scanner) {
        return None;
    }
    scanner.start = start;
    let raw = scanner.current();
    scanner.start = scanner.pos;
    if !scanner.eat_char(b'%') {
        scanner.eat_while(is_alpha_word);
    }
    let unit = scanner.current();
    let value = js_number(&raw);
    Some(scanner.token(Kind::Number { value, raw, unit }, start))
}

/// JavaScript's `Number()` of what `consumeNumber` read: digits with an
/// optional sign and dot (`1.`, `.5`, `-.5`).
fn js_number(raw: &str) -> f64 {
    let digits = raw.strip_suffix('.').unwrap_or(raw);
    let digits = if let Some(rest) = digits.strip_prefix("-.") {
        format!("-0.{rest}")
    } else if let Some(rest) = digits.strip_prefix('.') {
        format!("0.{rest}")
    } else {
        digits.to_string()
    };
    digits.parse().unwrap_or(f64::NAN)
}

/// A quoted string; an unterminated one runs to the end.
fn string_value(scanner: &mut Scanner<'_>) -> Option<Token> {
    let quote = scanner.peek().filter(|&ch| is_quote(ch))?;
    let start = scanner.pos;
    let mut finished = false;
    scanner.pos += 1;
    while !scanner.eof() {
        if scanner.eat(|ch| ch == quote) {
            finished = true;
            break;
        }
        scanner.pos += 1;
    }
    scanner.start = start;
    let value = scanner.substring(start + 1, scanner.pos - usize::from(finished));
    Some(scanner.token(
        Kind::String {
            value,
            single: quote == u16::from(b'\''),
        },
        start,
    ))
}

/// `#abc` → `#aabbcc`, `#0` → `#000000`, `#fff.5` → `rgba(255, 255, 255,
/// 0.5)`, `#t` → `transparent`.
fn color_value(scanner: &mut Scanner<'_>) -> Option<Token> {
    let start = scanner.pos;
    if !scanner.eat_char(b'#') {
        scanner.pos = start;
        return None;
    }
    let value_start = scanner.pos;
    let mut color = String::new();
    let alpha;
    if scanner.eat_while(is_hex) {
        color = scanner.substring(value_start, scanner.pos);
        alpha = color_alpha(scanner);
    } else if scanner.eat_char(b't') {
        color = "0".to_string();
        alpha = Some(color_alpha(scanner).unwrap_or_else(|| "0".to_string()));
    } else {
        alpha = color_alpha(scanner);
    }
    if !color.is_empty() || alpha.is_some() || scanner.eof() {
        return Some(scanner.token(Kind::Color(parse_color(&color, alpha.as_deref())), start));
    }
    // A `#` with no value is a literal.
    Some(scanner.token(Kind::Literal(scanner.substring(start, scanner.pos)), start))
}

/// A colour's `.1` alpha; a lone dot is `1`.
fn color_alpha(scanner: &mut Scanner<'_>) -> Option<String> {
    let start = scanner.pos;
    if !scanner.eat_char(b'.') {
        return None;
    }
    scanner.start = start;
    if scanner.eat_while(is_number) {
        return Some(scanner.current());
    }
    Some("1".to_string())
}

fn parse_color(value: &str, alpha: Option<&str>) -> Color {
    let channel = |hex: &str| u32::from_str_radix(hex, 16).unwrap_or(0);
    let (r, g, b) = match value.len() {
        0 => ("0".to_string(), "0".to_string(), "0".to_string()),
        1 => {
            let pair = value.repeat(2);
            (pair.clone(), pair.clone(), pair)
        }
        2 => (value.to_string(), value.to_string(), value.to_string()),
        3 => {
            let pair = |index: usize| value[index..=index].repeat(2);
            (pair(0), pair(1), pair(2))
        }
        _ => {
            let doubled = value.repeat(2);
            (
                doubled[0..2].to_string(),
                doubled[2..4].to_string(),
                doubled[4..6].to_string(),
            )
        }
    };
    Color {
        r: channel(&r),
        g: channel(&g),
        b: channel(&b),
        a: alpha.map_or(1.0, js_number),
    }
}

fn white_space(scanner: &mut Scanner<'_>) -> Option<Token> {
    let start = scanner.pos;
    scanner
        .eat_while(is_space)
        .then(|| scanner.token(Kind::WhiteSpace, start))
}

fn bracket(scanner: &mut Scanner<'_>) -> Option<Token> {
    let ch = scanner.peek()?;
    if ch != u16::from(b'(') && ch != u16::from(b')') {
        return None;
    }
    let start = scanner.pos;
    scanner.pos += 1;
    Some(scanner.token(
        Kind::Bracket {
            open: ch == u16::from(b'('),
        },
        start,
    ))
}

fn operator(scanner: &mut Scanner<'_>) -> Option<Token> {
    let ch = scanner
        .peek()
        .and_then(|ch| char::from_u32(u32::from(ch)))
        .filter(|ch| matches!(ch, '+' | '!' | ',' | ':' | '-'))?;
    let start = scanner.pos;
    scanner.pos += 1;
    Some(scanner.token(Kind::Operator(ch), start))
}

/// `short` reads letters, `%` and `/` only; otherwise numbers and dashes
/// too. A SCSS or LESS variable at the start reads letters only, so a
/// variable can be embedded.
fn literal(scanner: &mut Scanner<'_>, short: bool) -> Option<Token> {
    let start = scanner.pos;
    if scanner.eat(is_ident_prefix) {
        if start == 0 {
            scanner.eat_while(is_literal);
        } else {
            scanner.eat_while(is_keyword);
        }
    } else if scanner.eat(is_alpha_word) {
        if short {
            scanner.eat_while(is_literal);
        } else {
            scanner.eat_while(is_keyword);
        }
    } else {
        // A dot only at the start.
        scanner.eat_char(b'.');
        scanner.eat_while(is_literal);
    }
    if start == scanner.pos {
        return None;
    }
    scanner.start = start;
    Some(scanner.token(Kind::Literal(scanner.substring(start, scanner.pos)), start))
}

/// `-?\d*(\.\d*)?`, at least one digit.
fn consume_number(scanner: &mut Scanner<'_>) -> bool {
    let start = scanner.pos;
    scanner.eat_char(b'-');
    let after_negative = scanner.pos;
    let has_decimal = scanner.eat_while(is_number);
    let before_dot = scanner.pos;
    if scanner.eat_char(b'.') {
        // `1.` is a float.
        let has_float = scanner.eat_while(is_number);
        if !has_decimal && !has_float {
            // A lone dot.
            scanner.pos = before_dot;
        }
    }
    if scanner.pos == after_negative {
        scanner.pos = start;
    }
    scanner.pos != start
}

/// `mergeTokens`: the literals and numbers right before a function's `(`
/// become one literal, so `scale3d(` names `scale3d`.
fn merge_tokens(scanner: &Scanner<'_>, tokens: &mut Vec<Token>) {
    let mut start = 0;
    let mut end = 0;
    while let Some(token) = tokens.last() {
        if !matches!(token.kind, Kind::Literal(_) | Kind::Number { .. }) {
            break;
        }
        start = token.start.unwrap_or(0);
        if end == 0 {
            end = token.end.unwrap_or(0);
        }
        tokens.pop();
    }
    if start != end {
        tokens.push(Token {
            kind: Kind::Literal(scanner.substring(start, end)),
            start: Some(start),
            end: Some(end),
        });
    }
}

const fn is_number(ch: u16) -> bool {
    ch > 47 && ch < 58
}

/// emmet's `ch &= ~32` letter test.
const fn is_alpha(ch: u16) -> bool {
    let upper = ch & !32;
    upper >= b'A' as u16 && upper <= b'Z' as u16
}

const fn is_alpha_word(ch: u16) -> bool {
    ch == b'_' as u16 || is_alpha(ch)
}

const fn is_keyword(ch: u16) -> bool {
    is_number(ch) || is_alpha_word(ch) || ch == b'-' as u16
}

const fn is_literal(ch: u16) -> bool {
    is_alpha_word(ch) || ch == b'%' as u16 || ch == b'/' as u16
}

const fn is_hex(ch: u16) -> bool {
    let upper = ch & !32;
    is_number(ch) || upper >= b'A' as u16 && upper <= b'F' as u16
}

const fn is_ident_prefix(ch: u16) -> bool {
    ch == b'@' as u16 || ch == b'$' as u16
}

const fn is_quote(ch: u16) -> bool {
    ch == b'\'' as u16 || ch == b'"' as u16
}

/// A space, a tab, a non-breaking space or a line break.
const fn is_space(ch: u16) -> bool {
    matches!(ch, 32 | 9 | 160 | 10 | 13)
}

struct Tokens<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl Tokens<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn readable(&self) -> bool {
        self.pos < self.tokens.len()
    }

    fn consume(&mut self, test: impl Fn(&Token) -> bool) -> bool {
        let ok = self.peek().is_some_and(test);
        if ok {
            self.pos += 1;
        }
        ok
    }
}

fn parser(tokens: &[Token], value: bool) -> Result<Vec<Property>, EmmetError> {
    let mut scanner = Tokens { tokens, pos: 0 };
    let mut result = Vec::new();
    while scanner.readable() {
        if let Some(property) = consume_property(&mut scanner, value)? {
            result.push(property);
        } else if !scanner.consume(|token| is_operator(token, '+')) {
            return Err(EmmetError);
        }
    }
    Ok(result)
}

fn consume_property(
    scanner: &mut Tokens<'_>,
    value_mode: bool,
) -> Result<Option<Property>, EmmetError> {
    let mut name = None;
    let mut important = false;
    let mut value = Vec::new();
    if !value_mode
        && let Some(Token {
            kind: Kind::Literal(literal),
            ..
        }) = scanner.peek()
        && !is_function_start(scanner)
    {
        name = Some(literal.clone());
        scanner.pos += 1;
        // A value delimiter right after the name.
        scanner.consume(is_value_delimiter);
    }
    if value_mode {
        scanner.consume(|token| token.kind == Kind::WhiteSpace);
    }
    while scanner.readable() {
        if scanner.consume(|token| is_operator(token, '!')) {
            important = true;
        } else if let Some(fragment) = consume_value(scanner, value_mode)? {
            value.push(fragment);
        } else if !scanner.consume(|token| is_operator(token, ',')) {
            break;
        }
    }
    Ok(
        (name.is_some() || !value.is_empty() || important).then_some(Property {
            name,
            value,
            important,
        }),
    )
}

/// One value: the tokens before a comma.
fn consume_value(
    scanner: &mut Tokens<'_>,
    in_argument: bool,
) -> Result<Option<CssValue>, EmmetError> {
    let mut result = Vec::new();
    while let Some(token) = scanner.peek() {
        if is_value(token) {
            let token = token.clone();
            scanner.pos += 1;
            if let Kind::Literal(name) = &token.kind
                && let Some(arguments) = consume_arguments(scanner)?
            {
                result.push(Token::new(Kind::FunctionCall {
                    name: name.clone(),
                    arguments,
                }));
            } else {
                result.push(token);
            }
        } else if is_value_delimiter(token) || in_argument && token.kind == Kind::WhiteSpace {
            scanner.pos += 1;
        } else {
            break;
        }
    }
    Ok((!result.is_empty()).then_some(result))
}

fn consume_arguments(scanner: &mut Tokens<'_>) -> Result<Option<Vec<CssValue>>, EmmetError> {
    if !scanner.consume(|token| token.kind == Kind::Bracket { open: true }) {
        return Ok(None);
    }
    let mut arguments = Vec::new();
    while scanner.readable()
        && !scanner.consume(|token| token.kind == Kind::Bracket { open: false })
    {
        if let Some(value) = consume_value(scanner, true)? {
            arguments.push(value);
        } else if !scanner.consume(|token| token.kind == Kind::WhiteSpace)
            && !scanner.consume(|token| is_operator(token, ','))
        {
            return Err(EmmetError);
        }
    }
    Ok(Some(arguments))
}

fn is_operator(token: &Token, operator: char) -> bool {
    token.kind == Kind::Operator(operator)
}

fn is_value_delimiter(token: &Token) -> bool {
    is_operator(token, ':') || is_operator(token, '-')
}

const fn is_value(token: &Token) -> bool {
    matches!(
        token.kind,
        Kind::String { .. }
            | Kind::Color(_)
            | Kind::Number { .. }
            | Kind::Literal(_)
            | Kind::Field { .. }
            | Kind::CustomProperty(_)
    )
}

fn is_function_start(scanner: &Tokens<'_>) -> bool {
    matches!(
        (
            scanner.tokens.get(scanner.pos),
            scanner.tokens.get(scanner.pos + 1)
        ),
        (
            Some(Token {
                kind: Kind::Literal(_),
                ..
            }),
            Some(Token {
                kind: Kind::Bracket { .. },
                ..
            })
        )
    )
}
