//! The HTML formatter: `html` in emmet 2.4.11 (`markup/format/html.ts`),
//! with emmet-helper's options for the `html` syntax: tab indentation,
//! TextMate fields, double quotes, `<html>` children not indented, `<body>`
//! content always on its own line and no inline-element line breaking.

use std::fmt::Write as _;

use super::convert::{Attribute, Node, Value, ValueType};
use super::{Data, Options};

/// The caret an empty element or attribute gets.
static CARET: [Value; 1] = [Value::Field {
    index: 0,
    name: String::new(),
}];

struct Output<'a> {
    data: &'a Data,
    options: Options,
    value: String,
    level: i64,
    line: usize,
    /// The number the next numbered field starts from.
    field: usize,
}

pub(super) fn html(nodes: &[Node], data: &Data, options: Options) -> String {
    let mut out = Output {
        data,
        options,
        value: String::new(),
        level: 0,
        line: 0,
        field: 1,
    };
    for (index, node) in nodes.iter().enumerate() {
        element(node, index, nodes, None, &mut out);
    }
    out.value
}

fn element(node: &Node, index: usize, items: &[Node], parent: Option<&Node>, out: &mut Output<'_>) {
    let format = should_format(node, index, items, parent, out.data);
    let level = indent_of(parent);
    out.level += level;
    if format {
        out.newline(out.level);
    }
    if let Some(name) = &node.name {
        out.push_string(&format!("<{name}"));
        for attr in node.attributes.iter().flatten() {
            if should_output_attribute(attr) {
                push_attribute(attr, out);
            }
        }
        if node.self_closing && node.children.is_empty() && node.value.is_none() {
            out.push_string(">");
        } else {
            out.push_string(">");
            if !push_snippet(node, out) {
                if let Some(value) = &node.value {
                    let inner =
                        value.iter().any(has_newline) || starts_with_block_tag(value, out.data);
                    if inner {
                        out.level += 1;
                        out.newline(out.level);
                    }
                    push_tokens(value, out);
                    if inner {
                        out.level -= 1;
                        out.newline(out.level);
                    }
                }
                for (i, child) in node.children.iter().enumerate() {
                    element(child, i, &node.children, Some(node), out);
                }
                if node.value.is_none() && node.children.is_empty() {
                    // `output.formatForce`: `<body>` content on its own line.
                    let inner = name == "body";
                    if inner {
                        out.level += 1;
                        out.newline(out.level);
                    }
                    push_tokens(&CARET, out);
                    if inner {
                        out.level -= 1;
                        out.newline(out.level);
                    }
                }
            }
            out.push_string(&format!("</{name}>"));
            comment_after(node, out);
        }
    } else if !push_snippet(node, out)
        && let Some(value) = &node.value
    {
        // A text-only node (snippet).
        push_tokens(value, out);
        for (i, child) in node.children.iter().enumerate() {
            element(child, i, &node.children, Some(node), out);
        }
    }
    if format && index + 1 == items.len() && parent.is_some() {
        let offset = if parent.is_some_and(is_snippet) { 0 } else { 1 };
        out.newline(out.level - offset);
    }
    out.level -= level;
}

fn push_attribute(attr: &Attribute, out: &mut Output<'_>) {
    let Some(name) = &attr.name else {
        return;
    };
    let (open, close) = if attr.value_type == ValueType::Expression {
        ("{", "}")
    } else {
        ("\"", "\"")
    };
    let boolean_value;
    let value: &[Value] = match &attr.value {
        Some(value) => value,
        // `output.compactBoolean` is off: `disabled="disabled"`.
        None if attr.boolean || out.data.is_boolean_attribute(name) => {
            boolean_value = [Value::Text(name.clone())];
            &boolean_value
        }
        None => &CARET,
    };
    out.push_string(&format!(" {name}={open}"));
    push_tokens(value, out);
    out.push_string(close);
}

/// A value holding fields and children: the children take the first field's
/// place.
fn push_snippet(node: &Node, out: &mut Output<'_>) -> bool {
    let Some(value) = &node.value else {
        return false;
    };
    if node.children.is_empty() {
        return false;
    }
    let Some(field) = value
        .iter()
        .position(|token| matches!(token, Value::Field { .. }))
    else {
        return false;
    };
    push_tokens(&value[..field], out);
    let line = out.line;
    let mut pos = field + 1;
    for (i, child) in node.children.iter().enumerate() {
        element(child, i, &node.children, Some(node), out);
    }
    // After a line change, the text resumes without its leading whitespace.
    if out.line != line
        && let Some(Value::Text(text)) = value.get(pos)
    {
        out.push_string(text.trim_start_matches(is_js_space));
        pos += 1;
    }
    push_tokens(value.get(pos..).unwrap_or_default(), out);
    true
}

