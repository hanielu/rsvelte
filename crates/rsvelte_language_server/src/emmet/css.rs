//! Stylesheet abbreviations, resolved and written out: emmet 2.4.11's
//! `stylesheet` (`parse`, `resolveNode` and `css`), with emmet-helper's
//! options and without a context.

use std::fmt::Write as _;
use std::sync::OnceLock;

use super::stylesheet::{self, Color, CssValue, Kind, Property, Token};
use super::{Data, EmmetError};

/// The CSS snippets: a raw text snippet, or a property and the values it
/// may take (`|`-separated).
pub(super) enum Snippet {
    Raw {
        key: String,
        value: String,
    },
    Property {
        key: String,
        property: String,
        value: Vec<Vec<CssValue>>,
        keywords: Keywords,
        /// The more specific properties nested in this shorthand, as indices
        /// into the registry.
        dependencies: Vec<usize>,
    },
}

impl Snippet {
    fn key(&self) -> &str {
        match self {
            Self::Raw { key, .. } | Self::Property { key, .. } => key,
        }
    }
}

/// A JavaScript object of keywords: its keys enumerate array indices first,
/// in numeric order, then the rest in insertion order.
#[derive(Default)]
pub(super) struct Keywords(Vec<(String, Token)>);

impl Keywords {
    fn insert(&mut self, key: String, token: Token) {
        if let Some(entry) = self.0.iter_mut().find(|(name, _)| *name == key) {
            entry.1 = token;
        } else {
            self.0.push((key, token));
        }
    }

    fn keys(&self) -> Vec<&str> {
        let mut indices: Vec<(u64, &str)> = self
            .0
            .iter()
            .filter_map(|(key, _)| array_index(key).map(|index| (index, key.as_str())))
            .collect();
        indices.sort_unstable();
        indices
            .into_iter()
            .map(|(_, key)| key)
            .chain(
                self.0
                    .iter()
                    .filter(|(key, _)| array_index(key).is_none())
                    .map(|(key, _)| key.as_str()),
            )
            .collect()
    }

    fn get(&self, key: &str) -> Option<&Token> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, token)| token)
    }
}

/// A canonical array index: digits without a leading zero, below 2³² − 1.
fn array_index(key: &str) -> Option<u64> {
    if key.is_empty()
        || !key.bytes().all(|b| b.is_ascii_digit())
        || key.len() > 1 && key.starts_with('0')
    {
        return None;
    }
    key.parse()
        .ok()
        .filter(|&index| index < u64::from(u32::MAX))
}

/// `convertSnippets`: the registry, sorted by key, with the shorthands
/// nested.
pub(super) fn snippets(raw: &[(String, String)]) -> Vec<Snippet> {
    static PROPERTY: OnceLock<regex::Regex> = OnceLock::new();
    let property = PROPERTY.get_or_init(|| {
        regex::Regex::new(r"^([a-z-]+)(?:\s*:\s*([^\n\r;]+?);*)?$").expect("valid regex")
    });
    let mut snippets: Vec<Snippet> = raw
        .iter()
        .map(|(key, value)| {
            let Some(captures) = property.captures(value) else {
                return Snippet::Raw {
                    key: key.clone(),
                    value: value.clone(),
                };
            };
            let parsed: Vec<Vec<CssValue>> = captures
                .get(2)
                .map(|values| values.as_str().split('|').map(parse_value).collect())
                .unwrap_or_default();
            let mut keywords = Keywords::default();
            for value in parsed.iter().flatten() {
                collect_keywords(value, &mut keywords);
            }
            Snippet::Property {
                key: key.clone(),
                property: captures[1].to_string(),
                value: parsed,
                keywords,
                dependencies: Vec::new(),
            }
        })
        .collect();
    snippets.sort_by(|a, b| a.key().cmp(b.key()));
    // Each shorthand holds its more specific properties: `background` →
    // `background-position` → `background-position-x`.
    let mut stack: Vec<usize> = Vec::new();
    for current in 0..snippets.len() {
        let Snippet::Property { property, .. } = &snippets[current] else {
            continue;
        };
        let property = property.clone();
        while let Some(&previous) = stack.last() {
            let Snippet::Property {
                property: shorthand,
                dependencies,
                ..
            } = &mut snippets[previous]
            else {
                unreachable!("the stack holds properties");
            };
            if property.starts_with(shorthand.as_str())
                && property.as_bytes().get(shorthand.len()) == Some(&b'-')
            {
                dependencies.push(current);
                stack.push(current);
                break;
            }
            stack.pop();
        }
        if stack.is_empty() {
            stack.push(current);
        }
    }
    snippets
}

