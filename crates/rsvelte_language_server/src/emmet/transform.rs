//! Snippet resolution and the markup transforms emmet 2.4.11 runs before
//! output (`parse` in `markup/index.ts`): implicit tag names, attribute
//! merging, lorem ipsum, `<label>` cleanup and the BEM filter.

use std::collections::HashMap;

use super::convert::{self, Attribute, Node, Value, ValueType};
use super::{Data, Options};

/// `resolveSnippets`: every node named after a snippet becomes that
/// snippet's nodes, carrying the node's attributes, value and repeater.
pub(super) fn resolve_snippets(nodes: Vec<Node>, data: &Data) -> Vec<Node> {
    walk_resolve(nodes, data, &mut Vec::new())
}

fn walk_resolve<'a>(nodes: Vec<Node>, data: &'a Data, stack: &mut Vec<&'a str>) -> Vec<Node> {
    let mut result = Vec::new();
    for mut child in nodes {
        if let Some(mut resolved) = resolve(&child, data, stack) {
            let children = walk_resolve(std::mem::take(&mut child.children), data, stack);
            if let Some(last) = resolved.last_mut() {
                convert::deepest_node(last).children.extend(children);
            }
            result.extend(resolved);
        } else {
            child.children = walk_resolve(std::mem::take(&mut child.children), data, stack);
            result.push(child);
        }
    }
    result
}

fn resolve<'a>(child: &Node, data: &'a Data, stack: &mut Vec<&'a str>) -> Option<Vec<Node>> {
    let snippet = data.snippets.get(child.name.as_deref()?)?.as_str();
    // A snippet already being resolved — `img` is `img[src alt]/` — stays as
    // it is.
    if stack.contains(&snippet) {
        return None;
    }
    let nodes = convert::parse_abbreviation(snippet, &data.variables).ok()?;
    stack.push(snippet);
    let mut nodes = walk_resolve(nodes, data, stack);
    stack.pop();
    for top in &mut nodes {
        if let Some(attributes) = &child.attributes {
            top.attributes
                .get_or_insert_default()
                .extend(attributes.iter().cloned());
        }
        if child.self_closing {
            top.self_closing = true;
        }
        if child.value.is_some() {
            top.value.clone_from(&child.value);
        }
        if child.repeat.is_some() {
            top.repeat = child.repeat;
        }
    }
    Some(nodes)
}

/// What a transform reads of an ancestor, taken once the ancestor itself is
/// transformed.
struct Ancestor {
    name: Option<String>,
    repeat: Option<super::tokenizer::Repeater>,
    block: Option<String>,
}

pub(super) fn transform_all(nodes: &mut [Node], data: &Data, options: Options, lorem: &mut Lorem) {
    walk_transform(nodes, &mut Vec::new(), data, options, lorem);
}

fn walk_transform(
    nodes: &mut [Node],
    ancestors: &mut Vec<Ancestor>,
    data: &Data,
    options: Options,
    source: &mut Lorem,
) {
    for node in nodes {
        implicit_tag(node, ancestors, data);
        merge_attributes(node);
        lorem(node, ancestors, data, source);
        label(node);
        if options.bem {
            bem(node, ancestors);
        }
        ancestors.push(Ancestor {
            name: node.name.clone(),
            repeat: node.repeat,
            block: node.bem.as_ref().and_then(|bem| bem.block.clone()),
        });
        walk_transform(&mut node.children, ancestors, data, options, source);
        ancestors.pop();
    }
}

fn implicit_tag(node: &mut Node, ancestors: &[Ancestor], data: &Data) {
    if node.name.is_none() && node.attributes.is_some() {
        resolve_implicit_tag(node, ancestors, data);
    }
}

/// The name an element without one takes from its parent: `li` in a list,
/// `span` in an inline element, otherwise `div`.
fn resolve_implicit_tag(node: &mut Node, ancestors: &[Ancestor], data: &Data) {
    let parent = ancestors
        .last()
        .and_then(|parent| parent.name.as_deref())
        .unwrap_or_default()
        .to_lowercase();
    let name = match parent.as_str() {
        "p" => "span",
        "ul" | "ol" => "li",
        "table" | "tbody" | "thead" | "tfoot" => "tr",
        "tr" => "td",
        "colgroup" => "col",
        "select" | "optgroup" => "option",
        "audio" | "video" => "source",
        "object" => "param",
        "map" => "area",
        name if data.is_inline(name) => "span",
        _ => "div",
    };
    node.name = Some(name.to_string());
}

