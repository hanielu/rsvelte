//! Emmet abbreviations in markup: a port of the official server's
//! `doComplete(document, position, 'html', {})` from `@vscode/emmet-helper`
//! 2.8.4 and the markup half of `emmet` 2.4.11 it expands with.
//!
//! `data.json` holds what those packages ship as data — the `html` snippet
//! registry, the variables, emmet-helper's commonly used tags and the lorem
//! ipsum vocabularies — extracted from the installed versions.
//!
//! Both are MIT-licensed (emmet © Sergey Chikuyonok, emmet-helper ©
//! Microsoft); their notices are in `NOTICE.md`. Regenerate `data.json` and
//! the tests' oracle with `node scripts/dev/generate-emmet-data.mjs`.

mod convert;
mod extract;
mod html;
mod parser;
mod tokenizer;
mod transform;

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

/// An abbreviation that does not parse; emmet throws and emmet-helper offers
/// nothing.
#[derive(Debug)]
pub(crate) struct EmmetError;

/// One Emmet completion: the expansion replaces `start..end`, UTF-16
/// columns in the cursor's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    /// A snippet, ending in `${0}`.
    pub new_text: String,
    /// The expansion with each tab stop shown as `|`.
    pub documentation: String,
    pub start: usize,
    pub end: usize,
}

/// The suggestions for the abbreviation ending at UTF-16 column `cursor` of
/// `line`, the cursor's whole line.
#[must_use]
pub fn suggestions(line: &str, cursor: usize) -> Vec<Suggestion> {
    let data = data();
    let line: Vec<u16> = line.encode_utf16().collect();
    let till_cursor = &line[..cursor.min(line.len())];
    let (pos, filter) = filters(till_cursor, cursor);
    let filter_len = filter.as_ref().map_or(0, |filter| filter.len() + 1);
    let Some(extracted) = extract::extract(&line, pos) else {
        return Vec::new();
    };
    let abbreviation = String::from_utf16_lossy(&extracted.abbreviation);
    let start = extracted.location;
    let end = start + extracted.abbreviation.len() + filter_len;
    // An open tag being typed (`<div`) is not expanded.
    let till_cursor = String::from_utf16_lossy(till_cursor);
    if current_word(&till_cursor) == abbreviation
        && till_cursor.ends_with(&format!("<{abbreviation}"))
    {
        return Vec::new();
    }
    let options = Options {
        bem: filter
            .as_deref()
            .is_some_and(|filter| filter.split(',').any(|f| f.trim() == "bem")),
        comment: filter
            .as_deref()
            .is_some_and(|filter| filter.split(',').any(|f| f.trim() == "c")),
    };
    let item = |label: String, expanded: &str| Suggestion {
        label,
        new_text: escape_non_tab_stop_dollar(&add_final_tab_stop(expanded)),
        documentation: replace_tab_stops_with_cursors(expanded),
        start,
        end,
    };

    let mut items = Vec::new();
    let mut expanded_abbreviation = None;
    if is_abbreviation_valid(&abbreviation) {
        let expanded = expand(&abbreviation, options, data).unwrap_or_default();
        if !expanded.is_empty() && !is_expanded_text_noise(&abbreviation, &expanded, data) {
            let mut label = abbreviation.clone();
            if let Some(filter) = &filter {
                label.push('|');
                label.push_str(&filter.replacen(',', "|", 1));
            }
            let suggestion = item(label, &expanded);
            expanded_abbreviation = Some(suggestion.new_text.clone());
            items.push(suggestion);
        }
    }

    // `a` also offers `address`, `article`…; `ul>li` offers what `li` starts.
    let prefix = last_tag(&abbreviation).unwrap_or(&abbreviation);
    if !prefix.is_empty() {
        let lower = prefix.to_lowercase();
        let prefix_len = prefix.encode_utf16().count();
        for tag in &data.tags {
            if !tag.starts_with(&lower) || *tag == lower {
                continue;
            }
            let rest = tag.get(prefix_len..).unwrap_or_default();
            let Ok(expanded) = expand(&format!("{abbreviation}{rest}"), options, data) else {
                continue;
            };
            if !expanded.is_empty() {
                items.push(item(format!("{prefix}{rest}"), &expanded));
            }
        }
    }

    // A custom tag with a colon (`foo:bar`) is offered once, not also as
    // itself (microsoft/vscode#66680).
    if items.len() >= 2
        && abbreviation.contains(':')
        && expanded_abbreviation.as_deref()
            == Some(format!("<{abbreviation}>${{0}}</{abbreviation}>").as_str())
    {
        items.retain(|item| item.label != abbreviation);
    }
    items
}