/// A snippet's value alternative, parsed as a property value.
fn parse_value(value: &str) -> Vec<CssValue> {
    stylesheet::parse(value.trim(), true)
        .ok()
        .and_then(|properties| properties.into_iter().next())
        .map(|property| property.value)
        .unwrap_or_default()
}

fn collect_keywords(value: &CssValue, keywords: &mut Keywords) {
    for token in value {
        match &token.kind {
            Kind::Literal(literal) => keywords.insert(literal.clone(), token.clone()),
            Kind::FunctionCall { name, .. } => keywords.insert(name.clone(), token.clone()),
            Kind::Field { name, .. } => {
                // A field's placeholder is a keyword too.
                let name = name.trim();
                if !name.is_empty() {
                    keywords.insert(name.to_string(), Token::literal(name));
                }
            }
            _ => {}
        }
    }
}

/// emmet-helper's options for a stylesheet syntax.
#[derive(Clone, Copy)]
pub(super) struct Options {
    /// `stylesheet.after`: `;`, or nothing for `sass`.
    pub after: &'static str,
}

pub(super) const BETWEEN: &str = ": ";
const MIN_SCORE: f64 = 0.3;

/// Expand a stylesheet abbreviation as emmet's `expand` with emmet-helper's
/// options.
pub(super) fn expand(
    abbreviation: &str,
    options: Options,
    data: &Data,
) -> Result<String, EmmetError> {
    let nodes = stylesheet::parse(abbreviation, false)?;
    let mut out = String::new();
    for (index, node) in nodes.into_iter().enumerate() {
        let node = resolve_node(node, data);
        if index != 0 {
            out.push('\n');
        }
        property(&node, &mut out, options);
    }
    Ok(out)
}

/// `resolveNode`: the snippet the name matches best, and the keywords and
/// units of its value.
fn resolve_node(mut node: Property, data: &Data) -> Property {
    if !resolve_gradient(&mut node)
        && let Some(name) = node.name.clone()
        && let Some(index) = find_best_match(
            &name,
            data.stylesheet.snippets.iter().map(Snippet::key),
            MIN_SCORE,
            true,
        )
    {
        let snippet = &data.stylesheet.snippets[index];
        let resolved = match snippet {
            Snippet::Property { .. } => resolve_as_property(node.clone(), snippet, data),
            Snippet::Raw { value, .. } => Some(resolve_as_snippet(node.clone(), value)),
        };
        if let Some(resolved) = resolved {
            node = resolved;
        }
    }
    if node.name.is_some() {
        resolve_numeric_value(&mut node, data);
    }
    node
}

/// `lg(…)`, or a property named `lg`, is a `background-image` gradient.
fn resolve_gradient(node: &mut Property) -> bool {
    let function = match node.value.as_slice() {
        [value] => match value.as_slice() {
            [
                token @ Token {
                    kind: Kind::FunctionCall { name, .. },
                    ..
                },
            ] if name == "lg" => Some(token.clone()),
            _ => None,
        },
        _ => None,
    };
    if function.is_none() && node.name.as_deref() != Some("lg") {
        return false;
    }
    let function = match function {
        Some(Token {
            kind: Kind::FunctionCall { arguments, .. },
            start,
            end,
        }) => Token {
            kind: Kind::FunctionCall {
                name: "linear-gradient".to_string(),
                arguments,
            },
            start,
            end,
        },
        _ => Token::new(Kind::FunctionCall {
            name: "linear-gradient".to_string(),
            arguments: vec![vec![Token::field(0, "")]],
        }),
    };
    node.name = Some("background-image".to_string());
    node.value = vec![vec![function]];
    true
}

