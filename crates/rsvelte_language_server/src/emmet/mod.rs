//! Emmet abbreviations: a port of the official server's `doComplete(document,
//! position, syntax, {})` from `@vscode/emmet-helper` 2.8.4 and of `emmet`
//! 2.4.11, which it expands with — for `html` in markup, and for `css`,
//! `scss`, `less` and `sass` in a stylesheet.
//!
//! `data.json` holds what those packages ship as data — the `html` snippet
//! registry, the variables, emmet-helper's commonly used tags and the lorem
//! ipsum vocabularies; the stylesheet snippet registry, keywords and unitless
//! properties, and emmet-helper's CSS property names — extracted from the
//! installed versions.
//!
//! Both are MIT-licensed (emmet © Sergey Chikuyonok, emmet-helper ©
//! Microsoft); their notices are in `NOTICE.md`. Regenerate `data.json` and
//! the tests' oracles with `node scripts/dev/generate-emmet-data.mjs`.

mod convert;
mod css;
mod extract;
mod html;
mod parser;
mod stylesheet;
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
    /// A stylesheet expansion is labelled with its text and filtered by
    /// the abbreviation.
    pub filter_text: Option<String>,
    pub start: usize,
    pub end: usize,
}

/// emmet-helper's stylesheet syntaxes, as the official CSS plugin names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stylesheet {
    Css,
    Scss,
    Less,
    Sass,
}

impl Stylesheet {
    /// The name emmet-helper and `emmet.excludeLanguages` know it by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Css => "css",
            Self::Scss => "scss",
            Self::Less => "less",
            Self::Sass => "sass",
        }
    }

    /// emmet-helper's `stylesheet.after`.
    const fn options(self) -> css::Options {
        css::Options {
            after: match self {
                Self::Sass => "",
                Self::Css | Self::Scss | Self::Less => ";",
            },
        }
    }
}

/// The stylesheet suggestions for the abbreviation ending at UTF-16 column
/// `cursor` of `line`, the cursor's whole line: `None` where emmet-helper
/// answers `undefined`, and an empty list where it answers one (an
/// abbreviation longer than four characters that starts a CSS property name).
#[must_use]
pub fn stylesheet_suggestions(
    line: &str,
    cursor: usize,
    syntax: Stylesheet,
) -> Option<Vec<Suggestion>> {
    let data = data();
    let line: Vec<u16> = line.encode_utf16().collect();
    let till_cursor = &line[..cursor.min(line.len())];
    let (pos, filter) = filters(till_cursor, cursor);
    let filter_len = filter.as_ref().map_or(0, |filter| filter.len() + 1);
    let extracted = extract::extract(&line, pos, extract::Kind::Stylesheet)?;
    let abbreviation = String::from_utf16_lossy(&extracted.abbreviation);
    let start = extracted.location;
    let end = start + extracted.abbreviation.len() + filter_len;
    let options = syntax.options();

    let mut expanded = None;
    if is_stylesheet_abbreviation_valid(&abbreviation, data) {
        let mut text = css::expand(&abbreviation, options, data).unwrap_or_default();
        // microsoft/vscode#120245.
        if "!important".starts_with(&abbreviation) {
            text = "!important".to_string();
        }
        if !text.is_empty() && !is_stylesheet_noise(&abbreviation, &text, options) {
            expanded = Some(text);
        }
    }
    // An abbreviation longer than emmet's own that starts a property name
    // is that property being typed.
    if abbreviation.encode_utf16().count() > 4
        && data
            .stylesheet
            .properties
            .iter()
            .any(|property| property.starts_with(&abbreviation))
    {
        return Some(Vec::new());
    }
    let expanded = expanded?;
    let label = remove_tab_stops(&expanded);
    // `width` → `widows` or `font-family` → `font: family` would be noise
    // (microsoft/vscode#28933): without a digit, the label must hold the
    // abbreviation's characters in order.
    if !abbreviation.chars().any(|c| c.is_ascii_digit()) && !holds_in_order(&abbreviation, &label)?
    {
        return None;
    }
    Some(vec![Suggestion {
        label,
        new_text: escape_non_tab_stop_dollar(&add_final_tab_stop(&expanded)),
        documentation: replace_tab_stops_with_cursors(&expanded),
        filter_text: Some(abbreviation),
        start,
        end,
    }])
}