/// What follows the last `>` or `+`, when it is a tag name.
fn last_tag(abbreviation: &str) -> Option<&str> {
    let index = abbreviation.rfind(['>', '+'])?;
    let tag = &abbreviation[index + 1..];
    (!tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-')))
    .then_some(tag)
}

/// `getFilters`: up to three trailing `|bem`, `|c` or `|t` filters, and the
/// abbreviation's end without them.
fn filters(till_cursor: &[u16], cursor: usize) -> (usize, Option<String>) {
    let mut pos = cursor.min(till_cursor.len());
    let mut filter: Option<String> = None;
    for _ in 0..3 {
        let Some(name) = ["bem", "c", "t"].into_iter().find(|name| {
            let suffix: Vec<u16> = format!("|{name}").encode_utf16().collect();
            till_cursor[..pos].ends_with(&suffix)
        }) else {
            break;
        };
        pos -= name.len() + 1;
        filter = Some(match filter {
            Some(filter) => format!("{name},{filter}"),
            None => name.to_string(),
        });
    }
    (pos, filter)
}

/// `getCurrentWord`: `/[\w,:,-,\.]*$/` — whose `,-,` is a range of one comma,
/// so a dash ends the word.
fn current_word(till_cursor: &str) -> &str {
    let start = till_cursor
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '_' | ',' | ':' | '.'))
        .last()
        .map_or(till_cursor.len(), |(index, _)| index);
    &till_cursor[start..]
}

/// `isAbbreviationValid` for `html`.
fn is_abbreviation_valid(abbreviation: &str) -> bool {
    static PAREN_RULES: OnceLock<[regex::Regex; 4]> = OnceLock::new();
    if abbreviation.is_empty() {
        return false;
    }
    if abbreviation.starts_with('!') {
        return abbreviation.chars().all(|c| c == '!');
    }
    // Text in parentheses is not an abbreviation, unless it is a group
    // beside an operator, or inside braces or a quoted attribute value.
    if abbreviation.contains(['(', ')']) {
        // JavaScript's `.` matches neither line terminator.
        let rules = PAREN_RULES.get_or_init(|| {
            [
                r"\{[^}{]*[()]+[^}{]*\}(?:[>+*^]|$)",
                r"\(DOT*\)[>+*^]",
                r#"\[[^\[\]()]+="DOT*"\]"#,
                r"[>+*^]\(DOT*\)",
            ]
            .map(|rule| {
                regex::Regex::new(&rule.replace("DOT", r"[^\n\r\u{2028}\u{2029}]"))
                    .expect("valid regex")
            })
        });
        if !rules.iter().any(|rule| rule.is_match(abbreviation)) {
            return false;
        }
    }
    abbreviation.starts_with(|c: char| c.is_ascii_alphabetic() || ",!([#.{".contains(c))
        && abbreviation.contains(|c: char| c.is_ascii_alphabetic() || matches!(c, ',' | '.'))
}