fn resolve_as_property(mut node: Property, snippet: &Snippet, data: &Data) -> Option<Property> {
    let Snippet::Property {
        key,
        property,
        value,
        ..
    } = snippet
    else {
        return None;
    };
    // In `dib`, `d` matches `display` and `ib` is a keyword of its value; a
    // part that resolves to no keyword, or beside an explicit value, means
    // the snippet does not match.
    let abbreviation = node.name.clone().unwrap_or_default();
    let inline_value = unmatched_part(&abbreviation, key);
    if !inline_value.is_empty() {
        if !node.value.is_empty() {
            return None;
        }
        node.value.push(vec![resolve_keyword(
            inline_value,
            Some(snippet),
            0.0,
            data,
        )?]);
    }
    node.name = Some(property.clone());
    if !node.value.is_empty() {
        resolve_value_keywords(&mut node, Some(snippet), data);
    } else if let Some(default) = value.first() {
        // The value is selected only when there is a choice
        // (emmetio/emmet#558).
        node.value = if value.len() == 1 || default.iter().any(has_field) {
            default.clone()
        } else {
            default
                .iter()
                .map(|value| wrap_with_field(value, &mut 1))
                .collect()
        };
    }
    Some(node)
}

fn resolve_value_keywords(node: &mut Property, snippet: Option<&Snippet>, data: &Data) {
    for value in &mut node.value {
        *value = value
            .iter()
            .map(|token| match &token.kind {
                Kind::Literal(literal) => {
                    resolve_keyword(literal, snippet, 0.0, data).unwrap_or_else(|| token.clone())
                }
                Kind::FunctionCall { name, arguments } => {
                    // A matching function call keeps the arguments given and
                    // the rest of its own.
                    match resolve_keyword(name, snippet, 0.0, data) {
                        Some(Token {
                            kind:
                                Kind::FunctionCall {
                                    name: matched,
                                    arguments: defaults,
                                },
                            start,
                            end,
                        }) => Token {
                            kind: Kind::FunctionCall {
                                name: matched,
                                arguments: arguments
                                    .iter()
                                    .chain(defaults.iter().skip(arguments.len()))
                                    .cloned()
                                    .collect(),
                            },
                            start,
                            end,
                        },
                        _ => token.clone(),
                    }
                }
                _ => token.clone(),
            })
            .collect();
    }
}

/// A raw snippet's text, its fields filled with the abbreviation's values
/// in order.
fn resolve_as_snippet(mut node: Property, snippet: &str) -> Property {
    static FIELD: OnceLock<regex::Regex> = OnceLock::new();
    let field =
        FIELD.get_or_init(|| regex::Regex::new(r"\$\{(\d+)(:[^}]+)?\}").expect("valid regex"));
    let mut input: std::collections::VecDeque<Token> =
        node.value.first().cloned().unwrap_or_default().into();
    let mut output = Vec::new();
    let mut offset = 0;
    for captures in field.captures_iter(snippet) {
        let whole = captures.get(0).expect("a match");
        if offset != whole.start() {
            output.push(Token::literal(&snippet[offset..whole.start()]));
        }
        offset = whole.end();
        if let Some(token) = input.pop_front() {
            output.push(token);
        } else {
            output.push(Token::new(Kind::Field {
                index: captures[1].parse().ok(),
                name: captures
                    .get(2)
                    .map_or("", |name| &name.as_str()[1..])
                    .to_string(),
            }));
        }
    }
    if offset < snippet.len() {
        output.push(Token::literal(&snippet[offset..]));
    }
    node.name = None;
    node.value = vec![output];
    node
}