/// `isAbbreviationValid` for a stylesheet: a hex colour, or a start that can
/// begin a property, a keyword or an at-rule — and no tag before a `#`.
fn is_stylesheet_abbreviation_valid(abbreviation: &str, data: &Data) -> bool {
    if let Some(hash) = abbreviation.find('#') {
        if let Some(hex) = abbreviation.strip_prefix('#') {
            return (1..=6).contains(&hex.len())
                && hex.chars().all(|c| c.is_ascii_hexdigit() || c == ',');
        }
        if data.tags.iter().any(|tag| *tag == abbreviation[..hash]) {
            return false;
        }
    }
    let rest = abbreviation.strip_prefix('-').unwrap_or(abbreviation);
    rest.starts_with(|c: char| c.is_ascii_alphabetic() || ",!@#".contains(c))
}

/// `isExpandedTextNoise` for a stylesheet: an unknown name expands to an
/// empty property (`abc` → `abc: ;`, `abc:d` → `abc: d;`).
fn is_stylesheet_noise(abbreviation: &str, expanded: &str, options: css::Options) -> bool {
    let after = options.after;
    // The abbreviation without a `:` it ends with.
    let units: Vec<u16> = abbreviation.encode_utf16().collect();
    let from = units.len().saturating_sub(css::BETWEEN.len());
    let end = units[from..]
        .iter()
        .position(|&unit| unit == u16::from(b':'))
        .map_or(units.len(), |index| from + index);
    let name = String::from_utf16_lossy(&units[..end]);
    let unspaced = |text: &str| {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    };
    expanded == format!("{name}{}${{0}}{after}", css::BETWEEN)
        || unspaced(expanded) == unspaced(abbreviation) + after
}

/// emmet-helper's `abbrRegex`: `.*` between the abbreviation's characters,
/// case-insensitive, with `$` and `+` escaped — the other characters keep
/// their regular expression meaning, so `.` is any character but a line
/// break and parentheses group. `None` where JavaScript's `RegExp` would
/// throw.
fn holds_in_order(abbreviation: &str, label: &str) -> Option<bool> {
    const ANY: &str = r"[^\n\r\u{2028}\u{2029}]";
    let pattern: Vec<String> = abbreviation
        .chars()
        .map(|c| match c {
            '.' => ANY.to_string(),
            '(' | ')' | '^' => c.to_string(),
            _ => regex::escape(&c.to_string()),
        })
        .collect();
    let any = format!("{ANY}*");
    let pattern = format!("(?i){any}{}{any}", pattern.join(&any));
    Some(regex::Regex::new(&pattern).ok()?.is_match(label))
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
    let Some(extracted) = extract::extract(&line, pos, extract::Kind::Markup) else {
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
        filter_text: None,
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
    rewrite_tab_stops(text, "|")
}

/// `removeTabStops`: as `replaceTabStopsWithCursors`, with nothing for `${1}`.
fn remove_tab_stops(text: &str) -> String {
    rewrite_tab_stops(text, "")
}

fn rewrite_tab_stops(text: &str, cursor: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut cursors = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\'
            && let Some(end) = tab_stop_end(&chars, i + 1)
        {
            cursors.push(chars[i]);
            cursors.push_str(cursor);
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
    stylesheet: StylesheetData,
}

pub(crate) struct StylesheetData {
    /// The stylesheet snippet registry, parsed and sorted by name.
    snippets: Vec<css::Snippet>,
    /// emmet's `stylesheet.keywords`.
    keywords: Vec<String>,
    /// emmet's `stylesheet.unitless`: properties a bare number keeps no unit in.
    unitless: Vec<String>,
    /// emmet-helper's CSS property names.
    properties: Vec<String>,
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
    stylesheet: RawStylesheet,
}

#[derive(Deserialize)]
struct RawStylesheet {
    snippets: Vec<(String, String)>,
    keywords: Vec<String>,
    unitless: Vec<String>,
    properties: Vec<String>,
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
            stylesheet: StylesheetData {
                snippets: css::snippets(&raw.stylesheet.snippets),
                keywords: raw.stylesheet.keywords,
                unitless: raw.stylesheet.unitless,
                properties: raw.stylesheet.properties,
            },
        }
    })
}

#[cfg(test)]
mod tests;
