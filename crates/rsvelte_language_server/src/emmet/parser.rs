//! The markup abbreviation parser: `abbreviation` in emmet 2.4.11
//! (`@emmetio/abbreviation`, `parser/index.ts`), without its JSX branches,
//! which the `html` syntax never enables.

use super::EmmetError;
use super::tokenizer::{BracketContext, Operator, Repeater, Token};

pub(super) enum Statement {
    Element(Element),
    Group(Group),
}

#[derive(Default)]
pub(super) struct Group {
    pub elements: Vec<Statement>,
    pub repeat: Option<Repeater>,
}

/// Emmet's `TokenElement`. `None` and an empty list differ: an empty list is
/// still a value (`div{}`) or an attribute set (`div[]`).
#[derive(Default)]
pub(super) struct Element {
    pub name: Option<Vec<Token>>,
    pub attributes: Option<Vec<Attribute>>,
    pub value: Option<Vec<Token>>,
    pub repeat: Option<Repeater>,
    pub self_close: bool,
    pub elements: Vec<Statement>,
}

impl Element {
    const fn is_empty(&self) -> bool {
        self.name.is_none() && self.value.is_none() && self.attributes.is_none()
    }
}

pub(super) struct Attribute {
    pub name: Option<Vec<Token>>,
    pub value: Option<Vec<Token>>,
}

struct Scanner<'a> {
    tokens: &'a [Token],
    start: usize,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn readable(&self) -> bool {
        self.pos < self.tokens.len()
    }

    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    /// The current token, moving past it even at the end, as emmet's `next`.
    fn next(&mut self) -> Option<&'a Token> {
        let token = self.tokens.get(self.pos);
        self.pos += 1;
        token
    }

    fn consume(&mut self, test: impl Fn(&Token) -> bool) -> bool {
        if self.peek().is_some_and(test) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn slice(&self, from: usize, to: usize) -> Vec<Token> {
        self.tokens
            .get(from..to.min(self.tokens.len()))
            .map_or_else(Vec::new, <[Token]>::to_vec)
    }

    fn current(&self) -> Vec<Token> {
        self.slice(self.start, self.pos)
    }
}

pub(super) fn parse(tokens: &[Token]) -> Result<Group, EmmetError> {
    let mut scanner = Scanner {
        tokens,
        start: 0,
        pos: 0,
    };
    let result = statements(&mut scanner)?;
    if scanner.readable() {
        return Err(EmmetError);
    }
    Ok(result)
}

fn statements(scanner: &mut Scanner<'_>) -> Result<Group, EmmetError> {
    let mut result = Group::default();
    // Where new statements go, as a path of indices from `result`.
    let mut ctx: Vec<usize> = Vec::new();
    let mut stack: Vec<Vec<usize>> = Vec::new();
    while scanner.readable() {
        let node = match element(scanner)? {
            Some(element) => Statement::Element(element),
            None => match group(scanner)? {
                Some(group) => Statement::Group(group),
                None => break,
            },
        };
        let elements = children_at(&mut result, &ctx);
        elements.push(node);
        let index = elements.len() - 1;
        if scanner.consume(|token| is_operator(token, Operator::Child)) {
            stack.push(ctx.clone());
            ctx.push(index);
        } else if scanner.consume(|token| is_operator(token, Operator::Sibling)) {
        } else if scanner.consume(|token| is_operator(token, Operator::Climb)) {
            loop {
                if let Some(previous) = stack.pop() {
                    ctx = previous;
                }
                if !scanner.consume(|token| is_operator(token, Operator::Climb)) {
                    break;
                }
            }
        }
    }
    Ok(result)
}

fn children_at<'a>(group: &'a mut Group, path: &[usize]) -> &'a mut Vec<Statement> {
    let mut elements = &mut group.elements;
    for &index in path {
        elements = match &mut elements[index] {
            Statement::Element(element) => &mut element.elements,
            Statement::Group(group) => &mut group.elements,
        };
    }
    elements
}

