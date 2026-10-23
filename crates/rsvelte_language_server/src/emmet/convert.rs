//! Token tree → node tree: `convert` in emmet 2.4.11
//! (`@emmetio/abbreviation`, `convert.ts`), for an expansion with no wrapped
//! text, which is all a completion ever expands.

use std::collections::HashMap;

use super::EmmetError;
use super::parser::{self, Element, Group, Statement};
use super::tokenizer::{self, BracketContext, Repeater, Token};

/// The most elements repeaters may produce in one abbreviation
/// (`maxRepeat` in emmet-helper's expand options).
const MAX_REPEAT: i64 = 1000;

/// A converted element, text or snippet. `None` and an empty list differ as
/// in JavaScript: `Some(vec![])` is still a value or an attribute list.
#[derive(Clone, Debug, Default)]
pub(super) struct Node {
    pub name: Option<String>,
    pub value: Option<Vec<Value>>,
    pub attributes: Option<Vec<Attribute>>,
    pub children: Vec<Node>,
    pub repeat: Option<Repeater>,
    pub self_closing: bool,
    /// The BEM filter's per-node cache (`node._bem`).
    pub bem: Option<super::transform::BemData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Value {
    Text(String),
    Field { index: usize, name: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ValueType {
    Raw,
    SingleQuote,
    DoubleQuote,
    Expression,
}

#[derive(Clone, Debug)]
pub(super) struct Attribute {
    pub name: Option<String>,
    pub value: Option<Vec<Value>>,
    pub boolean: bool,
    pub implied: bool,
    pub value_type: ValueType,
}

struct State<'a> {
    inserted: bool,
    repeaters: Vec<Repeater>,
    repeat_guard: i64,
    variables: &'a HashMap<String, String>,
}

/// `parseAbbreviation`: tokenize, parse and convert.
pub(super) fn parse_abbreviation(
    abbreviation: &str,
    variables: &HashMap<String, String>,
) -> Result<Vec<Node>, EmmetError> {
    let tokens = tokenizer::tokenize(abbreviation)?;
    let tree = parser::parse(&tokens)?;
    let mut state = State {
        inserted: false,
        repeaters: Vec::new(),
        repeat_guard: MAX_REPEAT,
        variables,
    };
    convert_group(&tree, None, &mut state)
}

fn convert_statement(node: &Statement, state: &mut State<'_>) -> Result<Vec<Node>, EmmetError> {
    let own = match node {
        Statement::Element(element) => element.repeat,
        Statement::Group(group) => group.repeat,
    };
    let Some(original) = own else {
        return convert_one(node, None, state);
    };
    let mut repeat = original;
    repeat.count = repeat.count.max(1);
    let mut result = Vec::new();
    state.repeaters.push(repeat);
    for value in 0..repeat.count {
        repeat.value = value;
        if let Some(last) = state.repeaters.last_mut() {
            *last = repeat;
        }
        let mut items = convert_one(node, Some(repeat), state)?;
        if repeat.implicit && !state.inserted {
            // An implicit repeater with no `$#` takes the (empty) text into
            // its deepest node.
            if let Some(target) = items.last_mut() {
                insert_text(deepest_node(target), "");
            }
        }
        result.extend(items);
        // At least one item is output even past the limit.
        state.repeat_guard -= 1;
        if state.repeat_guard <= 0 {
            break;
        }
    }
    state.repeaters.pop();
    if repeat.implicit {
        state.inserted = true;
    }
    Ok(result)
}

fn convert_one(
    node: &Statement,
    repeat: Option<Repeater>,
    state: &mut State<'_>,
) -> Result<Vec<Node>, EmmetError> {
    match node {
        Statement::Element(element) => convert_element(element, repeat, state),
        Statement::Group(group) => convert_group(group, repeat, state),
    }
}

fn convert_element(
    node: &Element,
    repeat: Option<Repeater>,
    state: &mut State<'_>,
) -> Result<Vec<Node>, EmmetError> {
    let name = match &node.name {
        Some(tokens) => non_empty(stringify_name(tokens, state)?),
        None => None,
    };
    let value = match &node.value {
        Some(tokens) => Some(stringify_value(tokens, state)?),
        None => None,
    };
    let mut children = Vec::new();
    for child in &node.elements {
        children.extend(convert_statement(child, state)?);
    }
    let attributes = match &node.attributes {
        Some(attributes) => Some(
            attributes
                .iter()
                .map(|attribute| convert_attribute(attribute, state))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        None => None,
    };
    let mut elem = Node {
        name,
        value,
        attributes,
        repeat,
        self_closing: node.self_close,
        ..Node::default()
    };
    // A text-only snippet without fields puts its children beside itself.
    if elem.name.is_none()
        && elem.attributes.is_none()
        && elem
            .value
            .as_ref()
            .is_some_and(|value| !value.iter().any(is_field))
    {
        let mut result = vec![elem];
        result.extend(children);
        return Ok(result);
    }
    elem.children = children;
    Ok(vec![elem])
}

fn convert_group(
    node: &Group,
    repeat: Option<Repeater>,
    state: &mut State<'_>,
) -> Result<Vec<Node>, EmmetError> {
    let mut result = Vec::new();
    for child in &node.elements {
        result.extend(convert_statement(child, state)?);
    }
    if let Some(repeat) = repeat {
        for item in &mut result {
            item.repeat.get_or_insert(repeat);
        }
    }
    Ok(result)
}

fn convert_attribute(
    node: &parser::Attribute,
    state: &mut State<'_>,
) -> Result<Attribute, EmmetError> {
    let mut value_type = ValueType::Raw;
    let name = match &node.name {
        Some(tokens) => Some(stringify_name(tokens, state)?),
        None => None,
    };
    let implied = name.as_ref().is_some_and(|name| name.starts_with('!'));
    let boolean = name.as_ref().is_some_and(|name| name.ends_with('.'));
    let value = match &node.value {
        Some(tokens) => {
            let mut tokens = tokens.as_slice();
            if let Some(Token::Quote { single }) = tokens.first() {
                // A quoted value loses its quotes but remembers them.
                value_type = if *single {
                    ValueType::SingleQuote
                } else {
                    ValueType::DoubleQuote
                };
                tokens = &tokens[1..];
                if matches!(tokens.last(), Some(Token::Quote { .. })) {
                    tokens = &tokens[..tokens.len() - 1];
                }
            } else if tokens.first().is_some_and(|token| {
                parser::is_bracket(token, BracketContext::Expression, Some(true))
            }) {
                value_type = ValueType::Expression;
                tokens = &tokens[1..];
                if tokens.last().is_some_and(|token| {
                    parser::is_bracket(token, BracketContext::Expression, Some(false))
                }) {
                    tokens = &tokens[..tokens.len() - 1];
                }
            }
            Some(stringify_value(tokens, state)?)
        }
        None => None,
    };
    let name = name.and_then(|name| {
        let from = usize::from(implied);
        let to = name.len() - usize::from(boolean);
        non_empty(name.get(from..to.max(from)).unwrap_or_default().to_string())
    });
    Ok(Attribute {
        name,
        value,
        boolean,
        implied,
        value_type,
    })
}

fn stringify_name(tokens: &[Token], state: &mut State<'_>) -> Result<String, EmmetError> {
    let mut name = String::new();
    for token in tokens {
        name.push_str(&stringify(token, state)?);
    }
    Ok(name)
}

/// The value's text, with each numbered field kept as a field.
fn stringify_value(tokens: &[Token], state: &mut State<'_>) -> Result<Vec<Value>, EmmetError> {
    let mut result = Vec::new();
    let mut text = String::new();
    for token in tokens {
        if let Token::Field {
            index: Some(index),
            name,
        } = token
        {
            if !text.is_empty() {
                result.push(Value::Text(std::mem::take(&mut text)));
            }
            result.push(Value::Field {
                index: *index,
                name: name.clone(),
            });
        } else {
            text.push_str(&stringify(token, state)?);
        }
    }
    if !text.is_empty() {
        result.push(Value::Text(text));
    }
    Ok(result)
}

/// emmet's `tokenVisitor`, quirks included: a group's closing bracket prints
/// as `}`, and a field without a placeholder loses its closing brace.
fn stringify(token: &Token, state: &mut State<'_>) -> Result<String, EmmetError> {
    Ok(match token {
        Token::Literal(value) | Token::WhiteSpace(value) => value.clone(),
        Token::Quote { single } => if *single { "'" } else { "\"" }.to_string(),
        Token::Bracket { open, context } => match (context, open) {
            (BracketContext::Attribute, true) => "[",
            (BracketContext::Attribute, false) => "]",
            (BracketContext::Expression, true) => "{",
            (BracketContext::Expression, false) | (BracketContext::Group, false) => "}",
            (BracketContext::Group, true) => "(",
        }
        .to_string(),
        Token::Operator(operator) => operator.symbol().to_string(),
        Token::Field {
            index: Some(index),
            name,
        } => {
            if name.is_empty() {
                format!("${{{index}")
            } else {
                format!("${{{index}:{name}}}")
            }
        }
        Token::Field { index: None, name } => {
            if name.is_empty() {
                String::new()
            } else {
                state
                    .variables
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone())
            }
        }
        Token::RepeaterPlaceholder => {
            // The text a repeated element would take: none here.
            state.inserted = true;
            String::new()
        }
        Token::RepeaterNumber {
            size,
            reverse,
            base,
            parent,
        } => repeater_number(*size, *reverse, *base, *parent, &state.repeaters),
        // emmet's visitor has no case for a repeater, and throws.
        Token::Repeater(_) => return Err(EmmetError),
    })
}

fn repeater_number(
    size: usize,
    reverse: bool,
    base: usize,
    parent: usize,
    repeaters: &[Repeater],
) -> String {
    let as_i64 = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
    let mut value = 1i64;
    if let Some(repeater) = repeaters.last() {
        value = if reverse {
            as_i64(base)
                .saturating_add(as_i64(repeater.count))
                .saturating_sub(as_i64(repeater.value))
                .saturating_sub(1)
        } else {
            as_i64(base).saturating_add(as_i64(repeater.value))
        };
        if parent > 0 {
            let last = repeaters.len() - 1;
            let parent_index = last.saturating_sub(parent);
            if parent_index != last {
                value = value.saturating_add(
                    as_i64(repeater.count).saturating_mul(as_i64(repeaters[parent_index].value)),
                );
            }
        }
    }
    let mut result = value.to_string();
    while result.len() < size {
        result.insert(0, '0');
    }
    result
}

const fn is_field(value: &Value) -> bool {
    matches!(value, Value::Field { .. })
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

pub(super) fn deepest_node(node: &mut Node) -> &mut Node {
    if node.children.is_empty() {
        return node;
    }
    let last = node.children.len() - 1;
    deepest_node(&mut node.children[last])
}

pub(super) fn insert_text(node: &mut Node, text: &str) {
    match &mut node.value {
        Some(value) => match value.last_mut() {
            Some(Value::Text(last)) => last.push_str(text),
            _ => value.push(Value::Text(text.to_string())),
        },
        None => node.value = Some(vec![Value::Text(text.to_string())]),
    }
}
