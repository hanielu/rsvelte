//! Finding the abbreviation before the cursor: `extractAbbreviation` in
//! emmet 2.4.11 (`@emmetio/extract-abbreviation`), for markup, looking past
//! the brackets and quote an editor closes automatically. Works on UTF-16
//! code units, as JavaScript's strings do.

const SQUARE_L: u16 = b'[' as u16;
const SQUARE_R: u16 = b']' as u16;
const ROUND_L: u16 = b'(' as u16;
const ROUND_R: u16 = b')' as u16;
const CURLY_L: u16 = b'{' as u16;
const CURLY_R: u16 = b'}' as u16;
const SINGLE_QUOTE: u16 = b'\'' as u16;
const DOUBLE_QUOTE: u16 = b'"' as u16;
const ESCAPE: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
const EQUALS: u16 = b'=' as u16;
const ANGLE_L: u16 = b'<' as u16;
const ANGLE_R: u16 = b'>' as u16;

pub(super) struct Extracted {
    pub abbreviation: Vec<u16>,
    /// Where the abbreviation starts in the line.
    pub location: usize,
}

/// The longest abbreviation looked for. emmet has no limit, and its
/// backward scan is quadratic on a long minified line; an abbreviation that
/// would reach further back is not offered.
const MAX_LENGTH: usize = 2000;

pub(super) fn extract(line: &[u16], pos: usize) -> Option<Extracted> {
    let mut pos = pos.min(line.len());
    // A closing quote may only come right after the cursor; brackets follow.
    if line.get(pos).copied().is_some_and(is_quote) {
        pos += 1;
    }
    while line.get(pos).copied().is_some_and(is_close_brace) {
        pos += 1;
    }
    let start = pos.saturating_sub(MAX_LENGTH);
    let mut scanner = Backward {
        text: line,
        start,
        pos,
    };
    let mut stack: Vec<u16> = Vec::new();
    while !scanner.sol() {
        let ch = scanner.peek();
        if stack.contains(&CURLY_R) {
            if ch == CURLY_R {
                stack.push(ch);
                scanner.pos -= 1;
                continue;
            }
            if ch != CURLY_L {
                scanner.pos -= 1;
                continue;
            }
        }
        if is_close_brace(ch) {
            stack.push(ch);
        } else if is_open_brace(ch) {
            if stack.pop() != Some(brace_pair(ch)) {
                // An unexpected brace.
                break;
            }
        } else if stack.contains(&SQUARE_R) || stack.contains(&CURLY_R) {
            // Everything inside an attribute set or text belongs to it.
            scanner.pos -= 1;
            continue;
        } else if is_html(&mut scanner) || !is_abbreviation(ch) {
            break;
        }
        scanner.pos -= 1;
    }
    if !stack.is_empty() || scanner.pos == pos || (start > 0 && scanner.pos == start) {
        return None;
    }
    let mut abbreviation = &line[scanner.pos..pos];
    while let Some((&first, rest)) = abbreviation.split_first()
        && matches!(first, 0x2a | 0x2b | 0x3e | 0x5e)
    {
        // A leading `*`, `+`, `>` or `^` cannot start one.
        abbreviation = rest;
    }
    Some(Extracted {
        abbreviation: abbreviation.to_vec(),
        location: pos - abbreviation.len(),
    })
}

/// emmet's backward scanner: `pos` is just past the character it looks at,
/// and `start` the line's start as far as the scan is concerned.
struct Backward<'a> {
    text: &'a [u16],
    start: usize,
    pos: usize,
}

impl Backward<'_> {
    const fn sol(&self) -> bool {
        self.pos == self.start
    }

    /// The character before `pos`; `u16::MAX` stands for JavaScript's `NaN`.
    fn peek(&self) -> u16 {
        self.pos
            .checked_sub(1)
            .and_then(|index| self.text.get(index))
            .copied()
            .unwrap_or(u16::MAX)
    }

    fn previous(&mut self) -> Option<u16> {
        if self.sol() {
            return None;
        }
        self.pos -= 1;
        Some(self.text[self.pos])
    }

    fn consume(&mut self, test: impl Fn(u16) -> bool) -> bool {
        if self.sol() {
            return false;
        }
        let ok = test(self.peek());
        if ok {
            self.pos -= 1;
        }
        ok
    }

    fn consume_char(&mut self, ch: u16) -> bool {
        self.consume(|c| c == ch)
    }

    fn consume_while(&mut self, test: impl Fn(u16) -> bool) -> bool {
        let start = self.pos;
        while self.consume(&test) {}
        self.pos < start
    }

    fn consume_quoted(&mut self) -> bool {
        let start = self.pos;
        if let Some(quote) = self.previous().filter(|&c| is_quote(c)) {
            while !self.sol() {
                if self.previous() == Some(quote) && self.peek() != ESCAPE {
                    return true;
                }
            }
        }
        self.pos = start;
        false
    }
}