fn group(scanner: &mut Scanner<'_>) -> Result<Option<Group>, EmmetError> {
    if !scanner.consume(|token| is_bracket(token, BracketContext::Group, Some(true))) {
        return Ok(None);
    }
    let mut result = statements(scanner)?;
    // Whatever follows is consumed, closing bracket or not.
    if scanner
        .next()
        .is_some_and(|token| is_bracket(token, BracketContext::Group, Some(false)))
    {
        result.repeat = repeater(scanner);
    }
    Ok(Some(result))
}

fn element(scanner: &mut Scanner<'_>) -> Result<Option<Element>, EmmetError> {
    let mut elem = Element::default();
    if element_name(scanner) {
        elem.name = Some(scanner.current());
    }
    while scanner.readable() {
        scanner.start = scanner.pos;
        if elem.repeat.is_none()
            && !elem.is_empty()
            && let Some(Token::Repeater(repeat)) = scanner.peek()
        {
            elem.repeat = Some(*repeat);
            scanner.pos += 1;
        } else if elem.value.is_none() && text(scanner) {
            elem.value = Some(text_value(scanner));
        } else if let Some(attributes) = match short_attribute(scanner, Operator::Id) {
            Some(attribute) => Some(vec![attribute]),
            None => match short_attribute(scanner, Operator::Class) {
                Some(attribute) => Some(vec![attribute]),
                None => attribute_set(scanner)?,
            },
        } {
            elem.attributes.get_or_insert_default().extend(attributes);
        } else {
            if !elem.is_empty() && scanner.consume(|token| is_operator(token, Operator::Close)) {
                elem.self_close = true;
                if elem.repeat.is_none()
                    && let Some(Token::Repeater(repeat)) = scanner.peek()
                {
                    elem.repeat = Some(*repeat);
                    scanner.pos += 1;
                }
            }
            break;
        }
    }
    Ok((!elem.is_empty()).then_some(elem))
}

fn attribute_set(scanner: &mut Scanner<'_>) -> Result<Option<Vec<Attribute>>, EmmetError> {
    if !scanner.consume(|token| is_bracket(token, BracketContext::Attribute, Some(true))) {
        return Ok(None);
    }
    let mut attributes = Vec::new();
    while scanner.readable() {
        if let Some(attr) = attribute(scanner)? {
            attributes.push(attr);
        } else if scanner.consume(|token| is_bracket(token, BracketContext::Attribute, Some(false)))
        {
            break;
        } else if !scanner.consume(|token| matches!(token, Token::WhiteSpace(_))) {
            return Err(EmmetError);
        }
    }
    Ok(Some(attributes))
}

/// `#id` or `.class`. A repeated operator (`..x`) marks the attribute
/// multiple, which only JSX output reads.
fn short_attribute(scanner: &mut Scanner<'_>, kind: Operator) -> Option<Attribute> {
    if !scanner.peek().is_some_and(|token| is_operator(token, kind)) {
        return None;
    }
    while scanner.peek().is_some_and(|token| is_operator(token, kind)) {
        scanner.pos += 1;
    }
    let name = if kind == Operator::Id { "id" } else { "class" };
    Some(Attribute {
        name: Some(vec![Token::Literal(name.to_string())]),
        value: literal(scanner, false).then(|| scanner.current()),
    })
}

fn attribute(scanner: &mut Scanner<'_>) -> Result<Option<Attribute>, EmmetError> {
    if quoted(scanner)? {
        // A quoted value alone belongs to the default attribute.
        return Ok(Some(Attribute {
            name: None,
            value: Some(scanner.current()),
        }));
    }
    if literal(scanner, true) {
        let name = scanner.current();
        let mut value = None;
        if scanner.consume(|token| is_operator(token, Operator::Equal))
            && (quoted(scanner)? || literal(scanner, true))
        {
            value = Some(scanner.current());
        }
        return Ok(Some(Attribute {
            name: Some(name),
            value,
        }));
    }
    Ok(None)
}