/// `findBestMatch`: the index of the item `abbreviation` matches best, the
/// last of equals.
fn find_best_match<'a>(
    abbreviation: &str,
    items: impl IntoIterator<Item = &'a str>,
    min_score: f64,
    partial_match: bool,
) -> Option<usize> {
    let mut matched = None;
    let mut max_score = 0.0;
    for (index, item) in items.into_iter().enumerate() {
        let score = score_match(abbreviation, item, partial_match);
        if score == 1.0 {
            return Some(index);
        }
        if score != 0.0 && score >= max_score {
            max_score = score;
            matched = Some(index);
        }
    }
    (max_score >= min_score).then_some(matched).flatten()
}

/// `scoreMatch`: both strings start alike; characters nearer the start of
/// `str2`, or right after one of its dashes, score higher.
fn score_match(str1: &str, str2: &str, partial_match: bool) -> f64 {
    let str1: Vec<u16> = str1.to_lowercase().encode_utf16().collect();
    let str2: Vec<u16> = str2.to_lowercase().encode_utf16().collect();
    if str1 == str2 {
        return 1.0;
    }
    if str1.is_empty() || str2.is_empty() || str1[0] != str2[0] {
        return 0.0;
    }
    let (len1, len2) = (str1.len(), str2.len());
    if !partial_match && len1 > len2 {
        return 0.0;
    }
    let min_length = len1.min(len2);
    let max_length = len1.max(len2);
    let mut i = 1;
    let mut j = 1;
    let mut score = max_length;
    while i < len1 {
        let ch1 = str1[i];
        let mut found = false;
        let mut acronym = false;
        while j < len2 {
            let ch2 = str2[j];
            if ch1 == ch2 {
                found = true;
                score += max_length - if acronym { i } else { j };
                break;
            }
            // A bonus for a match right after an unmatched `-`.
            acronym = ch2 == u16::from(b'-');
            j += 1;
        }
        if !found {
            if !partial_match {
                return 0.0;
            }
            break;
        }
        i += 1;
    }
    let sum = |n: usize| (n * (n + 1)) as f64 / 2.0;
    let match_ratio = i as f64 / max_length as f64;
    let delta = max_length - min_length;
    let max_score = sum(max_length) - sum(delta);
    (score as f64 * match_ratio) / max_score
}

/// `getUnmatchedPart`: the abbreviation from its first character `str`
/// does not have in order.
fn unmatched_part<'a>(abbreviation: &'a str, str: &str) -> &'a str {
    let mut last = 0;
    for (index, ch) in abbreviation.char_indices() {
        match str.get(last..).and_then(|rest| rest.find(ch)) {
            Some(found) => last += found + ch.len_utf8(),
            None => return &abbreviation[index..],
        }
    }
    ""
}

/// A keyword of the snippet, of its nested properties, or a global one.
fn resolve_keyword(
    keyword: &str,
    snippet: Option<&Snippet>,
    min_score: f64,
    data: &Data,
) -> Option<Token> {
    let snippets = &data.stylesheet.snippets;
    if let Some(Snippet::Property {
        keywords,
        dependencies,
        ..
    }) = snippet
    {
        for keywords in
            std::iter::once(keywords).chain(dependencies.iter().filter_map(
                |&index| match &snippets[index] {
                    Snippet::Property { keywords, .. } => Some(keywords),
                    Snippet::Raw { .. } => None,
                },
            ))
        {
            let keys = keywords.keys();
            if let Some(index) = find_best_match(keyword, keys.iter().copied(), min_score, false) {
                return keywords.get(keys[index]).cloned();
            }
        }
    }
    let global = &data.stylesheet.keywords;
    find_best_match(keyword, global.iter().map(String::as_str), min_score, false)
        .map(|index| Token::literal(&global[index]))
}

/// `resolveNumericValue`: unit aliases, and a unit for a bare number but
/// zero.
fn resolve_numeric_value(node: &mut Property, data: &Data) {
    let unitless = node
        .name
        .as_ref()
        .is_some_and(|name| data.stylesheet.unitless.contains(name));
    for token in node.value.iter_mut().flatten() {
        if let Kind::Number { value, raw, unit } = &mut token.kind {
            if !unit.is_empty() {
                if let Some(alias) = unit_alias(unit) {
                    *unit = alias.to_string();
                }
            } else if *value != 0.0 && !unitless {
                *unit = if raw.contains('.') { "em" } else { "px" }.to_string();
            }
        }
    }
}