/// Whether the characters before the scanner end an HTML tag.
fn is_html(scanner: &mut Backward<'_>) -> bool {
    let start = scanner.pos;
    if !scanner.consume_char(ANGLE_R) {
        return false;
    }
    let mut ok = false;
    // Possibly a self-closed element.
    scanner.consume_char(SLASH);
    while !scanner.sol() {
        scanner.consume_while(is_white_space);
        if scanner.consume_while(is_ident) {
            // A tag name, a boolean attribute or an unquoted value.
            if scanner.consume_char(SLASH) {
                // A closing tag, or an invalid one.
                ok = scanner.consume_char(ANGLE_L);
                break;
            } else if scanner.consume_char(ANGLE_L) {
                ok = true;
                break;
            } else if scanner.consume(is_white_space) {
                continue;
            } else if scanner.consume_char(EQUALS) {
                if scanner.consume_while(is_ident) {
                    continue;
                }
                break;
            } else if consume_attribute_with_unquoted_value(scanner) {
                ok = true;
                break;
            }
            break;
        }
        if consume_attribute(scanner) {
            continue;
        }
        break;
    }
    scanner.pos = start;
    ok
}

fn consume_attribute(scanner: &mut Backward<'_>) -> bool {
    consume_attribute_with_quoted_value(scanner) || consume_attribute_with_unquoted_value(scanner)
}

fn consume_attribute_with_quoted_value(scanner: &mut Backward<'_>) -> bool {
    let start = scanner.pos;
    if scanner.consume_quoted() && scanner.consume_char(EQUALS) && scanner.consume_while(is_ident) {
        return true;
    }
    scanner.pos = start;
    false
}

fn consume_attribute_with_unquoted_value(scanner: &mut Backward<'_>) -> bool {
    let start = scanner.pos;
    let mut stack = Vec::new();
    while !scanner.sol() {
        let ch = scanner.peek();
        if is_close_brace(ch) {
            stack.push(ch);
        } else if is_open_brace(ch) {
            if stack.pop() != Some(brace_pair(ch)) {
                break;
            }
        } else if !is_unquoted_value(ch) {
            break;
        }
        scanner.pos -= 1;
    }
    if start != scanner.pos && scanner.consume_char(EQUALS) && scanner.consume_while(is_ident) {
        return true;
    }
    scanner.pos = start;
    false
}

const fn is_quote(ch: u16) -> bool {
    ch == SINGLE_QUOTE || ch == DOUBLE_QUOTE
}

const fn is_ident(ch: u16) -> bool {
    ch == b':' as u16 || ch == b'-' as u16 || is_alpha(ch) || is_number(ch)
}

/// emmet's `ch &= ~32` letter test.
const fn is_alpha(ch: u16) -> bool {
    let upper = ch & !32;
    upper >= b'A' as u16 && upper <= b'Z' as u16
}

const fn is_number(ch: u16) -> bool {
    ch >= b'0' as u16 && ch <= b'9' as u16
}

const fn is_white_space(ch: u16) -> bool {
    ch == b' ' as u16 || ch == b'\t' as u16
}

const fn is_unquoted_value(ch: u16) -> bool {
    ch != u16::MAX && ch != EQUALS && !is_white_space(ch) && !is_quote(ch)
}

const fn is_open_brace(ch: u16) -> bool {
    ch == ROUND_L || ch == SQUARE_L || ch == CURLY_L
}

const fn is_close_brace(ch: u16) -> bool {
    ch == ROUND_R || ch == SQUARE_R || ch == CURLY_R
}

const fn brace_pair(open: u16) -> u16 {
    match open {
        SQUARE_L => SQUARE_R,
        ROUND_L => ROUND_R,
        _ => CURLY_R,
    }
}

fn is_abbreviation(ch: u16) -> bool {
    char::from_u32(u32::from(ch))
        .is_some_and(|c| c.is_ascii_alphanumeric() || "#.*:$-_!@%^+>/".contains(c))
}
