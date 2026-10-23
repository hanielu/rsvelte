//! Emmet in `<style>` and in a `style` attribute, as official's CSS plugin
//! offers it (`CSSPlugin.getCompletionsInternal`).
//!
//! For `css`, `scss` and `less` the plugin registers `onCssProperty` and
//! `onCssPropertyValue` participants with vscode-css-languageservice and,
//! when one is called with a non-empty word, asks emmet-helper about the CSS
//! document — the style body, or the attribute's value as `__ {value}` — at
//! the cursor. `participant` restates where the language service calls
//! them; this server has no CSS parser to walk. For `sass`, which the
//! language service does not parse, the plugin asks emmet-helper about the
//! component itself.

use lsp_types::{
    CompletionItem, CompletionTextEdit, Documentation, InsertTextFormat, Position, Range, TextEdit,
};

use crate::emmet::{self, Stylesheet};
use crate::text::LineIndex;

/// `StyleAttributeDocument`'s wrapping of a `style` attribute's value.
const ATTRIBUTE_PREFIX: &str = "__ {";
const ATTRIBUTE_SUFFIX: &str = "}";

/// The stylesheet syntax emmet-helper is asked for: `getLanguage` of the
/// style's language, which is `css` for anything but `scss` and `less`.
#[must_use]
pub fn syntax(language: Option<&str>) -> Stylesheet {
    match language {
        Some("scss") => Stylesheet::Scss,
        Some("less") => Stylesheet::Less,
        Some("sass") => Stylesheet::Sass,
        _ => Stylesheet::Css,
    }
}

/// Emmet items for `offset` in the `<style>` body spanning `body`.
#[must_use]
pub fn style_items(
    text: &str,
    body: std::ops::Range<usize>,
    offset: usize,
    syntax: Stylesheet,
) -> Vec<CompletionItem> {
    let Some(fragment) = text.get(body.clone()) else {
        return Vec::new();
    };
    fragment_items(text, fragment, offset - body.start, syntax, |at| {
        body.start + at
    })
}

/// Emmet items for `offset` in a `style` attribute's value spanning
/// `value`, unless the value has a moustache
/// (`inStyleAttributeWithoutInterpolation`).
#[must_use]
pub fn attribute_items(
    text: &str,
    value: std::ops::Range<usize>,
    offset: usize,
) -> Vec<CompletionItem> {
    let Some(inner) = text.get(value.clone()) else {
        return Vec::new();
    };
    if inner.contains('{') || !value.contains(&offset) && offset != value.end {
        return Vec::new();
    }
    let fragment = format!("{ATTRIBUTE_PREFIX}{inner}{ATTRIBUTE_SUFFIX}");
    let at = offset - value.start + ATTRIBUTE_PREFIX.len();
    fragment_items(text, &fragment, at, Stylesheet::Css, |at| {
        // An abbreviation never starts in the prefix: it ends at its `{`.
        (value.start + at).saturating_sub(ATTRIBUTE_PREFIX.len())
    })
}

/// Emmet items for `offset` in a `sass` style: emmet-helper on the
/// component's own line, without the CSS language service.
#[must_use]
pub fn sass_items(text: &str, offset: usize) -> Option<Vec<CompletionItem>> {
    let index = LineIndex::new(text);
    let position = index.position(text, offset);
    let line = index.line_text(text, position.line as usize);
    let suggestions =
        emmet::stylesheet_suggestions(line, position.character as usize, Stylesheet::Sass)?;
    Some(
        suggestions
            .into_iter()
            .map(|suggestion| {
                let at = |character: usize| {
                    Position::new(position.line, u32::try_from(character).unwrap_or(u32::MAX))
                };
                let range = Range::new(at(suggestion.start), at(suggestion.end));
                item(suggestion, range)
            })
            .collect(),
    )
}