/// `mergeAttributes`: one attribute per name; classes join with a space and
/// any other repeated name keeps the later declaration.
fn merge_attributes(node: &mut Node) {
    let Some(attributes) = node.attributes.take() else {
        return;
    };
    let mut merged: Vec<Attribute> = Vec::new();
    let mut lookup: HashMap<String, usize> = HashMap::new();
    for attr in attributes {
        let Some(name) = attr.name.clone() else {
            merged.push(attr);
            continue;
        };
        if let Some(&index) = lookup.get(&name) {
            let prev = &mut merged[index];
            if name == "class" {
                prev.value = merge_value(prev.value.take(), attr.value, " ");
            } else {
                prev.value = attr.value;
                prev.implied |= attr.implied;
                prev.boolean |= attr.boolean;
                if prev.value_type != ValueType::Expression {
                    prev.value_type = attr.value_type;
                }
            }
        } else {
            lookup.insert(name, merged.len());
            merged.push(attr);
        }
    }
    node.attributes = Some(merged);
}

fn merge_value(
    prev: Option<Vec<Value>>,
    next: Option<Vec<Value>>,
    glue: &str,
) -> Option<Vec<Value>> {
    match (prev, next) {
        (Some(mut prev), Some(next)) => {
            if !prev.is_empty() && !glue.is_empty() {
                append(&mut prev, Value::Text(glue.to_string()));
            }
            for token in next {
                append(&mut prev, token);
            }
            Some(prev)
        }
        (prev, next) => prev.or(next),
    }
}

fn append(tokens: &mut Vec<Value>, value: Value) {
    if let (Some(Value::Text(last)), Value::Text(text)) = (tokens.last_mut(), &value) {
        last.push_str(text);
    } else {
        tokens.push(value);
    }
}

/// The most lorem ipsum words one expansion holds. emmet has no limit, and
/// `lorem99999999999` or `lorem10000*1000` would have the server build text
/// no editor can show; past it, the words run out.
pub(super) const MAX_LOREM_WORDS: usize = 10_000;

/// Lorem ipsum's randomness (emmet uses `Math.random`), and the words an
/// expansion has left.
pub(super) struct Lorem {
    rng: Rng,
    words_left: usize,
}

impl Lorem {
    pub(super) fn new() -> Self {
        Self {
            rng: Rng::new(),
            words_left: MAX_LOREM_WORDS,
        }
    }
}

/// `lorem`, `loremN`, `loremN-M` and their language variants (`loremru`).
fn lorem(node: &mut Node, ancestors: &[Ancestor], data: &Data, source: &mut Lorem) {
    let Some((language, min, max)) = node.name.as_deref().and_then(lorem_shape) else {
        return;
    };
    let vocabulary = data
        .lorem
        .get(language.as_str())
        .unwrap_or(&data.lorem["latin"]);
    let min = min.map_or(30, |min| min.max(1)).min(MAX_LOREM_WORDS);
    let max = max.map_or(min, |max| max.max(min)).min(MAX_LOREM_WORDS);
    let word_count = source.rng.range(min, max).min(source.words_left);
    source.words_left -= word_count;
    let repeat = node
        .repeat
        .or_else(|| ancestors.iter().rev().find_map(|ancestor| ancestor.repeat));
    node.name = None;
    node.attributes = None;
    let start_with_common = repeat.is_none_or(|repeat| repeat.value == 0);
    node.value = Some(vec![Value::Text(paragraph(
        vocabulary,
        word_count,
        start_with_common,
        &mut source.rng,
    ))]);
    if node.repeat.is_some() && !ancestors.is_empty() {
        resolve_implicit_tag(node, ancestors, data);
    }
}

/// `/^lorem([a-z]*)(\d*)(-\d*)?$/i`: the language, and the word counts.
fn lorem_shape(name: &str) -> Option<(String, Option<usize>, Option<usize>)> {
    let prefix = name.get(..5)?;
    if !prefix.eq_ignore_ascii_case("lorem") {
        return None;
    }
    let rest = &name[5..];
    let language_end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let (language, rest) = rest.split_at(language_end);
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (min, rest) = rest.split_at(digits_end);
    let max = match rest.strip_prefix('-') {
        Some(max) if max.bytes().all(|byte| byte.is_ascii_digit()) => Some(max),
        Some(_) => return None,
        None if rest.is_empty() => None,
        None => return None,
    };
    let number = |digits: &str| digits.parse::<usize>().unwrap_or(usize::MAX);
    Some((
        language.to_string(),
        (!min.is_empty()).then(|| number(min)),
        // `Number('')` is 0, which the minimum then overrides.
        max.map(|max| if max.is_empty() { 0 } else { number(max) }),
    ))
}

