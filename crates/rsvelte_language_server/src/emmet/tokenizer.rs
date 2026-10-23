//! The markup abbreviation tokenizer: `tokenize` in emmet 2.4.11
//! (`@emmetio/abbreviation`, `tokenizer/index.ts`).

use super::EmmetError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BracketContext {
    Group,
    Attribute,
    Expression,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operator {
    Child,
    Class,
    Climb,
    Id,
    Equal,
    Close,
    Sibling,
}

impl Operator {
    pub(super) const fn symbol(self) -> char {
        match self {
            Self::Child => '>',
            Self::Class => '.',
            Self::Climb => '^',
            Self::Id => '#',
            Self::Equal => '=',
            Self::Close => '/',
            Self::Sibling => '+',
        }
    }
}

/// `*N` (or a bare `*`, `implicit`), and while converting, the iteration it is at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Repeater {
    pub count: usize,
    pub value: usize,
    pub implicit: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Token {
    Literal(String),
    Quote {
        single: bool,
    },
    Bracket {
        open: bool,
        context: BracketContext,
    },
    Operator(Operator),
    Repeater(Repeater),
    RepeaterNumber {
        size: usize,
        reverse: bool,
        base: usize,
        parent: usize,
    },
    RepeaterPlaceholder,
    /// `${1:name}` (a field) or `${name}` (a variable, `index: None`).
    Field {
        index: Option<usize>,
        name: String,
    },
    WhiteSpace(String),
}

#[derive(Default)]
struct Context {
    group: i64,
    attribute: i64,
    expression: i64,
    quote: u32,
}

impl Context {
    fn bracket(&mut self, context: BracketContext) -> &mut i64 {
        match context {
            BracketContext::Group => &mut self.group,
            BracketContext::Attribute => &mut self.attribute,
            BracketContext::Expression => &mut self.expression,
        }
    }
}

/// A character reader over `chars`: emmet's `Scanner`.
struct Scanner<'a> {
    chars: &'a [char],
    pos: usize,
    start: usize,
}

impl Scanner<'_> {
    fn eof(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn at(&self, pos: usize) -> Option<char> {
        self.chars.get(pos).copied()
    }

    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_if(&mut self, test: impl Fn(char) -> bool) -> bool {
        if self.peek().is_some_and(&test) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_while(&mut self, test: impl Fn(char) -> bool) -> bool {
        let start = self.pos;
        while !self.eof() && self.eat_if(&test) {}
        self.pos != start
    }

    fn eat_while_char(&mut self, ch: char) -> bool {
        self.eat_while(|c| c == ch)
    }

    fn current(&self) -> String {
        self.chars[self.start..self.pos].iter().collect()
    }
}

pub(super) const fn is_number(ch: char) -> bool {
    ch.is_ascii_digit()
}

fn is_alpha(ch: char) -> bool {
    // emmet's `code &= ~32` upper-cases ASCII and also accepts a few others.
    let code = (ch as u32) & !32;
    (65..=90).contains(&code)
}

fn is_alpha_word(ch: char) -> bool {
    ch == '_' || is_alpha(ch)
}

fn is_umlaut(ch: char) -> bool {
    matches!(ch as u32, 196 | 214 | 220 | 228 | 246 | 252)
}

pub(super) const fn is_white_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\u{a0}')
}

pub(super) const fn is_space(ch: char) -> bool {
    is_white_space(ch) || matches!(ch, '\n' | '\r')
}

const fn is_quote(ch: char) -> bool {
    matches!(ch, '\'' | '"')
}

fn is_element_name(ch: char) -> bool {
    is_number(ch) || is_alpha_word(ch) || is_umlaut(ch) || matches!(ch, '-' | ':' | '!')
}

const fn bracket_type(ch: char) -> Option<BracketContext> {
    match ch {
        '(' | ')' => Some(BracketContext::Group),
        '[' | ']' => Some(BracketContext::Attribute),
        '{' | '}' => Some(BracketContext::Expression),
        _ => None,
    }
}