/// emmet-helper's suggestions for the CSS document `fragment` at its byte
/// offset `at`, where the language service calls a participant; `original`
/// maps a byte offset in `fragment` to one in `text`.
fn fragment_items(
    text: &str,
    fragment: &str,
    at: usize,
    syntax: Stylesheet,
    original: impl Fn(usize) -> usize,
) -> Vec<CompletionItem> {
    if !participant(fragment, at, syntax) {
        return Vec::new();
    }
    // `getCurrentLine`: the fragment's line around the cursor, between `\n`s.
    let line_start = fragment[..at].rfind('\n').map_or(0, |index| index + 1);
    let line_end = fragment[at..]
        .find('\n')
        .map_or(fragment.len(), |index| at + index);
    let line = &fragment[line_start..line_end];
    let cursor = fragment[line_start..at].encode_utf16().count();
    let Some(suggestions) = emmet::stylesheet_suggestions(line, cursor, syntax) else {
        return Vec::new();
    };
    let index = LineIndex::new(text);
    let position = |column: usize| {
        let within = utf16_to_byte(line, column);
        index.position(text, original(line_start + within))
    };
    suggestions
        .into_iter()
        .map(|suggestion| {
            let range = Range::new(position(suggestion.start), position(suggestion.end));
            item(suggestion, range)
        })
        .collect()
}

fn utf16_to_byte(line: &str, column: usize) -> usize {
    let mut units = 0;
    for (index, ch) in line.char_indices() {
        if units >= column {
            return index;
        }
        units += ch.len_utf16();
    }
    line.len()
}

/// emmet-helper's item: no kind, a snippet edit, the expansion as its label
/// and the abbreviation as its filter.
fn item(suggestion: emmet::Suggestion, range: Range) -> CompletionItem {
    CompletionItem {
        label: suggestion.label,
        detail: Some("Emmet Abbreviation".to_string()),
        documentation: Some(Documentation::String(suggestion.documentation)),
        filter_text: suggestion.filter_text,
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: suggestion.new_text,
        })),
        ..CompletionItem::default()
    }
}

/// What encloses the cursor in the CSS document.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// A `{ … }` block; `keyframes` when its rules are keyframe selectors.
    Block { keyframes: bool },
    /// A `(`, a function's when a name comes right before it; its content
    /// starts at `start` of the statement.
    Paren { function: bool, start: usize },
    /// SCSS `#{…}`, or LESS `@{…}` (`less`), which names a property or a
    /// value as if it were not there.
    Interpolation { less: bool },
}