/// `shouldFormat`: whether the node starts on a line of its own.
fn should_format(
    node: &Node,
    index: usize,
    items: &[Node],
    parent: Option<&Node>,
    data: &Data,
) -> bool {
    if index == 0 && parent.is_none() {
        return false;
    }
    // A snippet's single child stays inline.
    if parent.is_some_and(is_snippet) && items.len() == 1 {
        return false;
    }
    if is_snippet(node) {
        let value = node.value.as_deref().unwrap_or_default();
        // Adjacent text nodes, a code fragment, or a wrapper whose children
        // fill its field.
        let format = index
            .checked_sub(1)
            .and_then(|previous| items.get(previous))
            .is_some_and(is_snippet)
            || items.get(index + 1).is_some_and(is_snippet)
            || value.iter().any(has_newline)
            || (value
                .iter()
                .any(|token| matches!(token, Value::Field { .. }))
                && !node.children.is_empty());
        if format {
            return true;
        }
    }
    if is_inline(node, data) {
        if index == 0 {
            // A first inline node followed by a block one.
            if items.iter().any(|item| !is_inline(item, data)) {
                return true;
            }
        } else if !is_inline(&items[index - 1], data) {
            // Right after a block node.
            return true;
        }
        // An inline node holding a node that is formatted.
        return node
            .children
            .iter()
            .enumerate()
            .any(|(i, child)| should_format(child, i, &node.children, parent, data));
    }
    true
}

/// `getIndent`: children of `<html>` and of snippets are not indented.
fn indent_of(parent: Option<&Node>) -> i64 {
    match parent {
        Some(parent) if !is_snippet(parent) && parent.name.as_deref() != Some("html") => 1,
        _ => 0,
    }
}

const fn is_snippet(node: &Node) -> bool {
    node.name.is_none() && node.attributes.is_none()
}

/// An inline-level element, or a text-only node.
fn is_inline(node: &Node, data: &Data) -> bool {
    match &node.name {
        Some(name) => data.is_inline(name),
        None => node.value.is_some() && node.attributes.is_none(),
    }
}

fn has_newline(token: &Value) -> bool {
    matches!(token, Value::Text(text) if text.contains(['\r', '\n']))
}

/// `/^<([\w\-:]+)[\s>]/` naming a block-level tag.
fn starts_with_block_tag(value: &[Value], data: &Data) -> bool {
    let Some(Value::Text(text)) = value.first() else {
        return false;
    };
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let name_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':')))
        .unwrap_or(rest.len());
    if name_len == 0 {
        return false;
    }
    let follows = rest[name_len..].chars().next();
    follows.is_some_and(|c| c == '>' || is_js_space(c)) && !data.is_inline(&rest[..name_len])
}

/// JavaScript's `\s`, which has U+FEFF and not U+0085.
pub(super) fn is_js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

const fn should_output_attribute(attr: &Attribute) -> bool {
    // An implied attribute (`[!href]`) appears only once it has a value.
    !attr.implied
        || !matches!(attr.value_type, ValueType::Raw)
        || matches!(&attr.value, Some(value) if !value.is_empty())
}

fn push_tokens(tokens: &[Value], out: &mut Output<'_>) {
    let mut largest = None;
    for token in tokens {
        match token {
            Value::Text(text) => out.push_string(text),
            Value::Field { index, name } => {
                let number = out.field + index;
                if name.is_empty() {
                    let _ = write!(out.value, "${{{number}}}");
                } else {
                    let _ = write!(out.value, "${{{number}:{name}}}");
                }
                largest = largest.max(Some(*index));
            }
        }
    }
    if let Some(largest) = largest {
        out.field += largest + 1;
    }
}

/// The `|c` filter's comment after an element with an `id` or a `class`:
/// emmet's `\n<!-- /[#ID][.CLASS] -->`.
fn comment_after(node: &Node, out: &mut Output<'_>) {
    let attributes = node.attributes.as_deref().unwrap_or_default();
    if !out.options.comment
        || node.name.is_none()
        || node.attributes.is_none()
        || !attributes
            .iter()
            .any(|attr| matches!(attr.name.as_deref(), Some("id" | "class")))
    {
        return;
    }
    let value_of = |key: &str| {
        attributes
            .iter()
            .rev()
            .find(|attr| {
                attr.value.is_some()
                    && attr
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_uppercase() == key)
            })
            .and_then(|attr| attr.value.clone())
    };
    out.push_string("\n<!-- /");
    for (before, key) in [("#", "ID"), (".", "CLASS")] {
        if let Some(value) = value_of(key) {
            out.push_string(before);
            push_tokens(&value, out);
        }
    }
    out.push_string(" -->");
}

impl Output<'_> {
    /// Text, its line breaks re-indented at the current level.
    fn push_string(&mut self, text: &str) {
        let mut lines = split_lines(text).into_iter().peekable();
        while let Some(line) = lines.next() {
            self.value.push_str(line);
            if lines.peek().is_some() {
                self.newline(self.level);
            }
        }
    }

    fn newline(&mut self, indent: i64) {
        self.value.push('\n');
        self.line += 1;
        for _ in 0..indent.max(0) {
            self.value.push('\t');
        }
    }
}

/// `text.split(/\r\n|\r|\n/g)`.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                lines.push(&text[start..i]);
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = i;
            }
            b'\n' => {
                lines.push(&text[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    lines.push(&text[start..]);
    lines
}