const fn operator_type(ch: char) -> Option<Operator> {
    match ch {
        '>' => Some(Operator::Child),
        '+' => Some(Operator::Sibling),
        '^' => Some(Operator::Climb),
        '.' => Some(Operator::Class),
        '#' => Some(Operator::Id),
        '/' => Some(Operator::Close),
        '=' => Some(Operator::Equal),
        _ => None,
    }
}

fn is_allowed_operator(ch: char, ctx: &Context) -> bool {
    match operator_type(ch) {
        Some(op) if ctx.quote == 0 && ctx.expression == 0 => {
            ctx.attribute == 0 || op == Operator::Equal
        }
        _ => false,
    }
}

fn is_allowed_space(ch: char, ctx: &Context) -> bool {
    is_space(ch) && ctx.expression == 0
}

fn is_allowed_repeater(ch: char, ctx: &Context) -> bool {
    ch == '*' && ctx.attribute == 0 && ctx.expression == 0
}

pub(super) fn tokenize(source: &str) -> Result<Vec<Token>, EmmetError> {
    let chars: Vec<char> = source.chars().collect();
    let mut scanner = Scanner {
        chars: &chars,
        pos: 0,
        start: 0,
    };
    let mut result = Vec::new();
    let mut ctx = Context::default();
    while !scanner.eof() {
        let ch = scanner.peek().unwrap_or_default();
        let Some(token) = token(&mut scanner, &mut ctx)? else {
            return Err(EmmetError);
        };
        match &token {
            Token::Quote { .. } => {
                ctx.quote = if ch as u32 == ctx.quote { 0 } else { ch as u32 };
            }
            Token::Bracket { open, context } => {
                *ctx.bracket(*context) += if *open { 1 } else { -1 };
            }
            _ => {}
        }
        result.push(token);
    }
    Ok(result)
}

fn token(scanner: &mut Scanner<'_>, ctx: &mut Context) -> Result<Option<Token>, EmmetError> {
    if let Some(token) = field(scanner, ctx)? {
        return Ok(Some(token));
    }
    Ok(repeater_placeholder(scanner)
        .or_else(|| repeater_number(scanner))
        .or_else(|| repeater(scanner))
        .or_else(|| white_space(scanner))
        .or_else(|| literal(scanner, ctx))
        .or_else(|| operator(scanner))
        .or_else(|| quote(scanner))
        .or_else(|| bracket(scanner)))
}

/// If an escape is consumed, the scanner's range is the escaped character.
fn escaped(scanner: &mut Scanner<'_>) -> bool {
    if scanner.eat('\\') {
        scanner.start = scanner.pos;
        if !scanner.eof() {
            scanner.pos += 1;
        }
        return true;
    }
    false
}

fn literal(scanner: &mut Scanner<'_>, ctx: &mut Context) -> Option<Token> {
    let start = scanner.pos;
    let expression_start = ctx.expression;
    let mut value = String::new();
    while !scanner.eof() {
        if escaped(scanner) {
            value.push_str(&scanner.current());
            continue;
        }
        let ch = scanner.peek()?;
        if ch == '/' && ctx.quote == 0 && ctx.expression == 0 && ctx.attribute == 0 {
            // `/` between numbers in a class name (`.w-1/2`).
            let previous = scanner.pos.checked_sub(1).and_then(|pos| scanner.at(pos));
            let next = scanner.at(scanner.pos + 1);
            if previous.is_some_and(is_number) && next.is_some_and(is_number) {
                value.push(ch);
                scanner.pos += 1;
                continue;
            }
        }
        if ch as u32 == ctx.quote || ch == '$' || is_allowed_operator(ch, ctx) {
            break;
        }
        if expression_start != 0 {
            // Nested expressions, e.g. `span{{foo}}`.
            if ch == '{' {
                ctx.expression += 1;
            } else if ch == '}' {
                if ctx.expression > expression_start {
                    ctx.expression -= 1;
                } else {
                    break;
                }
            }
        } else if ctx.quote == 0 {
            if ctx.attribute == 0 && !is_element_name(ch) {
                break;
            }
            if is_allowed_space(ch, ctx)
                || is_allowed_repeater(ch, ctx)
                || is_quote(ch)
                || bracket_type(ch).is_some()
            {
                break;
            }
        }
        value.push(ch);
        scanner.pos += 1;
    }
    if start != scanner.pos {
        scanner.start = start;
        return Some(Token::Literal(value));
    }
    None
}