/// Whether vscode-css-languageservice's `doComplete` calls `onCssProperty`
/// or `onCssPropertyValue` with a non-empty word at byte offset `at` of the
/// CSS document `css`: a property's name or an identifier in its value,
/// inside a block — a rule's, an at-rule's or a nested one — or in an
/// `@supports` condition; not a keyframe selector, a variable, a LESS mixin,
/// an at-rule's parameters, a function's arguments, a hex colour or a
/// string. A comment is as if it were not there.
fn participant(css: &str, at: usize, syntax: Stylesheet) -> bool {
    // `getCurrentWord`.
    let word_start = css[..at]
        .rfind(|c: char| " \t\n\r\":{[()]},*>+".contains(c))
        .map_or(0, |index| index + 1);
    let word = &css[word_start..at];
    if word.is_empty() {
        return false;
    }
    let line_comments = matches!(syntax, Stylesheet::Scss | Stylesheet::Less);
    let bytes = css.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    // The statement the cursor is in, from its start, comments blanked out.
    let mut statement = String::new();
    let mut i = 0;
    while i < at {
        let byte = bytes[i];
        if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
            match css[i + 2..].find("*/") {
                Some(end) if i + 2 + end + 2 <= at => {
                    i += 2 + end + 2;
                    statement.push(' ');
                    continue;
                }
                // The cursor is in the comment.
                _ => break,
            }
        }
        if line_comments && byte == b'/' && bytes.get(i + 1) == Some(&b'/') {
            match css[i..].find('\n') {
                Some(end) if i + end < at => {
                    i += end;
                    continue;
                }
                _ => break,
            }
        }
        if byte == b'"' || byte == b'\'' {
            match string_end(bytes, i) {
                Some(end) if end <= at => {
                    statement.push_str(&css[i..end]);
                    i = end;
                    continue;
                }
                // The cursor is in the string.
                _ => return false,
            }
        }
        match byte {
            b'{' => {
                let before = i.checked_sub(1).map(|before| bytes[before]);
                match (syntax, before) {
                    (Stylesheet::Scss, Some(b'#')) | (Stylesheet::Less, Some(b'@')) => {
                        stack.push(Frame::Interpolation {
                            less: syntax == Stylesheet::Less,
                        });
                        statement.push('{');
                    }
                    _ => {
                        let prelude = statement.trim_start();
                        let name = prelude.split(char::is_whitespace).next().unwrap_or("");
                        stack.push(Frame::Block {
                            keyframes: name.starts_with('@') && name.ends_with("keyframes"),
                        });
                        statement.clear();
                    }
                }
            }
            b'}' => {
                while let Some(frame) = stack.pop() {
                    if let Frame::Interpolation { .. } = frame {
                        statement.push('}');
                        break;
                    }
                    if let Frame::Block { .. } = frame {
                        statement.clear();
                        break;
                    }
                }
            }
            b'(' => {
                let function = statement
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_'));
                statement.push('(');
                stack.push(Frame::Paren {
                    function,
                    start: statement.len(),
                });
            }
            b')' => {
                if let Some(Frame::Paren { .. }) = stack.last() {
                    stack.pop();
                }
                statement.push(')');
            }
            b';' if matches!(stack.last(), Some(Frame::Block { .. })) => statement.clear(),
            _ => {
                let ch = css[i..].chars().next().expect("a character");
                statement.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
        i += 1;
    }
    // A LESS interpolation names what the statement around it does.
    while let Some(Frame::Interpolation { less: true }) = stack.last() {
        stack.pop();
    }
    let parens = stack
        .iter()
        .rev()
        .take_while(|frame| matches!(frame, Frame::Paren { .. }))
        .count();
    match stack.iter().rev().nth(parens).copied() {
        Some(Frame::Block { keyframes }) => {
            if keyframes {
                return false;
            }
            // In parentheses after a property's name, or a selector's, the
            // property is still what is completed (`lg(r`, `:is(m`); in a
            // value they are a function's arguments.
            if let Some(&Frame::Paren { start, .. }) = parens
                .checked_sub(1)
                .and_then(|outermost| stack.iter().rev().nth(outermost))
                && has_value(&statement[..start])
            {
                return false;
            }
            block_statement(statement.trim_start(), word, syntax)
        }
        None => match stack.last() {
            Some(&Frame::Paren { start, .. }) if supports_condition(&stack, &statement) => {
                // `(property: value)`, a declaration.
                !statement[start..].contains(':') || value_word(word)
            }
            _ => false,
        },
        // In a SCSS interpolation.
        Some(_) => false,
    }
}

/// Whether the cursor's parentheses are an `@supports` condition: the
/// statement is one, at the top level (nested, it is not parsed as one), and
/// none of the parentheses is a function's.
fn supports_condition(stack: &[Frame], statement: &str) -> bool {
    statement.trim_start().starts_with("@supports")
        && stack.iter().all(|frame| {
            matches!(
                frame,
                Frame::Paren {
                    function: false,
                    ..
                }
            )
        })
}

/// A statement in a block, up to the cursor: a property's name or value.
fn block_statement(statement: &str, word: &str, syntax: Stylesheet) -> bool {
    if statement.starts_with('$') {
        // A SCSS variable's declaration.
        return false;
    }
    if let Some(at_rule) = statement.strip_prefix('@') {
        // An at-keyword being typed is a property; its parameters are not,
        // nor is a LESS variable's declaration.
        return !at_rule.contains(|c: char| c.is_whitespace() || c == ':');
    }
    if has_value(statement) {
        return value_word(word);
    }
    // A LESS mixin call: `.mixin`, `#namespace.mixin`.
    !(syntax == Stylesheet::Less && statement == word && word.starts_with(['.', '#']))
}

/// Whether a statement has a name and a `:` after it: a declaration's value
/// follows.
fn has_value(statement: &str) -> bool {
    statement
        .trim_start()
        .find(':')
        .is_some_and(|colon| colon > 0)
}

/// In a value, an identifier or a number: not a hex colour, nor a SCSS or
/// LESS variable.
fn value_word(word: &str) -> bool {
    if let Some(hex) = word.strip_prefix('#') {
        return !(matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    !word.starts_with(['$', '@'])
}

/// The end of the string starting with the quote at `start`: past its
/// closing quote, or at the line break or end an unterminated one stops at.
fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            byte if byte == quote => return Some(i + 1),
            b'\n' | b'\r' | b'\x0c' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use crate::completions::{EmmetGates, completions_with_strict_mode};

    const ORACLE: &str = include_str!("../tests/data/emmet-stylesheet-oracle.json");

    #[derive(Deserialize)]
    struct Oracle {
        documents: Vec<Document>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Document {
        text: String,
        position: lsp_types::Position,
        is_incomplete: Option<bool>,
        items: Vec<Item>,
    }

    #[derive(Deserialize, Debug, PartialEq)]
    #[serde(rename_all = "camelCase")]
    struct Item {
        label: String,
        new_text: String,
        documentation: String,
        filter_text: Option<String>,
        range: lsp_types::Range,
    }

    /// The Emmet labels completion offers at `‸`, asked by `triggered_by`.
    fn emmet_labels(marked: &str, triggered_by: Option<&str>) -> Vec<String> {
        let offset = marked.find('‸').expect("a cursor");
        let text = marked.replace('‸', "");
        completions_with_strict_mode(&text, offset, false, true, EmmetGates::all(), triggered_by)
            .map(|list| {
                list.items
                    .into_iter()
                    .filter(|item| item.detail.as_deref() == Some("Emmet Abbreviation"))
                    .map(|item| item.label)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Upstream's CSS plugin answers a completion another trigger character
    /// asked for with nothing; its HTML plugin offers markup Emmet for one it
    /// does not declare, in a style's content outside braces, before the CSS
    /// plugin's items.
    #[test]
    fn a_trigger_character_chooses_the_plugins_that_answer() {
        assert_eq!(
            emmet_labels("<style>a {\n\tc#‸\n}</style>", Some("#")),
            Vec::<String>::new()
        );
        assert_eq!(
            emmet_labels("<style>a {\n\tc#f‸\n}</style>", None),
            ["color: #fff;"]
        );
        assert_eq!(
            emmet_labels("<style>a {\n\tm10-‸\n}</style>", Some("-")),
            ["margin: 10px;"]
        );
        assert_eq!(
            emmet_labels("<style lang=\"sass\">\na\n\tm10-‸\n</style>", Some("-")),
            ["m10-", "margin: 10px"]
        );
        assert_eq!(
            emmet_labels("<style lang=\"sass\">\na\n\tc#‸\n</style>", Some("#")),
            ["c#"]
        );
        assert_eq!(
            emmet_labels("<div style=\"c#‸\"></div>", Some("#")),
            Vec::<String>::new()
        );
    }

    /// The official CSS plugin's `getCompletions` on components, from
    /// `node scripts/dev/generate-emmet-data.mjs`: its Emmet items, and its
    /// list incomplete exactly when it holds one (or, for `sass`, whenever
    /// emmet-helper answered).
    #[test]
    fn every_style_completes_emmet_as_the_css_plugin_does() {
        let oracle: Oracle = serde_json::from_str(ORACLE).expect("parse the stylesheet oracle");
        let mut failures = Vec::new();
        for case in &oracle.documents {
            let index = crate::text::LineIndex::new(&case.text);
            let offset = index.offset(&case.text, case.position);
            // The HTML plugin's Emmet is not the CSS plugin's.
            let gates = EmmetGates {
                html: false,
                ..EmmetGates::all()
            };
            let list = completions_with_strict_mode(&case.text, offset, false, true, gates, None);
            let got: Vec<Item> = list
                .iter()
                .flat_map(|list| &list.items)
                .filter(|item| item.detail.as_deref() == Some("Emmet Abbreviation"))
                .map(|item| {
                    let Some(lsp_types::CompletionTextEdit::Edit(edit)) = &item.text_edit else {
                        panic!("an Emmet item edits");
                    };
                    let Some(lsp_types::Documentation::String(documentation)) = &item.documentation
                    else {
                        panic!("an Emmet item is documented");
                    };
                    Item {
                        label: item.label.clone(),
                        new_text: edit.new_text.clone(),
                        documentation: documentation.clone(),
                        filter_text: item.filter_text.clone(),
                        range: edit.range,
                    }
                })
                .collect();
            let incomplete = list.as_ref().is_some_and(|list| list.is_incomplete);
            if got != case.items || incomplete != case.is_incomplete.unwrap_or(false) {
                failures.push(format!(
                    "{:?} at {:?}:\n  want {:?} {:?}\n  got  {incomplete} {:?}",
                    case.text,
                    case.position,
                    case.is_incomplete,
                    case.items
                        .iter()
                        .map(|item| (&item.label, item.range))
                        .collect::<Vec<_>>(),
                    got.iter()
                        .map(|item| (&item.label, item.range))
                        .collect::<Vec<_>>(),
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} components differ:\n{}",
            failures.len(),
            oracle.documents.len(),
            failures.join("\n")
        );
    }
}