/// `isExpandedTextNoise` for `html`: plain words expand to themselves as a
/// tag, and are not offered.
fn is_expanded_text_noise(abbreviation: &str, expanded: &str, data: &Data) -> bool {
    let lower = abbreviation.to_lowercase();
    if data.tags.contains(&lower) || data.snippets.contains_key(abbreviation) {
        return false;
    }
    // A custom tag can have `-` or `:`.
    if abbreviation.contains(['-', ',', ':'])
        && !abbreviation.contains("--")
        && !abbreviation.contains("::")
        && !abbreviation.ends_with(':')
    {
        return false;
    }
    if abbreviation == "." {
        return false;
    }
    // Text ending in a period is a sentence, unless it is `div.`.
    if let Some(word) = abbreviation.strip_suffix('.')
        && word.chars().all(|c| c.is_ascii_alphanumeric() || c == ',')
    {
        return word.is_empty() || !data.tags.iter().any(|tag| tag == word && tag != "lorem");
    }
    expanded.to_lowercase() == format!("<{lower}>${{1}}</{lower}>")
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Options {
    /// The `|bem` filter.
    pub bem: bool,
    /// The `|c` filter.
    pub comment: bool,
}

/// Expand a markup abbreviation as emmet's `expand` with emmet-helper's
/// options for `html`.
fn expand(abbreviation: &str, options: Options, data: &Data) -> Result<String, EmmetError> {
    let nodes = convert::parse_abbreviation(abbreviation, &data.variables)?;
    let mut nodes = transform::resolve_snippets(nodes, data);
    transform::transform_all(&mut nodes, data, options, &mut transform::Lorem::new());
    Ok(html::html(&nodes, data, options))
}

/// `addFinalTabStop`: the highest plain tab stop becomes `${0}`, unless the
/// expansion has one already.
fn add_final_tab_stop(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut max_tab_stop: i64 = -1;
    let mut max_ranges: Vec<(usize, usize)> = Vec::new();
    let mut found_last_stop = false;
    let mut replace_with_last_stop = false;
    let mut i = 0;
    while i < n && !found_last_stop {
        // Look for `${`.
        let dollar = chars[i] == '$';
        i += 1;
        if !dollar {
            continue;
        }
        let brace = chars.get(i) == Some(&'{');
        i += 1;
        if !brace {
            continue;
        }
        let number_start = i;
        while i < n && chars[i].is_ascii_digit() {
            i += 1;
        }
        let number_end = i;
        if number_start == number_end || i >= n || (chars[i] != '}' && chars[i] != ':') {
            continue;
        }
        let number: String = chars[number_start..number_end].iter().collect();
        found_last_stop = number == "0";
        if found_last_stop {
            break;
        }
        let mut found_placeholder = false;
        let colon = chars[i] == ':';
        i += 1;
        if colon {
            while i < n {
                if chars[i] == '}' {
                    found_placeholder = true;
                    break;
                }
                i += 1;
            }
        }
        let value: i64 = number.parse().unwrap_or(i64::MAX);
        if value > max_tab_stop {
            max_tab_stop = value;
            max_ranges = vec![(number_start, number_end)];
            replace_with_last_stop = !found_placeholder;
        } else if value == max_tab_stop {
            max_ranges.push((number_start, number_end));
        }
    }
    if replace_with_last_stop && !found_last_stop {
        // The original offsets, even once an earlier replacement shortened
        // the text, as emmet-helper does.
        for (start, end) in max_ranges {
            let end = end.min(chars.len());
            let start = start.min(end);
            chars.splice(start..end, ['0']);
        }
    }
    chars.into_iter().collect()
}

/// `escapeNonTabStopDollar`: `/([^\\])(\$)([^\{])/g` → `$1\\$2$3`.
fn escape_non_tab_stop_dollar(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i] != '\\' && chars[i + 1] == '$' && chars[i + 2] != '{' {
            result.push(chars[i]);
            result.push('\\');
            result.push('$');
            result.push(chars[i + 2]);
            i += 3;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// `replaceTabStopsWithCursors`: `${1}` → `|` (after any character but a
/// backslash), then `${1:text}` → `text`.
fn replace_tab_stops_with_cursors(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut cursors = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\'
            && let Some(end) = tab_stop_end(&chars, i + 1)
        {
            cursors.push(chars[i]);
            cursors.push('|');
            i = end;
            continue;
        }
        cursors.push(chars[i]);
        i += 1;
    }
    let chars: Vec<char> = cursors.chars().collect();
    let mut result = String::with_capacity(cursors.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some((placeholder, end)) = placeholder_at(&chars, i) {
            result.extend(placeholder);
            i = end;
            continue;
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// The end of `${digits}` starting at `start`.
fn tab_stop_end(chars: &[char], start: usize) -> Option<usize> {
    if chars.get(start) != Some(&'$') || chars.get(start + 1) != Some(&'{') {
        return None;
    }
    let digits = chars[start + 2..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count();
    let close = start + 2 + digits;
    (digits > 0 && chars.get(close) == Some(&'}')).then_some(close + 1)
}

/// `${digits:text}` starting at `start`: the text and the end.
fn placeholder_at(chars: &[char], start: usize) -> Option<(&[char], usize)> {
    if chars.get(start) != Some(&'$') || chars.get(start + 1) != Some(&'{') {
        return None;
    }
    let digits = chars[start + 2..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count();
    let colon = start + 2 + digits;
    if digits == 0 || chars.get(colon) != Some(&':') {
        return None;
    }
    let text = chars[colon + 1..].iter().take_while(|c| **c != '}').count();
    let close = colon + 1 + text;
    (text > 0 && chars.get(close) == Some(&'}')).then(|| (&chars[colon + 1..close], close + 1))
}

/// What emmet and emmet-helper ship as data, extracted into `data.json`.
pub(crate) struct Data {
    /// The `html` snippet registry: an abbreviation for each name.
    snippets: HashMap<String, String>,
    variables: HashMap<String, String>,
    /// emmet-helper's commonly used tags: the HTML tags, then `lorem`.
    tags: Vec<String>,
    lorem: HashMap<String, Vocabulary>,
}

#[derive(Deserialize)]
pub(crate) struct Vocabulary {
    common: Vec<String>,
    words: Vec<String>,
}

#[derive(Deserialize)]
struct RawData {
    snippets: Vec<(String, String)>,
    variables: Vec<(String, String)>,
    tags: Vec<String>,
    lorem: HashMap<String, Vocabulary>,
}

/// emmet's default `inlineElements`.
const INLINE_ELEMENTS: [&str; 39] = [
    "a", "abbr", "acronym", "applet", "b", "basefont", "bdo", "big", "br", "button", "cite",
    "code", "del", "dfn", "em", "font", "i", "iframe", "img", "input", "ins", "kbd", "label",
    "map", "object", "q", "s", "samp", "select", "small", "span", "strike", "strong", "sub", "sup",
    "textarea", "tt", "u", "var",
];

/// emmet's default `output.booleanAttributes`.
const BOOLEAN_ATTRIBUTES: [&str; 21] = [
    "contenteditable",
    "seamless",
    "async",
    "autofocus",
    "autoplay",
    "checked",
    "controls",
    "defer",
    "disabled",
    "formnovalidate",
    "hidden",
    "ismap",
    "loop",
    "multiple",
    "muted",
    "novalidate",
    "readonly",
    "required",
    "reversed",
    "selected",
    "typemustmatch",
];

impl Data {
    fn is_inline(&self, name: &str) -> bool {
        INLINE_ELEMENTS.contains(&name.to_lowercase().as_str())
    }

    fn is_boolean_attribute(&self, name: &str) -> bool {
        BOOLEAN_ATTRIBUTES.contains(&name.to_lowercase().as_str())
    }
}

fn data() -> &'static Data {
    static DATA: OnceLock<Data> = OnceLock::new();
    DATA.get_or_init(|| {
        let raw: RawData =
            serde_json::from_str(include_str!("data.json")).expect("bundled Emmet data");
        Data {
            snippets: raw.snippets.into_iter().collect(),
            variables: raw.variables.into_iter().collect(),
            tags: raw.tags,
            lorem: raw.lorem,
        }
    })
}

#[cfg(test)]
mod tests;