fn white_space(scanner: &mut Scanner<'_>) -> Option<Token> {
    let start = scanner.pos;
    scanner
        .eat_while(is_space)
        .then(|| Token::WhiteSpace(scanner.chars[start..scanner.pos].iter().collect()))
}

fn quote(scanner: &mut Scanner<'_>) -> Option<Token> {
    let ch = scanner.peek()?;
    is_quote(ch).then(|| {
        scanner.pos += 1;
        Token::Quote { single: ch == '\'' }
    })
}

fn bracket(scanner: &mut Scanner<'_>) -> Option<Token> {
    let ch = scanner.peek()?;
    let context = bracket_type(ch)?;
    scanner.pos += 1;
    Some(Token::Bracket {
        open: matches!(ch, '{' | '[' | '('),
        context,
    })
}

fn operator(scanner: &mut Scanner<'_>) -> Option<Token> {
    let op = operator_type(scanner.peek()?)?;
    scanner.pos += 1;
    Some(Token::Operator(op))
}

fn repeater(scanner: &mut Scanner<'_>) -> Option<Token> {
    if !scanner.eat('*') {
        return None;
    }
    scanner.start = scanner.pos;
    let mut count = 1;
    let mut implicit = false;
    if scanner.eat_while(is_number) {
        count = scanner.current().parse().unwrap_or(usize::MAX);
    } else {
        implicit = true;
    }
    Some(Token::Repeater(Repeater {
        count,
        value: 0,
        implicit,
    }))
}

fn repeater_placeholder(scanner: &mut Scanner<'_>) -> Option<Token> {
    let start = scanner.pos;
    if scanner.eat('$') && scanner.eat('#') {
        return Some(Token::RepeaterPlaceholder);
    }
    scanner.pos = start;
    None
}

fn repeater_number(scanner: &mut Scanner<'_>) -> Option<Token> {
    let start = scanner.pos;
    if !scanner.eat_while_char('$') {
        return None;
    }
    let size = scanner.pos - start;
    let mut reverse = false;
    let mut base = 1;
    let mut parent = 0;
    if scanner.eat('@') {
        while scanner.eat('^') {
            parent += 1;
        }
        reverse = scanner.eat('-');
        scanner.start = scanner.pos;
        if scanner.eat_while(is_number) {
            base = scanner.current().parse().unwrap_or(usize::MAX);
        }
    }
    scanner.start = start;
    Some(Token::RepeaterNumber {
        size,
        reverse,
        base,
        parent,
    })
}

fn field(scanner: &mut Scanner<'_>, ctx: &Context) -> Result<Option<Token>, EmmetError> {
    let start = scanner.pos;
    if (ctx.expression != 0 || ctx.attribute != 0) && scanner.eat('$') && scanner.eat('{') {
        scanner.start = scanner.pos;
        let mut index = None;
        let mut name = String::new();
        if scanner.eat_while(is_number) {
            index = Some(scanner.current().parse().unwrap_or(usize::MAX));
            if scanner.eat(':') {
                name = consume_placeholder(scanner)?;
            }
        } else if scanner.peek().is_some_and(is_alpha) {
            name = consume_placeholder(scanner)?;
        }
        if scanner.eat('}') {
            return Ok(Some(Token::Field { index, name }));
        }
        return Err(EmmetError);
    }
    scanner.pos = start;
    Ok(None)
}

/// The value after `:` in a field, which may be empty.
fn consume_placeholder(scanner: &mut Scanner<'_>) -> Result<String, EmmetError> {
    let mut stack = Vec::new();
    scanner.start = scanner.pos;
    while !scanner.eof() {
        if scanner.eat('{') {
            stack.push(scanner.pos);
        } else if scanner.eat('}') {
            if stack.is_empty() {
                scanner.pos -= 1;
                break;
            }
            stack.pop();
        } else {
            scanner.pos += 1;
        }
    }
    if !stack.is_empty() {
        return Err(EmmetError);
    }
    Ok(scanner.current())
}