/// emmet-helper's `stylesheet.unitAliases`.
fn unit_alias(unit: &str) -> Option<&'static str> {
    match unit {
        "e" => Some("em"),
        "p" => Some("%"),
        "x" => Some("ex"),
        "r" => Some("rem"),
        _ => None,
    }
}

fn has_field(value: &CssValue) -> bool {
    value.iter().any(|token| match &token.kind {
        Kind::Field { .. } => true,
        Kind::FunctionCall { arguments, .. } => arguments.iter().any(has_field),
        _ => false,
    })
}

/// Each token of a default value as a numbered field, so it is selected.
fn wrap_with_field(value: &CssValue, index: &mut usize) -> CssValue {
    fn field(wrapped: &mut Vec<Token>, index: &mut usize, placeholder: String) {
        wrapped.push(Token::field(*index, placeholder));
        *index += 1;
    }
    let mut wrapped = Vec::new();
    for token in value {
        match &token.kind {
            Kind::Color(color) => field(&mut wrapped, index, color_text(*color)),
            Kind::Literal(literal) => field(&mut wrapped, index, literal.clone()),
            Kind::Number { value, unit, .. } => field(
                &mut wrapped,
                index,
                format!("{}{unit}", js_number_to_string(*value)),
            ),
            Kind::String { value, single } => {
                let quote = if *single { '\'' } else { '"' };
                field(&mut wrapped, index, format!("{quote}{value}{quote}"));
            }
            Kind::FunctionCall { name, arguments } => {
                field(&mut wrapped, index, name.clone());
                wrapped.push(Token::literal("("));
                for (position, argument) in arguments.iter().enumerate() {
                    wrapped.extend(wrap_with_field(argument, index));
                    if position + 1 != arguments.len() {
                        wrapped.push(Token::literal(", "));
                    }
                }
                wrapped.push(Token::literal(")"));
            }
            _ => wrapped.push(token.clone()),
        }
    }
    wrapped
}

/// `property`: `name: value;`, or a raw snippet's tokens.
fn property(node: &Property, out: &mut String, options: Options) {
    if let Some(name) = &node.name {
        push_string(out, &format!("{name}{BETWEEN}"));
        if node.value.is_empty() {
            push_field(out, Some(0), "");
        } else {
            for (index, value) in node.value.iter().enumerate() {
                if index != 0 {
                    out.push_str(", ");
                }
                output_value(value, out);
            }
        }
        output_important(node, out, true);
        out.push_str(options.after);
    } else {
        for token in node.value.iter().flatten() {
            output_token(token, out);
        }
        output_important(node, out, !node.value.is_empty());
    }
}

fn output_important(node: &Property, out: &mut String, separator: bool) {
    if node.important {
        if separator {
            out.push(' ');
        }
        out.push_str("!important");
    }
}

/// A value's tokens, space-separated — but for a field written right after
/// the token before it (`foo${bar}`).
fn output_value(value: &CssValue, out: &mut String) {
    let mut previous_end = None;
    for (index, token) in value.iter().enumerate() {
        let adjacent_field =
            matches!(token.kind, Kind::Field { .. }) && token.start == previous_end;
        if index != 0 && !adjacent_field {
            out.push(' ');
        }
        output_token(token, out);
        previous_end = token.end;
    }
}

fn output_token(token: &Token, out: &mut String) {
    match &token.kind {
        Kind::Color(color) => out.push_str(&color_text(*color)),
        Kind::Literal(value) | Kind::CustomProperty(value) => push_string(out, value),
        Kind::Number { value, unit, .. } => push_string(out, &format!("{}{unit}", frac(*value, 4))),
        Kind::String { value, single } => {
            let quote = if *single { '\'' } else { '"' };
            push_string(out, &format!("{quote}{value}{quote}"));
        }
        Kind::Field { index, name } => push_field(out, *index, name),
        Kind::FunctionCall { name, arguments } => {
            out.push_str(name);
            out.push('(');
            for (index, argument) in arguments.iter().enumerate() {
                if index != 0 {
                    out.push_str(", ");
                }
                output_value(argument, out);
            }
            out.push(')');
        }
        Kind::Bracket { .. } | Kind::Operator(_) | Kind::WhiteSpace => {}
    }
}

