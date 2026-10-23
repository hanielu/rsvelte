//! Native HTML/Svelte tag pairing for editor features.
//!
//! This deliberately works on incomplete documents: linked editing is queried
//! while a closing tag is being typed, when the component parser often cannot
//! produce an AST yet.

use std::ops::Range as ByteRange;

use lsp_types::{DocumentHighlight, DocumentHighlightKind, LinkedEditingRanges, Range};

use crate::context::skip_braces;
use crate::text::LineIndex;

const HTML_COMMENT_OPEN: &str = "<!--";

#[derive(Debug, Clone, PartialEq, Eq)]
struct TagPair {
    open: ByteRange<usize>,
    close: ByteRange<usize>,
}

/// The word pattern the official server sends; the ranges' own contents must match it.
const WORD_PATTERN: &str =
    r#"(-?\d*\.\d\w*)|([^\`\~\!\@\#\^\&\*\(\)\=\+\[\{\]\}\\\|\;\:\'\"\,\<\>\/\s]+)"#;

/// Linked ranges for the opening and closing name under `offset`.
#[must_use]
pub fn linked_ranges(text: &str, offset: usize) -> Option<LinkedEditingRanges> {
    let pair = pair_at(text, offset)?;
    let index = LineIndex::new(text);
    Some(LinkedEditingRanges {
        ranges: vec![
            range(&index, text, pair.open),
            range(&index, text, pair.close),
        ],
        // Byte-identical to what the official server sends: VS Code's default word pattern.
        word_pattern: Some(WORD_PATTERN.to_string()),
    })
}

/// Text highlights for the matched opening and closing tag names.
#[must_use]
pub fn highlights(text: &str, offset: usize) -> Vec<DocumentHighlight> {
    let Some(pair) = pair_at(text, offset) else {
        return Vec::new();
    };
    let index = LineIndex::new(text);
    [pair.open, pair.close]
        .into_iter()
        .map(|span| DocumentHighlight {
            range: range(&index, text, span),
            // `htmlHighlighting.js:18,21` pushes both tag names as `Read`.
            kind: Some(DocumentHighlightKind::READ),
        })
        .collect()
}

/// Completion text for the `html/tag` compatibility request.
#[must_use]
pub fn close_tag(text: &str, offset: usize) -> Option<String> {
    let before = text.get(..offset)?;
    let end = before.strip_suffix('>')?;
    let open = end.rfind('<')?;
    let name = end[open + 1..]
        .split(|character: char| character.is_ascii_whitespace() || character == '/')
        .next()?;
    (!name.is_empty() && !name.starts_with(['/', '!', '?']) && !is_void(name))
        .then(|| format!("</{name}>"))
}

/// The start tag `offset` sits inside that is still open there, as
/// `collectCloseTagSuggestions` (`htmlCompletion.js:106-117`) walks up from
/// `findNodeBefore(offset)`: an ancestor counts while it has no end tag, or its
/// end tag begins after the cursor. The name comes from the document rather
/// than from the tag data, because a component and `svelte:head` are ancestors
/// the data provider does not list.
#[must_use]
pub(crate) fn enclosing_open_tag(text: &str, offset: usize) -> Option<(String, usize)> {
    scan_open_tags(text)
        .into_iter()
        .filter(|open| open.lt < offset && open.close_lt.is_none_or(|close| close > offset))
        .max_by_key(|open| open.lt)
        .map(|open| (open.name, open.lt))
}

struct OpenTag {
    name: String,
    lt: usize,
    close_lt: Option<usize>,
}

/// The start and end tags of a document, read as upstream's `parseHtml`
/// (`lib/documents/parseHtml.ts`) reads them: a `{…}` in text is an
/// expression, where `<` opens no tag; a start tag ends at the `>` outside its
/// `{…}` values and quotes ([`start_tag_end`]); and a `<script>` or `<style>`
/// body is raw text, where a TypeScript generic (`$state<User>()`) is no tag.
fn scan_open_tags(text: &str) -> Vec<OpenTag> {
    let mut all: Vec<OpenTag> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let bytes = text.as_bytes();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let Some(relative) = text[cursor..].find(['<', '{']) else {
            break;
        };
        let start = cursor + relative;
        if bytes[start] == b'{' {
            cursor = skip_braces(text, start);
            continue;
        }
        if text[start..].starts_with(HTML_COMMENT_OPEN) {
            cursor = text[start + 4..]
                .find("-->")
                .map_or(bytes.len(), |end| start + 7 + end);
            continue;
        }
        let mut name_start = start + 1;
        let closing = bytes.get(name_start) == Some(&b'/');
        if closing {
            name_start += 1;
            while bytes.get(name_start).is_some_and(u8::is_ascii_whitespace) {
                name_start += 1;
            }
        }
        let mut name_end = name_start;
        while bytes
            .get(name_end)
            .is_some_and(|byte| is_tag_name_byte(*byte))
        {
            name_end += 1;
        }
        if name_end == name_start {
            cursor = start + 1;
            continue;
        }
        let name = &text[name_start..name_end];
        if closing {
            let Some(end) = text[name_end..].find('>').map(|end| name_end + end) else {
                break;
            };
            if let Some(position) = stack.iter().rposition(|&index| all[index].name == name) {
                let index = stack.remove(position);
                all[index].close_lt = Some(start);
            }
            cursor = end + 1;
            continue;
        }
        let Some((end, self_closing)) = start_tag_end(text, name_end) else {
            break;
        };
        if !is_void(name) && !self_closing {
            all.push(OpenTag {
                name: name.to_string(),
                lt: start,
                close_lt: None,
            });
            stack.push(all.len() - 1);
        }
        cursor = match name {
            "script" | "style" if !self_closing => text[end..]
                .find(&format!("</{name}"))
                .map_or(bytes.len(), |body_end| end + body_end),
            _ => end,
        };
    }
    all
}