fn paragraph(
    vocabulary: &super::Vocabulary,
    word_count: usize,
    start_with_common: bool,
    rng: &mut Rng,
) -> String {
    let mut result = Vec::new();
    let mut total = 0;
    if word_count == 0 {
        return String::new();
    }
    if start_with_common && !vocabulary.common.is_empty() {
        let words: Vec<String> = vocabulary.common.iter().take(word_count).cloned().collect();
        total += words.len();
        result.push(sentence(insert_commas(words, rng), Some("."), rng));
    }
    while total < word_count {
        let count = rng.range(2, 30).min(word_count - total);
        let words = sample(&vocabulary.words, count, rng);
        total += words.len();
        result.push(sentence(insert_commas(words, rng), None, rng));
    }
    result.join(" ")
}

fn sample(words: &[String], count: usize, rng: &mut Rng) -> Vec<String> {
    let distinct = {
        let mut seen: Vec<&String> = words.iter().collect();
        seen.sort();
        seen.dedup();
        seen.len()
    };
    let iterations = words.len().min(count).min(distinct);
    let mut result: Vec<String> = Vec::new();
    while result.len() < iterations {
        let word = &words[rng.range(0, words.len())];
        if !result.contains(word) {
            result.push(word.clone());
        }
    }
    result
}

fn sentence(mut words: Vec<String>, end: Option<&str>, rng: &mut Rng) -> String {
    if let Some(first) = words.first_mut() {
        let mut chars = first.chars();
        if let Some(initial) = chars.next() {
            *first = initial.to_uppercase().chain(chars).collect();
        }
    }
    // More dots than question marks: emmet picks from `?!..`.
    let end = end.unwrap_or_else(|| ["?", "!", ".", "."][rng.range(0, 4)]);
    format!("{}{end}", words.join(" "))
}

fn insert_commas(mut words: Vec<String>, rng: &mut Rng) -> Vec<String> {
    let len = words.len();
    if len < 2 {
        return words;
    }
    let total = if (4..=6).contains(&len) {
        rng.range(0, 1)
    } else if (7..=12).contains(&len) {
        rng.range(0, 2)
    } else {
        rng.range(1, 4)
    };
    for _ in 0..total {
        let pos = rng.range(0, len - 2);
        if !words[pos].ends_with(',') {
            words[pos].push(',');
        }
    }
    words
}

/// A `<label>` holding an `<input>` or `<textarea>` loses its empty `for`,
/// and the field its empty `id`.
fn label(node: &mut Node) {
    if node.name.as_deref() != Some("label") {
        return;
    }
    let Some(input) = find_field(&mut node.children) else {
        return;
    };
    if let Some(attributes) = &mut input.attributes {
        attributes.retain(|attr| !(attr.name.as_deref() == Some("id") && is_empty_attribute(attr)));
    }
    if let Some(attributes) = &mut node.attributes {
        attributes
            .retain(|attr| !(attr.name.as_deref() == Some("for") && is_empty_attribute(attr)));
    }
}

fn find_field(children: &mut [Node]) -> Option<&mut Node> {
    for child in children {
        if matches!(child.name.as_deref(), Some("input" | "textarea")) {
            return Some(child);
        }
        if let Some(found) = find_field(&mut child.children) {
            return Some(found);
        }
    }
    None
}

fn is_empty_attribute(attr: &Attribute) -> bool {
    match attr.value.as_deref() {
        None => true,
        Some([Value::Field { name, .. }]) => name.is_empty(),
        Some(_) => false,
    }
}

/// The BEM filter's view of a node's classes (`node._bem`).
#[derive(Clone, Debug)]
pub(super) struct BemData {
    class_names: Vec<String>,
    block: Option<String>,
}

const BEM_ELEMENT: &str = "__";
const BEM_MODIFIER: &str = "_";

fn bem(node: &mut Node, ancestors: &[Ancestor]) {
    expand_class_names(node);
    expand_short_notation(node, ancestors);
}

/// `b__el_mod` also gets `b__el`.
fn expand_class_names(node: &mut Node) {
    let mut data = bem_data(node);
    let mut class_names = Vec::new();
    for class in &data.class_names {
        match class.find('_') {
            Some(index) if index > 0 && !class.starts_with('-') => {
                class_names.push(class[..index].to_string());
                class_names.push(class[index..].to_string());
            }
            _ => class_names.push(class.clone()),
        }
    }
    if !class_names.is_empty() {
        data.class_names = unique_classes(class_names);
        data.block = find_block_name(&data.class_names);
        update_class(node, data.class_names.join(" "));
    }
    node.bem = Some(data);
}