/// `pushString`: line by line, each new line at the output's (zero)
/// indentation.
fn push_string(out: &mut String, value: &str) {
    out.push_str(&value.replace("\r\n", "\n").replace('\r', "\n"));
}

/// emmet-helper's `emmetSnippetField`: `${1}` or `${1:placeholder}`.
fn push_field(out: &mut String, index: Option<usize>, placeholder: &str) {
    let index = index.map_or_else(|| "undefined".to_string(), |index| index.to_string());
    if placeholder.is_empty() {
        let _ = write!(out, "${{{index}}}");
    } else {
        let _ = write!(out, "${{{index}:{placeholder}}}");
    }
}

/// `color`: `transparent`, a (short) hex colour, or `rgba(…)`.
fn color_text(color: Color) -> String {
    if color.r == 0 && color.g == 0 && color.b == 0 && color.a == 0.0 {
        return "transparent".to_string();
    }
    if color.a == 1.0 {
        let short = [color.r, color.g, color.b]
            .iter()
            .all(|channel| channel % 17 == 0);
        let hex = |channel: u32| {
            if short {
                format!("{:x}", channel >> 4)
            } else {
                format!("{channel:02x}")
            }
        };
        return format!("#{}{}{}", hex(color.r), hex(color.g), hex(color.b));
    }
    format!(
        "rgba({}, {}, {}, {})",
        color.r,
        color.g,
        color.b,
        frac(color.a, 8)
    )
}

/// `frac`: `num.toFixed(digits)` without trailing zeros (`/\.?0+$/`).
fn frac(value: f64, digits: u32) -> String {
    let fixed = js_to_fixed(value, digits);
    let zeros = fixed.len() - fixed.trim_end_matches('0').len();
    if zeros == 0 {
        return fixed;
    }
    let end = fixed.len() - zeros;
    let end = if fixed[..end].ends_with('.') {
        end - 1
    } else {
        end
    };
    fixed[..end].to_string()
}

/// JavaScript's `Number.prototype.toFixed`: the nearest of the two `n /
/// 10^digits` around the value, the larger (away from zero) on a tie; from
/// 10²¹ on, the number's own string.
fn js_to_fixed(value: f64, digits: u32) -> String {
    if !value.is_finite() || value.abs() >= 1e21 {
        return js_number_to_string(value);
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let magnitude = value.abs();
    // A tie is a value that is exactly `m / (2 · 10^digits)`, `m` odd: as a
    // binary fraction, an odd multiple of `2^-(digits + 1)`.
    let scaled = magnitude * f64::from(2u32.pow(digits + 1));
    let text = if scaled.fract() == 0.0 && scaled % 2.0 == 1.0 && scaled < 2f64.powi(64) {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a whole number below 2^64"
        )]
        let odd = scaled as u128;
        let n = (odd * 5u128.pow(digits)).div_ceil(2);
        let n = n.to_string();
        let digits = digits as usize;
        let n = format!("{n:0>width$}", width = digits + 1);
        format!("{}.{}", &n[..n.len() - digits], &n[n.len() - digits..])
    } else {
        format!("{magnitude:.prec$}", prec = digits as usize)
    };
    format!("{sign}{text}")
}

/// JavaScript's `Number.prototype.toString`: the shortest digits, plain from
/// 10⁻⁶ to below 10²¹, otherwise as `1.5e+21`.
pub(super) fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let magnitude = value.abs();
    if (1e-6..1e21).contains(&magnitude) {
        return format!("{value}");
    }
    let exponential = format!("{value:e}");
    let (mantissa, exponent) = exponential.split_once('e').expect("an exponent");
    if exponent.starts_with('-') {
        format!("{mantissa}e{exponent}")
    } else {
        format!("{mantissa}e+{exponent}")
    }
}