/// Where a start tag whose name ends at `from` ends, just past its `>`, and
/// whether it closes itself (`/>`). A `{…}` (an expression value, a spread, an
/// attachment), a quoted value and a `//` or `/* */` comment are read whole,
/// so the `>` of `onclick={() => go()}` or `title="a > b"` ends nothing.
/// `None` while the tag is unterminated.
fn start_tag_end(text: &str, from: usize) -> Option<(usize, bool)> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1)) {
            (b'>', _) => return Some((i + 1, false)),
            (b'/', Some(b'>')) => return Some((i + 2, true)),
            (b'/', Some(b'/')) => {
                i = text[i..].find('\n').map_or(bytes.len(), |end| i + end);
                continue;
            }
            (b'/', Some(b'*')) => {
                i = text[i + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| i + 2 + end + 2);
                continue;
            }
            (b'{', _) => {
                i = skip_braces(text, i);
                continue;
            }
            (quote @ (b'"' | b'\''), _) if bytes[from..i].trim_ascii_end().ends_with(b"=") => {
                // A quoted value, whose `{…}` may hold quotes of its own.
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i = if bytes[i] == b'{' {
                        skip_braces(text, i)
                    } else {
                        i + 1
                    };
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn range(index: &LineIndex, text: &str, span: ByteRange<usize>) -> Range {
    Range::new(
        index.position(text, span.start),
        index.position(text, span.end),
    )
}

fn pair_at(text: &str, offset: usize) -> Option<TagPair> {
    let mut stack: Vec<(String, ByteRange<usize>)> = Vec::new();
    let bytes = text.as_bytes();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let Some(relative) = text[cursor..].find('<') else {
            break;
        };
        let start = cursor + relative;
        if text[start..].starts_with("<!--") {
            cursor = text[start + 4..]
                .find("-->")
                .map_or(bytes.len(), |end| start + 7 + end);
            continue;
        }
        let mut name_start = start + 1;
        let closing = bytes.get(name_start) == Some(&b'/');
        if closing {
            name_start += 1;
        }
        while bytes.get(name_start).is_some_and(u8::is_ascii_whitespace) {
            name_start += 1;
        }
        let mut name_end = name_start;
        while bytes
            .get(name_end)
            .is_some_and(|byte| is_tag_name_byte(*byte))
        {
            name_end += 1;
        }
        if name_end == name_start {
            cursor = start + 1;
            continue;
        }
        let Some(end) = text[name_end..].find('>').map(|end| name_end + end) else {
            break;
        };
        let span = name_start..name_end;
        let name = &text[span.clone()];
        if closing {
            if let Some(index) = stack.iter().rposition(|(open, _)| open == name) {
                let (_, open) = stack.remove(index);
                if open.contains(&offset) || span.contains(&offset) || offset == span.end {
                    return Some(TagPair { open, close: span });
                }
            }
        } else if !is_void(name) && !text[name_end..end].trim_end().ends_with('/') {
            stack.push((name.to_string(), span));
        }
        cursor = end + 1;
    }
    None
}

const fn is_tag_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'$')
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_pattern_is_byte_identical_to_the_official_server() {
        let ranges = linked_ranges("<Foo.Bar><span /></Foo.Bar>", 3).unwrap();
        assert_eq!(ranges.ranges.len(), 2);
        assert_eq!(ranges.ranges[0].end.character, 8);
        assert_eq!(ranges.word_pattern.as_deref(), Some(WORD_PATTERN));
    }

    // The contract: a returned pattern must accept the contents of the returned
    // ranges. This outlives whatever string the official server settles on.
    #[test]
    fn word_pattern_accepts_the_full_contents_of_the_ranges_it_is_sent_with() {
        for (text, offset, name) in [
            ("<div></div>", 2, "div"),
            ("<Foo></Foo>", 2, "Foo"),
            ("<Foo.Bar><span /></Foo.Bar>", 3, "Foo.Bar"),
        ] {
            let ranges = linked_ranges(text, offset).unwrap();
            let pattern = ranges.word_pattern.as_deref().unwrap();
            let regex = fancy_regex::Regex::new(pattern).unwrap();
            let matched = regex.find(name).unwrap().expect("pattern matched nothing");
            assert_eq!(
                (matched.start(), matched.end()),
                (0, name.len()),
                "{pattern} does not fully accept {name}"
            );
        }
    }

    #[test]
    fn ignores_void_and_comment_tags() {
        assert!(linked_ranges("<!-- <div></div> --><br>", 7).is_none());
        assert!(linked_ranges("<img><div></div>", 2).is_none());
    }

    #[test]
    fn highlights_both_names() {
        assert_eq!(highlights("<section>x</section>", 3).len(), 2);
    }

    #[test]
    fn completes_a_just_opened_tag() {
        assert_eq!(close_tag("<section>", 9).as_deref(), Some("</section>"));
        assert_eq!(close_tag("<img>", 5), None);
    }
}