fn repeater(scanner: &mut Scanner<'_>) -> Option<Repeater> {
    match scanner.peek() {
        Some(Token::Repeater(repeat)) => {
            let repeat = *repeat;
            scanner.pos += 1;
            Some(repeat)
        }
        _ => None,
    }
}

fn quoted(scanner: &mut Scanner<'_>) -> Result<bool, EmmetError> {
    let start = scanner.pos;
    let Some(&Token::Quote { single }) = scanner.peek() else {
        return Ok(false);
    };
    scanner.pos += 1;
    while scanner.readable() {
        if matches!(scanner.next(), Some(&Token::Quote { single: other }) if other == single) {
            scanner.start = start;
            return Ok(true);
        }
    }
    Err(EmmetError)
}

/// An unquoted value; with `allow_brackets`, balanced brackets belong to it.
fn literal(scanner: &mut Scanner<'_>, allow_brackets: bool) -> bool {
    let start = scanner.pos;
    let mut attribute = 0i64;
    let mut expression = 0i64;
    let mut group = 0i64;
    while let Some(token) = scanner.peek() {
        if expression != 0 {
            // Everything inside an expression belongs to it.
            if let Token::Bracket {
                open,
                context: BracketContext::Expression,
            } = token
            {
                expression += if *open { 1 } else { -1 };
            }
        } else if matches!(
            token,
            Token::Quote { .. } | Token::Operator(_) | Token::WhiteSpace(_) | Token::Repeater(_)
        ) {
            break;
        } else if let Token::Bracket { open, context } = token {
            if !allow_brackets {
                break;
            }
            let count = match context {
                BracketContext::Attribute => &mut attribute,
                BracketContext::Expression => &mut expression,
                BracketContext::Group => &mut group,
            };
            if *open {
                *count += 1;
            } else if *count == 0 {
                // An unmatched closing bracket belongs to the caller.
                break;
            } else {
                *count -= 1;
            }
        }
        scanner.pos += 1;
    }
    if start != scanner.pos {
        scanner.start = start;
        return true;
    }
    false
}

fn element_name(scanner: &mut Scanner<'_>) -> bool {
    let start = scanner.pos;
    while scanner.consume(|token| {
        matches!(
            token,
            Token::Literal(_) | Token::RepeaterNumber { .. } | Token::RepeaterPlaceholder
        )
    }) {}
    if scanner.pos != start {
        scanner.start = start;
        return true;
    }
    false
}

fn text(scanner: &mut Scanner<'_>) -> bool {
    let start = scanner.pos;
    if !scanner.consume(|token| is_bracket(token, BracketContext::Expression, Some(true))) {
        return false;
    }
    let mut brackets = 0u32;
    while scanner.readable() {
        if let Some(&Token::Bracket {
            open,
            context: BracketContext::Expression,
        }) = scanner.next()
        {
            if open {
                brackets += 1;
            } else if brackets == 0 {
                break;
            } else {
                brackets -= 1;
            }
        }
    }
    scanner.start = start;
    true
}

/// The text's tokens, less its own braces.
fn text_value(scanner: &Scanner<'_>) -> Vec<Token> {
    let mut from = scanner.start;
    let mut to = scanner.pos.min(scanner.tokens.len());
    if scanner
        .tokens
        .get(from)
        .is_some_and(|token| is_bracket(token, BracketContext::Expression, Some(true)))
    {
        from += 1;
    }
    if to > 0
        && scanner
            .tokens
            .get(to - 1)
            .is_some_and(|token| is_bracket(token, BracketContext::Expression, Some(false)))
    {
        to -= 1;
    }
    if from >= to {
        return Vec::new();
    }
    scanner.slice(from, to)
}

pub(super) fn is_bracket(token: &Token, context: BracketContext, open: Option<bool>) -> bool {
    matches!(token, Token::Bracket { open: is_open, context: c }
        if *c == context && open.is_none_or(|open| open == *is_open))
}

fn is_operator(token: &Token, operator: Operator) -> bool {
    matches!(token, Token::Operator(op) if *op == operator)
}