/// `-element` and `_modifier` against the nearest block.
fn expand_short_notation(node: &mut Node, ancestors: &[Ancestor]) {
    let data = bem_data(node);
    let own_block = data.block.clone();
    let block_name = |depth: usize| -> String {
        // The path is the ancestors and the node itself, searched from
        // `depth` levels up towards the root.
        let len = ancestors.len() + 1;
        let start = len.saturating_sub(depth);
        (0..=start)
            .rev()
            .find_map(|index| {
                if index == len - 1 {
                    own_block.clone()
                } else {
                    ancestors
                        .get(index)
                        .and_then(|ancestor| ancestor.block.clone())
                }
            })
            .unwrap_or_default()
    };
    let mut class_names = Vec::new();
    for original in &data.class_names {
        let mut class = original.as_str();
        let mut prefix = String::new();
        if let Some((dashes, name, matched)) = bem_prefix(class, '-', false) {
            prefix = format!("{}{BEM_ELEMENT}{name}", block_name(dashes));
            class_names.push(prefix.clone());
            class = &class[matched..];
        }
        if let Some((underscores, name, matched)) = bem_prefix(class, '_', true) {
            if prefix.is_empty() {
                prefix = block_name(underscores);
                class_names.push(prefix.clone());
            }
            class_names.push(format!("{prefix}{BEM_MODIFIER}{name}"));
            class = &class[matched..];
        }
        if class == original {
            class_names.push(original.clone());
        }
    }
    let class_names = unique_classes(class_names);
    if !class_names.is_empty() {
        update_class(node, class_names.join(" "));
    }
}

/// `/^(-+)([a-z0-9]+[a-z0-9-]*)/i`, or with `_` and `underscore_in_name`,
/// `/^(_+)([a-z0-9]+[a-z0-9-_]*)/i`: the run's length, the name and the
/// matched length.
fn bem_prefix(class: &str, marker: char, underscore_in_name: bool) -> Option<(usize, &str, usize)> {
    let run = class.len() - class.trim_start_matches(marker).len();
    if run == 0 {
        return None;
    }
    let rest = &class[run..];
    if !rest.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    let name_len = rest
        .find(|c: char| {
            !(c.is_ascii_alphanumeric() || c == '-' || (underscore_in_name && c == '_'))
        })
        .unwrap_or(rest.len());
    Some((run, &rest[..name_len], run + name_len))
}

fn bem_data(node: &Node) -> BemData {
    if let Some(data) = &node.bem {
        return data.clone();
    }
    let class_value = node
        .attributes
        .iter()
        .flatten()
        .find(|attr| attr.name.as_deref() == Some("class") && attr.value.is_some())
        .and_then(|attr| attr.value.as_ref())
        .map(|value| {
            value
                .iter()
                .map(|token| match token {
                    Value::Text(text) => text.as_str(),
                    Value::Field { name, .. } => name.as_str(),
                })
                .collect::<String>()
        })
        .unwrap_or_default();
    // `classValue.split(/\s+/)`, keeping the empty ends a split leaves.
    let class_names: Vec<String> = if class_value.is_empty() {
        Vec::new()
    } else {
        split_whitespace_runs(&class_value)
    };
    let block = find_block_name(&class_names);
    BemData { class_names, block }
}

fn split_whitespace_runs(text: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_space {
                parts.push(String::new());
            }
            in_space = true;
        } else {
            in_space = false;
            if let Some(last) = parts.last_mut() {
                last.push(c);
            }
        }
    }
    parts
}

fn find_block_name(class_names: &[String]) -> Option<String> {
    find_class(class_names, |class| {
        let mut chars = class.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some('-')
    })
    .or_else(|| {
        find_class(class_names, |class| {
            class.starts_with(|c: char| c.is_ascii_alphabetic())
        })
    })
}

/// The first class before any element or modifier class that passes `filter`.
fn find_class(class_names: &[String], filter: impl Fn(&str) -> bool) -> Option<String> {
    for class in class_names {
        if bem_prefix(class, '-', false).is_some() || bem_prefix(class, '_', true).is_some() {
            break;
        }
        if filter(class) {
            return Some(class.clone());
        }
    }
    None
}

fn update_class(node: &mut Node, value: String) {
    if let Some(attr) = node
        .attributes
        .iter_mut()
        .flatten()
        .find(|attr| attr.name.as_deref() == Some("class"))
    {
        attr.value = Some(vec![Value::Text(value)]);
    }
}

fn unique_classes(classes: Vec<String>) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for class in classes {
        if !class.is_empty() && !result.contains(&class) {
            result.push(class);
        }
    }
    result
}

/// The randomness lorem ipsum needs (emmet uses `Math.random`).
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                elapsed.as_secs() ^ (u64::from(elapsed.subsec_nanos()) << 32)
            });
        let seed = time ^ COUNTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
        Self(seed | 1)
    }

    /// `Math.floor(Math.random() * (to - from) + from)`.
    fn range(&mut self, from: usize, to: usize) -> usize {
        if to <= from {
            return from;
        }
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let random = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        let span = u64::try_from(to - from).unwrap_or(u64::MAX);
        from + usize::try_from(random % span).unwrap_or_default()
    }
}
